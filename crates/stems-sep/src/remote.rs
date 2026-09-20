//! Remote separation worker: run the model on another machine (a desktop
//! GPU) and stream windows to it over TCP.
//!
//! Protocol (all integers little-endian):
//!
//! * server → client on connect: `u32 len` + JSON [`Hello`]
//! * client → server per window: `u8 denoise`, `u32 n`, `n × f32` samples
//! * server → client per window: `u32 n`, `n × f32` samples; `n == u32::MAX`
//!   is followed by `u32 len` + UTF-8 error text
//!
//! One window is ~2 MB each way, so a LAN handles a few windows per second.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::params::MdxParams;

/// First message from a worker: what it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub model: String,
    pub provider: String,
    pub params: MdxParams,
}

const MAGIC: &[u8; 4] = b"STM1";
const MAX_SAMPLES: u32 = 4 * 1024 * 1024;

fn write_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn write_samples(w: &mut impl Write, samples: &[f32]) -> io::Result<()> {
    write_u32(w, samples.len() as u32)?;
    let mut buf = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    w.write_all(&buf)
}

fn read_samples(r: &mut impl Read, n: u32) -> io::Result<Vec<f32>> {
    if n > MAX_SAMPLES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("window too large: {n} samples"),
        ));
    }
    let mut buf = vec![0u8; n as usize * 4];
    r.read_exact(&mut buf)?;
    Ok(buf
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

/// Client side: one TCP connection, one window in flight at a time.
/// Use one per streaming worker for parallelism.
pub struct RemoteRunner {
    stream: TcpStream,
    hello: Hello,
}

impl RemoteRunner {
    pub fn connect(addr: impl ToSocketAddrs + std::fmt::Debug) -> Result<Self> {
        let addr = addr
            .to_socket_addrs()
            .context("resolve worker address")?
            .next()
            .ok_or_else(|| anyhow!("worker address resolves to nothing"))?;
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
            .with_context(|| format!("connect to worker {addr}"))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let mut magic = [0u8; 4];
        stream.read_exact(&mut magic).context("worker handshake")?;
        anyhow::ensure!(&magic == MAGIC, "not a stems worker at {addr}");
        let len = read_u32(&mut stream)?;
        let mut json = vec![0u8; len as usize];
        stream.read_exact(&mut json)?;
        let hello: Hello = serde_json::from_slice(&json).context("worker hello")?;
        Ok(Self { stream, hello })
    }

    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// Send one window, block for the instrumental.
    pub fn run(&mut self, window: &[f32], denoise: bool) -> Result<Vec<f32>> {
        self.stream.write_all(&[denoise as u8])?;
        write_samples(&mut self.stream, window)?;
        self.stream.flush()?;
        let n = read_u32(&mut self.stream)?;
        if n == u32::MAX {
            let len = read_u32(&mut self.stream)?;
            let mut msg = vec![0u8; len as usize];
            self.stream.read_exact(&mut msg)?;
            return Err(anyhow!("worker: {}", String::from_utf8_lossy(&msg)));
        }
        let out = read_samples(&mut self.stream, n)?;
        anyhow::ensure!(
            out.len() == window.len(),
            "worker returned {} samples for {}",
            out.len(),
            window.len()
        );
        Ok(out)
    }
}

/// Server side: serve connections from a pool of window runners.
pub mod serve {
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Instant;

    use anyhow::{Context, Result};

    use super::*;

    /// Runs one window (optionally denoised) → instrumental.
    pub type Runner = Box<dyn FnMut(&[f32], bool) -> Result<Vec<f32>> + Send>;

    /// Serve a pool of model sessions.
    #[cfg(feature = "onnx")]
    pub fn run(
        listener: TcpListener,
        model_name: &str,
        models: Vec<crate::mdx::MdxModel>,
    ) -> Result<()> {
        anyhow::ensure!(!models.is_empty(), "no model sessions");
        let hello = Hello {
            model: model_name.to_string(),
            provider: models[0].provider().to_string(),
            params: models[0].params().clone(),
        };
        let runners = models
            .into_iter()
            .map(|mut m| -> Runner { Box::new(move |w, d| m.instrumental_for_window_opt(w, d)) })
            .collect();
        run_with(listener, hello, runners)
    }

    /// Blocking accept loop; each connection gets a thread and borrows a
    /// runner from the pool per window, so `runners.len()` windows run
    /// concurrently across all clients.
    pub fn run_with(listener: TcpListener, hello: Hello, runners: Vec<Runner>) -> Result<()> {
        anyhow::ensure!(!runners.is_empty(), "no runners");
        let hello_json = serde_json::to_vec(&hello)?;
        let (pool_tx, rx) = mpsc::channel::<Runner>();
        for r in runners {
            pool_tx.send(r).expect("pool");
        }
        let pool = Arc::new(Mutex::new(rx));
        for conn in listener.incoming() {
            let stream = match conn {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("accept: {e}");
                    continue;
                }
            };
            let peer = stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            tracing::info!(peer, "worker client connected");
            let pool = Arc::clone(&pool);
            let pool_tx = pool_tx.clone();
            let hello_json = hello_json.clone();
            std::thread::Builder::new()
                .name(format!("stems-worker-{peer}"))
                .spawn(move || {
                    if let Err(e) = serve_conn(stream, &hello_json, pool, pool_tx) {
                        tracing::info!(peer, "worker client gone: {e:#}");
                    }
                })
                .context("spawn connection thread")?;
        }
        Ok(())
    }

    fn serve_conn(
        mut stream: TcpStream,
        hello_json: &[u8],
        pool: Arc<Mutex<mpsc::Receiver<Runner>>>,
        pool_tx: mpsc::Sender<Runner>,
    ) -> Result<()> {
        stream.set_nodelay(true)?;
        stream.write_all(MAGIC)?;
        write_u32(&mut stream, hello_json.len() as u32)?;
        stream.write_all(hello_json)?;
        stream.flush()?;
        loop {
            let mut head = [0u8; 5];
            if stream.read_exact(&mut head).is_err() {
                return Ok(()); // client closed
            }
            // A browser pointed at this port: say so instead of failing.
            if head.starts_with(b"GET ") || head.starts_with(b"HEAD") {
                let body = "stems separation worker: this is a raw TCP port for stemsd \
                            (backend = \"remote\"), not a web page.\n";
                let _ = write!(
                    stream,
                    "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                return Ok(());
            }
            let denoise = head[0] != 0;
            let n = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
            let window = read_samples(&mut stream, n)?;
            // borrow a runner; the receiver lock is only held while waiting
            let mut runner = {
                let rx = pool.lock().map_err(|_| anyhow!("pool poisoned"))?;
                rx.recv().map_err(|_| anyhow!("pool closed"))?
            };
            let started = Instant::now();
            let result = runner(&window, denoise);
            let _ = pool_tx.send(runner);
            match result {
                Ok(inst) => {
                    tracing::debug!(
                        ms = started.elapsed().as_millis() as u64,
                        denoise,
                        "worker window"
                    );
                    write_samples(&mut stream, &inst)?;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::warn!("worker inference failed: {msg}");
                    write_u32(&mut stream, u32::MAX)?;
                    write_u32(&mut stream, msg.len() as u32)?;
                    stream.write_all(msg.as_bytes())?;
                }
            }
            stream.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn roundtrip_samples() {
        let v: Vec<f32> = (0..1000).map(|i| i as f32 * 0.001 - 0.5).collect();
        let mut buf = Vec::new();
        write_samples(&mut buf, &v).unwrap();
        let mut r = &buf[..];
        let n = read_u32(&mut r).unwrap();
        assert_eq!(read_samples(&mut r, n).unwrap(), v);
    }

    #[test]
    fn client_server_roundtrip_and_errors() {
        use crate::params::{MdxParams, PrimaryStem};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hello = Hello {
            model: "identity".into(),
            provider: "test".into(),
            params: MdxParams {
                n_fft: 512,
                hop: 16,
                dim_f: 256,
                dim_t: 257,
                primary_stem: PrimaryStem::Instrumental,
                compensate: 1.0,
            },
        };
        let runners: Vec<serve::Runner> = (0..2)
            .map(|_| -> serve::Runner {
                Box::new(|w: &[f32], denoise: bool| {
                    if denoise {
                        anyhow::bail!("no denoise here")
                    }
                    Ok(w.iter().map(|x| x * 0.5).collect())
                })
            })
            .collect();
        std::thread::spawn(move || serve::run_with(listener, hello, runners));

        let mut a = RemoteRunner::connect(addr).unwrap();
        let mut b = RemoteRunner::connect(addr).unwrap();
        assert_eq!(a.hello().model, "identity");
        assert_eq!(a.hello().params.chunk_size(), 4096);
        let w: Vec<f32> = (0..4096 * 2).map(|i| (i % 100) as f32 / 100.0).collect();
        let out = a.run(&w, false).unwrap();
        assert_eq!(out.len(), w.len());
        assert!((out[150] - w[150] * 0.5).abs() < 1e-6);
        let out_b = b.run(&w, false).unwrap();
        assert_eq!(out_b, out);
        // an error on one window does not break the connection
        let err = a.run(&w, true).unwrap_err().to_string();
        assert!(err.contains("no denoise here"), "{err}");
        assert_eq!(a.run(&w, false).unwrap(), out);
    }

    #[test]
    fn rejects_non_worker() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let _ = s.write_all(b"HTTP/1.1 400\r\n\r\n");
        });
        let err = match RemoteRunner::connect(addr) {
            Ok(_) => panic!("connected to a non-worker"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("not a stems worker"), "{err}");
    }
}
