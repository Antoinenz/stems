//! Build the configured [`Separator`] backend.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use stems_core::Separator;
use stems_sep::CentreCancel;

use crate::config::KaraokeConfig;

pub async fn build(cfg: &KaraokeConfig, models_dir: PathBuf) -> Result<Box<dyn Separator>> {
    if cfg.backend == "dsp" {
        return Ok(Box::new(CentreCancel::default()));
    }
    let opts = stream_opts(cfg);
    let r = build_runners(cfg, models_dir).await?;
    let sep = stems_sep::MdxSeparator::with_runners(r.params, r.provider, r.runners, opts);
    tracing::info!(
        backend = cfg.backend,
        latency_ms = sep.latency_frames() as u64 * 1000 / stems_core::SAMPLE_RATE as u64,
        "streaming separator ready"
    );
    Ok(Box::new(sep))
}

/// `mode = "prebuffer"`: whole-track jobs on the same runners; the audio
/// path reads finished stems with zero latency.
pub async fn build_offline(
    cfg: &KaraokeConfig,
    models_dir: PathBuf,
    stems_dir: PathBuf,
) -> Result<(stems_sep::OfflineSeparator, Arc<AtomicU32>)> {
    anyhow::ensure!(
        cfg.backend != "dsp",
        "mode = \"prebuffer\" needs an ML backend (mdx or remote)"
    );
    let opts = stream_opts(cfg);
    let r = build_runners(cfg, models_dir).await?;
    let limit = cfg.gpu_limit.clamp(0.1, 1.0);
    // One governor shared by every worker: windows are serialised and
    // paced, so two workers cannot keep the GPU saturated by taking turns
    // sleeping. At a limit of 1.0 it steps aside. The limit can be
    // changed while running (settings API).
    let gov = Arc::new(Governor::new(limit));
    let limit_handle = gov.limit_handle();
    let runners: Vec<stems_sep::WindowRunner> = r
        .runners
        .into_iter()
        .map(|mut run| -> stems_sep::WindowRunner {
            let gov = gov.clone();
            Box::new(move |w: &[f32]| {
                let t = gov.acquire();
                let out = run(w);
                gov.release(t);
                out
            })
        })
        .collect();
    let disk = if cfg.stem_cache_mb > 0 {
        match stems_sep::disk::StemDisk::new(&stems_dir, cfg.stem_cache_mb * 1024 * 1024) {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!("stem cache disabled: {e:#}");
                None
            }
        }
    } else {
        None
    };
    tracing::info!(
        backend = cfg.backend,
        provider = r.provider,
        gpu_limit = limit,
        stem_cache = disk.as_ref().map(|d| d.dir().display().to_string()),
        "offline separator ready"
    );
    Ok((
        stems_sep::OfflineSeparator::with_disk(r.params, r.provider, runners, opts, disk),
        limit_handle,
    ))
}

/// Utilisation cap for the offline job. Windows run one at a time and
/// each is followed by an idle gap of `busy × (1/limit − 1)`, so the
/// accelerator sees a steady `limit` duty cycle rather than bursts.
/// Running windows concurrently would not help here: a saturated GPU
/// just time-slices them, and the concurrency is what starves the
/// compositor and everything else.
struct Governor {
    /// f32 bits; ≥ 1.0 disables pacing and serialisation.
    limit: Arc<AtomicU32>,
    inner: Mutex<GovState>,
    cv: Condvar,
}

struct GovState {
    active: bool,
    /// Earliest instant the next window may start.
    next_start: Instant,
}

impl Governor {
    fn new(limit: f32) -> Self {
        Self {
            limit: Arc::new(AtomicU32::new(limit.to_bits())),
            inner: Mutex::new(GovState {
                active: false,
                next_start: Instant::now(),
            }),
            cv: Condvar::new(),
        }
    }

    fn limit_handle(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.limit)
    }

    fn limit(&self) -> f32 {
        f32::from_bits(self.limit.load(Ordering::Relaxed)).clamp(0.1, 1.0)
    }

    /// Wait for the accelerator to be free and the pacing gap to pass.
    fn acquire(&self) -> Instant {
        if self.limit() >= 1.0 {
            return Instant::now();
        }
        let mut g = self.inner.lock().unwrap();
        loop {
            let now = Instant::now();
            if !g.active && now >= g.next_start {
                g.active = true;
                return now;
            }
            if g.active {
                g = self.cv.wait(g).unwrap();
            } else {
                let wait = g.next_start - now;
                g = self.cv.wait_timeout(g, wait).unwrap().0;
            }
        }
    }

    fn release(&self, started: Instant) {
        let busy = started.elapsed();
        let limit = self.limit();
        let mut g = self.inner.lock().unwrap();
        g.active = false;
        if limit < 1.0 {
            g.next_start = Instant::now() + busy.mul_f32(1.0 / limit - 1.0);
        }
        drop(g);
        self.cv.notify_all();
    }
}

fn stream_opts(cfg: &KaraokeConfig) -> stems_sep::StreamOpts {
    let ms = |v: u64| (v * stems_core::SAMPLE_RATE as u64 / 1000) as usize;
    stems_sep::StreamOpts {
        hop_frames: ms(cfg.hop_ms),
        lookahead_frames: ms(cfg.lookahead_ms),
        slack_frames: ms(cfg.slack_ms),
        // model sessions batch x/−x themselves; the remote does it server-side
        denoise: false,
        workers: cfg.workers.max(1),
        realtime: true,
    }
}

struct Runners {
    params: stems_sep::MdxParams,
    provider: &'static str,
    runners: Vec<stems_sep::WindowRunner>,
}

/// Window runners for the configured backend: in-process model sessions
/// or connections to a `stemsd worker`.
async fn build_runners(cfg: &KaraokeConfig, models_dir: PathBuf) -> Result<Runners> {
    use stems_sep::WindowRunner;
    match cfg.backend.as_str() {
        #[cfg(not(feature = "onnx"))]
        "mdx" => {
            let _ = models_dir;
            Err(anyhow!(
                "this build has no ONNX support; rebuild stemsd with `--features onnx`"
            ))
        }
        #[cfg(feature = "onnx")]
        "mdx" => {
            let models = load_models(cfg, models_dir, cfg.workers.max(1)).await?;
            let params = models[0].params().clone();
            let provider = models[0].provider();
            let denoise = cfg.denoise;
            let runners = models
                .into_iter()
                .map(|mut m| -> WindowRunner {
                    Box::new(move |w: &[f32]| m.instrumental_for_window_opt(w, denoise))
                })
                .collect();
            Ok(Runners {
                params,
                provider,
                runners,
            })
        }
        "remote" => {
            use stems_sep::remote::RemoteRunner;
            anyhow::ensure!(
                !cfg.remote.is_empty(),
                "backend = \"remote\" needs `remote = \"host:port\"` in [karaoke]"
            );
            let addr = cfg.remote.clone();
            let n = cfg.workers.max(1);
            let conns = tokio::task::spawn_blocking(move || {
                (0..n)
                    .map(|_| RemoteRunner::connect(addr.as_str()))
                    .collect::<Result<Vec<_>>>()
            })
            .await
            .context("worker connect task")??;
            let hello = conns[0].hello().clone();
            tracing::info!(
                remote = cfg.remote,
                model = hello.model,
                provider = hello.provider,
                connections = n,
                "remote separation worker"
            );
            let denoise = cfg.denoise;
            let runners = conns
                .into_iter()
                .map(|mut c| -> WindowRunner { Box::new(move |w: &[f32]| c.run(w, denoise)) })
                .collect();
            Ok(Runners {
                params: hello.params,
                provider: "remote",
                runners,
            })
        }
        other => Err(anyhow!(
            "unknown karaoke backend `{other}` (dsp, mdx, remote)"
        )),
    }
}

/// Load the word-alignment model, downloading it on first use.
#[cfg(feature = "onnx")]
pub async fn build_aligner(
    models_dir: PathBuf,
    threads: usize,
) -> Result<stems_sep::align::Aligner> {
    use stems_sep::align::{Aligner, MODEL_NAME, MODEL_URL};
    let path = models_dir.join(format!("{MODEL_NAME}.onnx"));
    if !path.is_file() {
        download(MODEL_URL, &path).await?;
    }
    tokio::task::spawn_blocking(move || Aligner::load(&path, threads))
        .await
        .context("aligner load task")?
}

/// Resolve and (if needed) download the configured model; load
/// `count` sessions.
#[cfg(feature = "onnx")]
pub async fn load_models(
    cfg: &KaraokeConfig,
    models_dir: PathBuf,
    count: usize,
) -> Result<Vec<stems_sep::MdxModel>> {
    use stems_sep::{Device, MdxModel, known_model};

    let (base_url, params) =
        known_model(&cfg.model).ok_or_else(|| anyhow!("unknown MDX model `{}`", cfg.model))?;
    let path = models_dir.join(format!("{}.onnx", cfg.model));
    if !path.is_file() {
        download(&format!("{base_url}/{}.onnx", cfg.model), &path).await?;
    }
    let device = match cfg.device.as_str() {
        "cpu" => Device::Cpu,
        "cuda" => Device::Cuda,
        "directml" => Device::DirectMl,
        "coreml" => Device::CoreMl,
        "coreml-gpu" => Device::CoreMl,
        "coreml-all" => Device::CoreMlAll,
        "coreml-ane" => Device::CoreMlAne,
        _ => Device::Auto,
    };
    let threads = cfg.threads;
    let batch = if cfg.denoise { 2 } else { 1 };
    // Loading a 60+ MB model blocks; keep it off the async runtime.
    tokio::task::spawn_blocking(move || {
        (0..count.max(1))
            .map(|_| {
                let mut m = MdxModel::load(&path, params.clone(), device, threads, batch)?;
                m.warm_up()?;
                Ok(m)
            })
            .collect::<Result<Vec<_>>>()
    })
    .await
    .context("model load task")?
}

#[cfg(feature = "onnx")]
async fn download(url: &str, path: &std::path::Path) -> Result<()> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    tracing::info!(url, to = %path.display(), "downloading model");
    let res = reqwest::get(url).await.context("model download")?;
    anyhow::ensure!(
        res.status().is_success(),
        "model download: http {}",
        res.status()
    );
    let total = res.content_length().unwrap_or(0);
    let tmp = path.with_extension("onnx.part");
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut stream = res.bytes_stream();
    let mut done: u64 = 0;
    let mut last_pct = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("model download stream")?;
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        if let Some(pct) = (done * 100).checked_div(total) {
            let pct = pct as u32;
            if pct / 10 > last_pct / 10 {
                last_pct = pct;
                tracing::info!(pct, "downloading model");
            }
        }
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, path).await?;
    tracing::info!(bytes = done, "model downloaded");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Two threads hammering the GPU stand-in never overlap and leave
    /// the configured idle fraction between windows.
    #[test]
    fn governor_serialises_and_paces() {
        let gov = Arc::new(Governor::new(0.5));
        let start = Instant::now();
        let overlap = Arc::new(Mutex::new((0usize, false)));
        let hs: Vec<_> = (0..2)
            .map(|_| {
                let gov = gov.clone();
                let overlap = overlap.clone();
                std::thread::spawn(move || {
                    for _ in 0..8 {
                        let t = gov.acquire();
                        {
                            let mut o = overlap.lock().unwrap();
                            if o.0 > 0 {
                                o.1 = true;
                            }
                            o.0 += 1;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                        overlap.lock().unwrap().0 -= 1;
                        gov.release(t);
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert!(!overlap.lock().unwrap().1, "windows overlapped");
        let elapsed = start.elapsed();
        // 16 × 20 ms of work at a 50 % duty cycle ≈ 640 ms
        assert!(elapsed >= Duration::from_millis(600), "elapsed {elapsed:?}");
        assert!(elapsed < Duration::from_secs(5), "elapsed {elapsed:?}");
    }
}
