//! Build the configured [`Separator`] backend.

use std::path::PathBuf;

#[cfg(feature = "onnx")]
use anyhow::Context;
use anyhow::{Result, anyhow};
use stems_core::Separator;
use stems_sep::CentreCancel;

use crate::config::KaraokeConfig;

pub async fn build(cfg: &KaraokeConfig, models_dir: PathBuf) -> Result<Box<dyn Separator>> {
    match cfg.backend.as_str() {
        "dsp" => Ok(Box::new(CentreCancel::default())),
        "mdx" => build_mdx(cfg, models_dir).await,
        other => Err(anyhow!("unknown karaoke backend `{other}` (dsp, mdx)")),
    }
}

#[cfg(feature = "onnx")]
async fn build_mdx(cfg: &KaraokeConfig, models_dir: PathBuf) -> Result<Box<dyn Separator>> {
    use stems_sep::mdx::{Device, MdxModel, MdxSeparator, StreamOpts, known_model};

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
        "coreml-gpu" => Device::CoreMlGpu,
        "coreml-ane" => Device::CoreMlAne,
        _ => Device::Auto,
    };
    let threads = cfg.threads;
    let ms = |v: u64| (v * stems_core::SAMPLE_RATE as u64 / 1000) as usize;
    let opts = StreamOpts {
        hop_frames: ms(cfg.hop_ms),
        lookahead_frames: ms(cfg.lookahead_ms),
        slack_frames: ms(cfg.slack_ms),
        denoise: cfg.denoise,
        workers: cfg.workers.max(1),
    };
    // Loading a 60+ MB model blocks; keep it off the async runtime.
    let n = opts.workers;
    let models = tokio::task::spawn_blocking(move || {
        (0..n)
            .map(|_| MdxModel::load(&path, params.clone(), device, threads))
            .collect::<Result<Vec<_>>>()
    })
    .await
    .context("model load task")??;
    let sep = MdxSeparator::with_models(models, opts);
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
