//! Build the configured [`Separator`] backend.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use stems_core::Separator;
use stems_sep::CentreCancel;

use crate::config::KaraokeConfig;

pub async fn build(cfg: &KaraokeConfig, models_dir: PathBuf) -> Result<Box<dyn Separator>> {
    match cfg.backend.as_str() {
        "dsp" => Ok(Box::new(CentreCancel::default())),
        "mdx" => build_mdx(cfg, models_dir).await,
        "remote" => build_remote(cfg).await,
        other => Err(anyhow!(
            "unknown karaoke backend `{other}` (dsp, mdx, remote)"
        )),
    }
}

fn stream_opts(cfg: &KaraokeConfig) -> stems_sep::StreamOpts {
    let ms = |v: u64| (v * stems_core::SAMPLE_RATE as u64 / 1000) as usize;
    stems_sep::StreamOpts {
        hop_frames: ms(cfg.hop_ms),
        lookahead_frames: ms(cfg.lookahead_ms),
        slack_frames: ms(cfg.slack_ms),
        denoise: cfg.denoise,
        workers: cfg.workers.max(1),
    }
}

/// `backend = "remote"`: windows go to a `stemsd worker` on another
/// machine; one connection per streaming worker.
async fn build_remote(cfg: &KaraokeConfig) -> Result<Box<dyn Separator>> {
    use stems_sep::remote::RemoteRunner;
    use stems_sep::{MdxSeparator, WindowRunner};

    anyhow::ensure!(
        !cfg.remote.is_empty(),
        "backend = \"remote\" needs `remote = \"host:port\"` in [karaoke]"
    );
    let opts = stream_opts(cfg);
    let addr = cfg.remote.clone();
    let n = opts.workers;
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
    let denoise = opts.denoise;
    let runners: Vec<WindowRunner> = conns
        .into_iter()
        .map(|mut c| -> WindowRunner { Box::new(move |w: &[f32]| c.run(w, denoise)) })
        .collect();
    // the remote already averaged x/−x, so the local wrapper must not
    let opts = stems_sep::StreamOpts {
        denoise: false,
        ..opts
    };
    let sep = MdxSeparator::with_runners(hello.params, "remote", runners, opts);
    tracing::info!(
        latency_ms = sep.latency_frames() as u64 * 1000 / stems_core::SAMPLE_RATE as u64,
        "remote separator ready"
    );
    Ok(Box::new(sep))
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
            .map(|_| MdxModel::load(&path, params.clone(), device, threads, batch))
            .collect::<Result<Vec<_>>>()
    })
    .await
    .context("model load task")?
}

#[cfg(feature = "onnx")]
async fn build_mdx(cfg: &KaraokeConfig, models_dir: PathBuf) -> Result<Box<dyn Separator>> {
    let opts = stream_opts(cfg);
    let models = load_models(cfg, models_dir, opts.workers).await?;
    let sep = stems_sep::MdxSeparator::with_models(models, opts);
    tracing::info!(
        latency_ms = sep.latency_frames() as u64 * 1000 / stems_core::SAMPLE_RATE as u64,
        "mdx separator ready"
    );
    Ok(Box::new(sep))
}

#[cfg(not(feature = "onnx"))]
async fn build_mdx(_cfg: &KaraokeConfig, _models_dir: PathBuf) -> Result<Box<dyn Separator>> {
    Err(anyhow!(
        "this build has no ONNX support; rebuild stemsd with `--features onnx`"
    ))
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
