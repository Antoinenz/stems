//! MDX-Net (UVR family) separation through ONNX Runtime: one ONNX session
//! plus the model's STFT parameters; runs one window of audio and returns
//! the primary stem. See [`crate::stream`] for the streaming wrapper.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use ort::session::Session;
use ort::value::Tensor;
use realfft::num_complex::Complex32;
use serde::{Deserialize, Serialize};

pub use crate::params::{MdxParams, PrimaryStem, known_model};
use crate::stft::Stft;
pub use crate::stream::{MdxSeparator, StreamOpts, WindowRunner};

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
    /// Core ML on CPU + GPU (the known-fast path).
    CoreMl,
    /// Core ML choosing between CPU, GPU and Neural Engine.
    CoreMlAll,
    /// Core ML restricted to CPU + Neural Engine.
    CoreMlAne,
}

pub struct MdxModel {
    session: Session,
    params: MdxParams,
    stft: Stft,
    input_name: String,
    provider: &'static str,
    /// Run `[x, −x]` as one batch of 2 (session loaded with `batch = 2`).
    batch_denoise: bool,
}

impl MdxModel {
    /// `batch` fixes the model's symbolic batch dimension (1, or 2 when
    /// the denoise pair is batched). Static shapes let Core ML compile for
    /// the Neural Engine and let every provider plan memory up front.
    pub fn load(
        path: &Path,
        params: MdxParams,
        device: Device,
        threads: usize,
        batch: usize,
    ) -> Result<Self> {
        let mut builder = Session::builder()
            .context("ort session builder")?
            .with_dimension_override("batch_size", batch.max(1) as i64)
            .map_err(|e| anyhow!("dimension override: {e}"))?;
        let mut eps: Vec<ort::ep::ExecutionProviderDispatch> = Vec::new();
        #[allow(unused_mut)]
        let mut provider = "cpu";
        match device {
            Device::Auto
            | Device::Cuda
            | Device::DirectMl
            | Device::CoreMl
            | Device::CoreMlAll
            | Device::CoreMlAne => {
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
                if matches!(
                    device,
                    Device::Auto | Device::CoreMl | Device::CoreMlAll | Device::CoreMlAne
                ) {
                    use ort::ep::coreml::{ComputeUnits, ModelFormat};
                    let units = match device {
                        Device::CoreMlAll => ComputeUnits::All,
                        Device::CoreMlAne => ComputeUnits::CPUAndNeuralEngine,
                        _ => ComputeUnits::CPUAndGPU,
                    };
                    // MLProgram covers more operators than the legacy
                    // NeuralNetwork format, so less of the graph falls back
                    // to the CPU EP. STEMS_COREML_PROFILE=1 prints which
                    // unit each op landed on.
                    let ep = ort::ep::CoreML::default()
                        .with_model_format(ModelFormat::MLProgram)
                        .with_compute_units(units)
                        .with_profile_compute_plan(
                            std::env::var_os("STEMS_COREML_PROFILE").is_some(),
                        )
                        .build();
                    eps.push(ep);
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
        let input = session
            .inputs()
            .first()
            .ok_or_else(|| anyhow!("model has no inputs"))?;
        let input_name = input.name().to_string();
        tracing::info!(
            model = %path.display(),
            provider,
            input = %input.dtype(),
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
            batch_denoise: batch >= 2,
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
        self.instrumental_for_window_opt(chunk, false)
    }

    /// As [`instrumental_for_window`](Self::instrumental_for_window); with
    /// `denoise` the model runs on `x` and `−x` and the results are
    /// averaged, which cancels some of its additive artefacts.
    pub fn instrumental_for_window_opt(
        &mut self,
        chunk: &[f32],
        denoise: bool,
    ) -> Result<Vec<f32>> {
        if !denoise {
            return self.run_window(chunk, false);
        }
        if self.batch_denoise {
            match self.run_window(chunk, true) {
                Ok(inst) => return Ok(inst),
                Err(e) => {
                    tracing::warn!("batched denoise unsupported, running twice: {e:#}");
                    self.batch_denoise = false;
                }
            }
        }
        let neg: Vec<f32> = chunk.iter().map(|x| -x).collect();
        let a = self.run_window(chunk, false)?;
        let b = self.run_window(&neg, false)?;
        Ok(a.iter().zip(&b).map(|(x, y)| 0.5 * (x - y)).collect())
    }

    /// One inference. With `denoise` the batch is `[x, −x]` (the STFT is
    /// linear, so the second spectrogram is just negated) and the two
    /// outputs are averaged before a single iSTFT.
    fn run_window(&mut self, chunk: &[f32], denoise: bool) -> Result<Vec<f32>> {
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

        let batch = if denoise { 2 } else { 1 };
        let per = 4 * dim_f * dim_t;
        if denoise {
            let neg: Vec<f32> = input.iter().map(|v| -v).collect();
            input.extend(neg);
        }
        let tensor = Tensor::from_array((vec![batch as i64, 4, dim_f as i64, dim_t as i64], input))
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
            out.len() == batch * per,
            "unexpected output size {} for batch {batch}",
            out.len()
        );
        tracing::debug!(
            ms = started.elapsed().as_millis() as u64,
            batch,
            "mdx window inference"
        );
        let averaged: Vec<f32>;
        let out: &[f32] = if denoise {
            averaged = out[..per]
                .iter()
                .zip(&out[per..])
                .map(|(a, b)| 0.5 * (a - b))
                .collect();
            &averaged
        } else {
            out
        };

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
