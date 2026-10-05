//! Remote separation worker: run the model on another machine (a desktop
//! GPU) and stream windows to it over TCP.
//!
//! Protocol (all integers little-endian):
//!
//! * server → client on connect: `STM1`, `u32 len` + JSON [`Hello`]
//! * client → server per window: `u8 denoise` (0/1), `u32 n`, `n × f32`
//!   samples
//! * server → client per window: `u32 n`, `n × f32` samples; `n == u32::MAX`
//!   is followed by `u32 len` + UTF-8 error text
//! * word alignment, only if the hello says `align`: `u8 2`, `u32 len` +
//!   JSON list of words, `u32 n`, `n × f32` mono 16 kHz vocals → `u32 len`
//!   + JSON `null` or list of [`WordSpan`] (or the error form above)
//!
//! One window is ~2 MB each way, so a LAN handles a few windows per second.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::params::MdxParams;
use crate::words::WordSpan;

/// Request tag for word alignment (0/1 are windows with denoise off/on).
const REQ_ALIGN: u8 = 2;

/// First message from a worker: what it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub model: String,
    pub provider: String,
    pub params: MdxParams,
    /// The worker can also place lyric words (has the speech model).
    #[serde(default)]
    pub align: bool,
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

    /// Read the error text that follows a `u32::MAX` length.
    fn read_error(&mut self) -> Result<anyhow::Error> {
        let len = read_u32(&mut self.stream)?;
        let mut msg = vec![0u8; len as usize];
        self.stream.read_exact(&mut msg)?;
        Ok(anyhow!("worker: {}", String::from_utf8_lossy(&msg)))
    }

    /// Place `words` in mono 16 kHz vocals on the worker.
    pub fn align(&mut self, audio16k: &[f32], words: &[String]) -> Result<Option<Vec<WordSpan>>> {
        anyhow::ensure!(self.hello.align, "worker: has no word alignment model");
        let json = serde_json::to_vec(words)?;
        self.stream.write_all(&[REQ_ALIGN])?;
        write_u32(&mut self.stream, json.len() as u32)?;
        self.stream.write_all(&json)?;
        write_samples(&mut self.stream, audio16k)?;
        self.stream.flush()?;
        let len = read_u32(&mut self.stream)?;
        if len == u32::MAX {
            return Err(self.read_error()?);
        }
        anyhow::ensure!(len < 16 * 1024 * 1024, "alignment reply too large");
        let mut buf = vec![0u8; len as usize];
        self.stream.read_exact(&mut buf)?;
        serde_json::from_slice(&buf).context("alignment reply")
    }

    /// Send one window, block for the instrumental.
    pub fn run(&mut self, window: &[f32], denoise: bool) -> Result<Vec<f32>> {
        self.stream.write_all(&[denoise as u8])?;
        write_samples(&mut self.stream, window)?;
        self.stream.flush()?;
        let n = read_u32(&mut self.stream)?;
        if n == u32::MAX {
            return Err(self.read_error()?);
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

/// A worker connection that survives the worker: it connects on first
/// use, reconnects after a failure (trying every address in turn, with
/// backoff while none answers) and retries a window once on a fresh
/// connection before giving up on it. Workers running a different model
/// than `expect` are refused.
pub struct ResilientRemote {
    addrs: Vec<String>,
    /// Address to try first; rotates on failure and spreads connections.
    next: usize,
    expect: MdxParams,
    denoise: bool,
    conn: Option<RemoteRunner>,
    retry_at: Instant,
    backoff: Duration,
}

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

impl ResilientRemote {
    /// `first` picks which address this connection prefers, so N
    /// connections spread over the workers.
    pub fn new(addrs: Vec<String>, first: usize, expect: MdxParams, denoise: bool) -> Self {
        let next = if addrs.is_empty() {
            0
        } else {
            first % addrs.len()
        };
        Self {
            addrs,
            next,
            expect,
            denoise,
            conn: None,
            retry_at: Instant::now(),
            backoff: BACKOFF_MIN,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.conn.is_some()
    }

    /// Try each address once, starting with the preferred one.
    fn connect(&mut self) -> Result<()> {
        let mut last = anyhow!("no worker addresses configured");
        for k in 0..self.addrs.len() {
            let i = (self.next + k) % self.addrs.len();
            let addr = self.addrs[i].clone();
            match RemoteRunner::connect(addr.as_str()) {
                Ok(c) if c.hello().params != self.expect => {
                    last = anyhow!(
                        "worker {addr} runs model `{}` with different parameters; \
                         set the same [karaoke] model on both machines",
                        c.hello().model
                    );
                }
                Ok(c) => {
                    tracing::info!(
                        worker = addr,
                        model = c.hello().model,
                        provider = c.hello().provider,
                        "connected to separation worker"
                    );
                    self.next = i;
                    self.conn = Some(c);
                    self.backoff = BACKOFF_MIN;
                    return Ok(());
                }
                Err(e) => last = e.context(format!("worker {addr}")),
            }
        }
        self.retry_at = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        Err(last)
    }

    /// Separate one window remotely.
    pub fn run(&mut self, window: &[f32]) -> Result<Vec<f32>> {
        let denoise = self.denoise;
        self.call(|c| c.run(window, denoise))
    }

    /// Place lyric words on the worker (see [`RemoteRunner::align`]).
    pub fn align(&mut self, audio16k: &[f32], words: &[String]) -> Result<Option<Vec<WordSpan>>> {
        self.call(|c| c.align(audio16k, words))
    }

    /// One request with reconnect-and-retry-once on a lost connection.
    fn call<T>(&mut self, mut f: impl FnMut(&mut RemoteRunner) -> Result<T>) -> Result<T> {
        for attempt in 0..2 {
            if self.conn.is_none() {
                if Instant::now() < self.retry_at {
                    return Err(anyhow!("no separation worker reachable (retrying)"));
                }
                self.connect()?;
            }
            let conn = self.conn.as_mut().expect("connected above");
            match f(conn) {
                Ok(out) => return Ok(out),
                // the worker answered with an error: the connection is fine
                Err(e) if e.to_string().starts_with("worker: ") => return Err(e),
                Err(e) => {
                    tracing::warn!(
                        worker = self.addrs.get(self.next).map(String::as_str).unwrap_or("?"),
                        attempt,
                        "lost separation worker: {e:#}"
                    );
                    self.conn = None;
                    // prefer another worker for the retry
                    if !self.addrs.is_empty() {
                        self.next = (self.next + 1) % self.addrs.len();
                    }
                }
            }
        }
        Err(anyhow!("separation worker connection lost"))
    }
}

/// Split `"a:7374, b:7374"` into addresses.
pub fn parse_addrs(s: &str) -> Vec<String> {
    s.split([',', ' '])
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(String::from)
        .collect()
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

    /// Places words in mono 16 kHz vocals.
    pub type AlignRunner =
        Box<dyn FnMut(&[f32], &[String]) -> Result<Option<Vec<WordSpan>>> + Send>;
    type SharedAligner = Arc<Mutex<Option<AlignRunner>>>;

    /// Serve a pool of model sessions.
    #[cfg(feature = "onnx")]
    pub fn run(
        listener: TcpListener,
        model_name: &str,
        models: Vec<crate::mdx::MdxModel>,
        aligner: Option<crate::align::Aligner>,
    ) -> Result<()> {
        anyhow::ensure!(!models.is_empty(), "no model sessions");
        let hello = Hello {
            model: model_name.to_string(),
            provider: models[0].provider().to_string(),
            params: models[0].params().clone(),
            align: false,
        };
        let runners = models
            .into_iter()
            .map(|mut m| -> Runner { Box::new(move |w, d| m.instrumental_for_window_opt(w, d)) })
            .collect();
        let aligner = aligner
            .map(|mut a| -> AlignRunner { Box::new(move |audio, words| a.align(audio, words)) });
        run_with(listener, hello, runners, aligner)
    }

    /// Blocking accept loop; each connection gets a thread and borrows a
    /// runner from the pool per window, so `runners.len()` windows run
    /// concurrently across all clients.
    pub fn run_with(
        listener: TcpListener,
        mut hello: Hello,
        runners: Vec<Runner>,
        aligner: Option<AlignRunner>,
    ) -> Result<()> {
        anyhow::ensure!(!runners.is_empty(), "no runners");
        hello.align = aligner.is_some();
        let aligner: SharedAligner = Arc::new(Mutex::new(aligner));
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
            let aligner = Arc::clone(&aligner);
            std::thread::Builder::new()
                .name(format!("stems-worker-{peer}"))
                .spawn(move || {
                    if let Err(e) = serve_conn(stream, &hello_json, pool, pool_tx, aligner) {
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
        aligner: SharedAligner,
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
            let n = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
            if head[0] == REQ_ALIGN {
                serve_align(&mut stream, n, &aligner)?;
                continue;
            }
            let denoise = head[0] != 0;
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

/// One alignment request (tag already read; `json_len` is the next field).
fn serve_align(
    stream: &mut TcpStream,
    json_len: u32,
    aligner: &std::sync::Arc<std::sync::Mutex<Option<serve::AlignRunner>>>,
) -> Result<()> {
    anyhow::ensure!(json_len < 1024 * 1024, "word list too large");
    let mut json = vec![0u8; json_len as usize];
    stream.read_exact(&mut json)?;
    let words: Vec<String> = serde_json::from_slice(&json).context("word list")?;
    let n = read_u32(stream)?;
    let audio = read_samples(stream, n)?;
    let started = Instant::now();
    let result = match aligner.lock() {
        Ok(mut g) => match g.as_mut() {
            Some(a) => a(&audio, &words),
            None => Err(anyhow!("no word alignment model on this worker")),
        },
        Err(_) => Err(anyhow!("aligner poisoned")),
    };
    match result.and_then(|spans| Ok(serde_json::to_vec(&spans)?)) {
        Ok(reply) => {
            tracing::debug!(
                words = words.len(),
                ms = started.elapsed().as_millis() as u64,
                "worker alignment"
            );
            write_u32(stream, reply.len() as u32)?;
            stream.write_all(&reply)?;
        }
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::warn!("worker alignment failed: {msg}");
            write_u32(stream, u32::MAX)?;
            write_u32(stream, msg.len() as u32)?;
            stream.write_all(msg.as_bytes())?;
        }
    }
    stream.flush()?;
    Ok(())
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
            align: false,
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
        std::thread::spawn(move || serve::run_with(listener, hello, runners, None));

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

    fn identity_worker(scale: f32, params: MdxParams) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hello = Hello {
            model: "identity".into(),
            provider: "test".into(),
            params,
            align: false,
        };
        let runners: Vec<serve::Runner> = vec![Box::new(move |w: &[f32], _d: bool| {
            Ok(w.iter().map(|x| x * scale).collect())
        })];
        std::thread::spawn(move || serve::run_with(listener, hello, runners, None));
        addr
    }

    fn test_params() -> MdxParams {
        use crate::params::PrimaryStem;
        MdxParams {
            n_fft: 512,
            hop: 16,
            dim_f: 256,
            dim_t: 257,
            primary_stem: PrimaryStem::Instrumental,
            compensate: 1.0,
        }
    }

    #[test]
    fn resilient_remote_skips_dead_workers_and_wrong_models() {
        // a port nothing listens on
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        let mut other = test_params();
        other.n_fft = 1024;
        let wrong = identity_worker(0.5, other).to_string();
        let good = identity_worker(0.5, test_params()).to_string();
        let mut r = ResilientRemote::new(vec![dead, wrong, good], 0, test_params(), false);
        let w = vec![0.4f32; 4096 * 2];
        let out = r.run(&w).unwrap();
        assert!(r.is_connected());
        assert!((out[0] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn resilient_remote_backs_off_when_nothing_answers() {
        let dead = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        let mut r = ResilientRemote::new(vec![dead], 0, test_params(), false);
        let w = vec![0.0f32; 4096 * 2];
        assert!(r.run(&w).is_err());
        // inside the backoff window it fails fast without dialling
        let t = Instant::now();
        let err = r.run(&w).unwrap_err().to_string();
        assert!(err.contains("retrying"), "{err}");
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn resilient_remote_retries_a_window_after_the_worker_dies() {
        // serves one window, then drops the connection mid-request
        let flaky = TcpListener::bind("127.0.0.1:0").unwrap();
        let flaky_addr = flaky.local_addr().unwrap().to_string();
        let hello = serde_json::to_vec(&Hello {
            model: "identity".into(),
            provider: "test".into(),
            params: test_params(),
            align: false,
        })
        .unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = flaky.accept().unwrap();
            s.write_all(MAGIC).unwrap();
            write_u32(&mut s, hello.len() as u32).unwrap();
            s.write_all(&hello).unwrap();
            let mut head = [0u8; 5];
            s.read_exact(&mut head).unwrap();
            let n = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
            let w = read_samples(&mut s, n).unwrap();
            write_samples(&mut s, &w).unwrap();
            let _ = s.read_exact(&mut head);
            // drop: the next window is lost with the connection
        });
        let good = identity_worker(0.5, test_params()).to_string();
        let mut r = ResilientRemote::new(vec![flaky_addr, good], 0, test_params(), false);
        let w = vec![0.4f32; 4096 * 2];
        assert!(
            (r.run(&w).unwrap()[0] - 0.4).abs() < 1e-6,
            "served by the flaky one"
        );
        let out = r.run(&w).expect("retried on the other worker");
        assert!((out[0] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn alignment_over_the_wire_when_the_worker_has_a_model() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let hello = Hello {
            model: "identity".into(),
            provider: "test".into(),
            params: test_params(),
            align: false, // set by run_with from the aligner below
        };
        let runners: Vec<serve::Runner> = vec![Box::new(|w: &[f32], _| Ok(w.to_vec()))];
        let aligner: serve::AlignRunner = Box::new(|audio: &[f32], words: &[String]| {
            if words.is_empty() {
                anyhow::bail!("nothing to align");
            }
            // one 100 ms span per word, scored by the audio length
            Ok(Some(
                (0..words.len() as u32)
                    .map(|i| WordSpan {
                        start_ms: i * 100,
                        end_ms: i * 100 + 90,
                        score: -(audio.len() as f32) / 16_000.0,
                    })
                    .collect(),
            ))
        });
        std::thread::spawn(move || serve::run_with(listener, hello, runners, Some(aligner)));

        let mut r = ResilientRemote::new(vec![addr], 0, test_params(), false);
        let words: Vec<String> = ["hello", "world"].map(String::from).to_vec();
        let spans = r.align(&vec![0.0; 16_000], &words).unwrap().unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].start_ms, 100);
        assert_eq!(spans[0].score, -1.0);
        // a worker-side error leaves the connection usable
        assert!(
            r.align(&[0.0; 10], &[])
                .unwrap_err()
                .to_string()
                .contains("nothing to align")
        );
        assert_eq!(r.run(&vec![0.5; 4096 * 2]).unwrap()[0], 0.5);
    }

    #[test]
    fn workers_without_a_speech_model_say_so() {
        let addr = identity_worker(1.0, test_params()).to_string();
        let mut r = ResilientRemote::new(vec![addr], 0, test_params(), false);
        let err = r.align(&[0.0; 16], &["x".into()]).unwrap_err().to_string();
        assert!(err.contains("no word alignment model"), "{err}");
    }

    #[test]
    fn parses_address_lists() {
        assert_eq!(
            parse_addrs(" 192.168.1.50:7374, surface.local:7374 "),
            vec!["192.168.1.50:7374", "surface.local:7374"]
        );
        assert!(parse_addrs("").is_empty());
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
