//! Build the configured [`Separator`] backend.

use std::path::PathBuf;

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
) -> Result<stems_sep::OfflineSeparator> {
    anyhow::ensure!(
        cfg.backend != "dsp",
        "mode = \"prebuffer\" needs an ML backend (mdx or remote)"
    );
    let opts = stream_opts(cfg);
    let r = build_runners(cfg, models_dir).await?;
    tracing::info!(
        backend = cfg.backend,
        provider = r.provider,
        "offline separator ready"
    );
    Ok(stems_sep::OfflineSeparator::new(
        r.params, r.provider, r.runners, opts,
    ))
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
