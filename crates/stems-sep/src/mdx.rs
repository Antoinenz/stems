//! MDX-Net (UVR family) separation through ONNX Runtime.
//!
//! * [`MdxModel`]: one ONNX session + the model's STFT parameters; runs one
//!   window of audio and returns the primary stem.
//! * [`MdxSeparator`]: streaming [`Separator`] that feeds windows to a
//!   worker thread and hands back instrumental audio delayed by a fixed
//!   number of frames, so a slow model never stalls the audio thread.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use ort::session::Session;
use ort::value::Tensor;
use realfft::num_complex::Complex32;
use serde::{Deserialize, Serialize};
use stems_core::Separator;

use crate::stft::Stft;

/// Which stem the model's output is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryStem {
    Instrumental,
    Vocals,
}

/// STFT / tensor geometry of an MDX-Net model (from UVR's model registry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MdxParams {
    pub n_fft: usize,
    pub hop: usize,
    pub dim_f: usize,
    pub dim_t: usize,
    pub primary_stem: PrimaryStem,
    pub compensate: f32,
}

impl MdxParams {
    /// Samples per model window.
    pub fn chunk_size(&self) -> usize {
        self.hop * (self.dim_t - 1)
    }

    /// Samples trimmed at each end of a window (edge artefacts).
    pub fn trim(&self) -> usize {
        self.n_fft / 2
    }

    /// Valid output samples produced per window.
    pub fn gen_size(&self) -> usize {
        self.chunk_size() - 2 * self.trim()
    }
}

/// Where to run the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    /// Try GPU providers compiled in, then CPU.
    #[default]
    Auto,
    Cpu,
    Cuda,
    DirectMl,
    CoreMl,
}

pub struct MdxModel {
    session: Session,
    params: MdxParams,
    stft: Stft,
    input_name: String,
    provider: &'static str,
}

impl MdxModel {
    pub fn load(path: &Path, params: MdxParams, device: Device, threads: usize) -> Result<Self> {
        let mut builder = Session::builder().context("ort session builder")?;
        let mut eps: Vec<ort::ep::ExecutionProviderDispatch> = Vec::new();
        #[allow(unused_mut)]
        let mut provider = "cpu";
        match device {
            Device::Auto | Device::Cuda | Device::DirectMl | Device::CoreMl => {
                #[cfg(feature = "cuda")]
                if matches!(device, Device::Auto | Device::Cuda) {
                    eps.push(ort::ep::CUDA::default().build());
                    provider = "cuda";
                }
                #[cfg(feature = "directml")]
                if matches!(device, Device::Auto | Device::DirectMl) {
                    eps.push(ort::ep::DirectML::default().build());
                    if provider == "cpu" {
                        provider = "directml";
                    }
                }
                #[cfg(feature = "coreml")]
                if matches!(device, Device::Auto | Device::CoreMl) {
                    eps.push(ort::ep::CoreML::default().build());
                    if provider == "cpu" {
                        provider = "coreml";
                    }
                }
            }
            Device::Cpu => {}
        }
        eps.push(ort::ep::CPU::default().build());
        builder = builder
            .with_execution_providers(eps)
            .map_err(|e| anyhow!("execution providers: {e}"))?;
        if threads > 0 {
            builder = builder
                .with_intra_threads(threads)
                .map_err(|e| anyhow!("threads: {e}"))?;
        }
        let session = builder
            .commit_from_file(path)
            .with_context(|| format!("load {}", path.display()))?;
        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .ok_or_else(|| anyhow!("model has no inputs"))?;
        tracing::info!(
            model = %path.display(),
            provider,
            n_fft = params.n_fft,
            dim_t = params.dim_t,
            stem = ?params.primary_stem,
            "mdx model loaded"
        );
        let stft = Stft::new(params.n_fft, params.hop);
        Ok(Self {
            session,
            params,
            stft,
            input_name,
            provider,
        })
    }

    pub fn params(&self) -> &MdxParams {
        &self.params
    }

    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// Run one window. `chunk` is interleaved stereo of exactly
    /// `chunk_size()` frames. Returns the **instrumental** for the whole
    /// window, interleaved (derived as mix − vocals for vocal models).
    pub fn instrumental_for_window(&mut self, chunk: &[f32]) -> Result<Vec<f32>> {
        let n = self.params.chunk_size();
        anyhow::ensure!(chunk.len() == n * 2, "window must be {n} stereo frames");
        let left: Vec<f32> = chunk.iter().step_by(2).copied().collect();
        let right: Vec<f32> = chunk.iter().skip(1).step_by(2).copied().collect();

        let n_bins = self.stft.n_bins();
        let dim_f = self.params.dim_f;
        let dim_t = self.params.dim_t;
        let spec_l = self.stft.forward(&left);
        let spec_r = self.stft.forward(&right);
        let frames = spec_l.len() / n_bins;
        anyhow::ensure!(frames == dim_t, "expected {dim_t} frames, got {frames}");

        // [1, 4, dim_f, dim_t]: c0 = L.re, c1 = L.im, c2 = R.re, c3 = R.im
        let mut input = vec![0.0f32; 4 * dim_f * dim_t];
        for (ch, spec) in [&spec_l, &spec_r].into_iter().enumerate() {
            for t in 0..dim_t {
                for f in 0..dim_f {
                    let v = spec[t * n_bins + f];
                    input[((ch * 2) * dim_f + f) * dim_t + t] = v.re;
                    input[((ch * 2 + 1) * dim_f + f) * dim_t + t] = v.im;
                }
            }
        }

        let tensor = Tensor::from_array((vec![1i64, 4, dim_f as i64, dim_t as i64], input))
            .context("input tensor")?;
        let started = Instant::now();
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .context("model run")?;
        let (_, out) = outputs[0]
            .try_extract_tensor::<f32>()
            .context("output tensor")?;
        anyhow::ensure!(
            out.len() == 4 * dim_f * dim_t,
            "unexpected output size {}",
            out.len()
        );
        tracing::debug!(
            ms = started.elapsed().as_millis() as u64,
            "mdx window inference"
        );

        // back to complex spectrograms, zero-padding the cropped bins
        let mut stem_l = vec![Complex32::default(); dim_t * n_bins];
        let mut stem_r = vec![Complex32::default(); dim_t * n_bins];
        for t in 0..dim_t {
            for f in 0..dim_f {
                stem_l[t * n_bins + f] =
                    Complex32::new(out[(f) * dim_t + t], out[(dim_f + f) * dim_t + t]);
                stem_r[t * n_bins + f] = Complex32::new(
                    out[(2 * dim_f + f) * dim_t + t],
                    out[(3 * dim_f + f) * dim_t + t],
                );
            }
        }
        let l = self.stft.inverse(&stem_l, n);
        let r = self.stft.inverse(&stem_r, n);
        let comp = self.params.compensate;

        let mut inst = vec![0.0f32; n * 2];
        match self.params.primary_stem {
            PrimaryStem::Instrumental => {
                for i in 0..n {
                    inst[2 * i] = l[i] * comp;
                    inst[2 * i + 1] = r[i] * comp;
                }
            }
            PrimaryStem::Vocals => {
                for i in 0..n {
                    inst[2 * i] = chunk[2 * i] - l[i] * comp;
                    inst[2 * i + 1] = chunk[2 * i + 1] - r[i] * comp;
                }
            }
        }
        Ok(inst)
    }
}

/// Streaming wrapper: windows go to a worker thread; instrumental audio
/// comes back delayed by `latency_frames()`.
pub struct MdxSeparator {
    params: MdxParams,
    latency: usize,
    /// Interleaved input awaiting windowing (includes the window overlap).
    pending: Vec<f32>,
    /// Interleaved instrumental ready to be handed out.
    ready: VecDeque<f32>,
    /// Frames of output still owed before `ready` catches up (priming).
    to_worker: mpsc::Sender<Vec<f32>>,
    from_worker: mpsc::Receiver<Vec<f32>>,
    worker: Option<JoinHandle<()>>,
    starved: bool,
    /// Samples at the head of `ready` that are priming silence, not audio.
    priming_left: usize,
    provider: &'static str,
}

impl MdxSeparator {
    /// `slack_frames` extends the latency beyond one window so inference
    /// slower than real time in bursts does not starve the output.
    pub fn new(model: MdxModel, slack_frames: usize) -> Self {
        let params = model.params().clone();
        let provider = model.provider();
        let (to_worker, jobs) = mpsc::channel::<Vec<f32>>();
        let (results, from_worker) = mpsc::channel::<Vec<f32>>();
        let mut model = model;
        let worker = thread::Builder::new()
            .name("stems-mdx".into())
            .spawn(move || {
                let trim = model.params().trim();
                let gen_size = model.params().gen_size();
                for window in jobs {
                    let out = match model.instrumental_for_window(&window) {
                        Ok(inst) => inst[trim * 2..(trim + gen_size) * 2].to_vec(),
                        Err(e) => {
                            tracing::error!("mdx inference failed: {e:#}");
                            // fall back to the (trimmed) original: better than silence
                            window[trim * 2..(trim + gen_size) * 2].to_vec()
                        }
                    };
                    if results.send(out).is_err() {
                        break;
                    }
                }
            })
            .expect("spawn mdx worker");

        let latency = params.chunk_size() + slack_frames;
        let mut s = Self {
            params,
            latency,
            pending: Vec::new(),
            ready: VecDeque::new(),
            to_worker,
            from_worker,
            worker: Some(worker),
            starved: false,
            priming_left: 0,
            provider,
        };
        s.prime();
        s
    }

    /// Start with `latency` frames of silence owed so output lags input.
    fn prime(&mut self) {
        self.pending.clear();
        self.ready.clear();
        // The first window's leading `trim` samples are discarded, so seed
        // the input with `trim` frames of silence to keep alignment exact.
        self.pending.resize(self.params.trim() * 2, 0.0);
        self.ready.resize(self.latency * 2, 0.0);
        self.priming_left = self.latency * 2;
    }

    fn dispatch_windows(&mut self) {
        let chunk = self.params.chunk_size() * 2;
        let step = self.params.gen_size() * 2;
        while self.pending.len() >= chunk {
            let window = self.pending[..chunk].to_vec();
            if self.to_worker.send(window).is_err() {
                break;
            }
            self.pending.drain(..step);
        }
    }

    fn collect_results(&mut self) {
        while let Ok(out) = self.from_worker.try_recv() {
            self.ready.extend(out);
        }
    }
}

impl Separator for MdxSeparator {
    fn name(&self) -> &'static str {
        self.provider
    }

    fn latency_frames(&self) -> usize {
        self.latency
    }

    fn process(&mut self, input: &[f32], instrumental: &mut [f32]) -> bool {
        self.pending.extend_from_slice(input);
        self.dispatch_windows();
        self.collect_results();
        if self.ready.len() < instrumental.len() {
            if !self.starved {
                tracing::warn!("mdx separator starved: inference slower than real time");
                self.starved = true;
            }
            instrumental.fill(0.0);
            return false;
        }
        if self.starved {
            tracing::info!("mdx separator caught up");
            self.starved = false;
        }
        for o in instrumental.iter_mut() {
            *o = self.ready.pop_front().unwrap_or(0.0);
        }
        if self.priming_left > 0 {
            self.priming_left = self.priming_left.saturating_sub(instrumental.len());
            return false;
        }
        true
    }

    fn reset(&mut self) {
        // Drop in-flight results; they belong to audio that was flushed.
        while self.from_worker.try_recv().is_ok() {}
        self.prime();
    }
}

impl Drop for MdxSeparator {
    fn drop(&mut self) {
        // closing the job channel ends the worker loop
        let (tx, _) = mpsc::channel();
        self.to_worker = tx;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Known models: download source and parameters (from UVR's registry).
pub fn known_model(name: &str) -> Option<(&'static str, MdxParams)> {
    let base = "https://github.com/TRvlvr/model_repo/releases/download/all_public_uvr_models";
    match name {
        "UVR-MDX-NET-Inst_HQ_3" => Some((
            base,
            MdxParams {
                n_fft: 6144,
                hop: 1024,
                dim_f: 3072,
                dim_t: 256,
                primary_stem: PrimaryStem::Instrumental,
                compensate: 1.022,
            },
        )),
        "Kim_Vocal_2" => Some((
            base,
            MdxParams {
                n_fft: 7680,
                hop: 1024,
                dim_f: 3072,
                dim_t: 256,
                primary_stem: PrimaryStem::Vocals,
                compensate: 1.009,
            },
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_geometry() {
        let (_, p) = known_model("UVR-MDX-NET-Inst_HQ_3").unwrap();
        assert_eq!(p.chunk_size(), 261_120);
        assert_eq!(p.trim(), 3072);
        assert_eq!(p.gen_size(), 261_120 - 6144);
    }
}
