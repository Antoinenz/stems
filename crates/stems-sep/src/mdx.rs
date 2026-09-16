//! MDX-Net (UVR family) separation through ONNX Runtime.
//!
//! * [`MdxModel`]: one ONNX session + the model's STFT parameters; runs one
//!   window of audio and returns the primary stem.
//! * [`MdxSeparator`]: streaming [`Separator`] that feeds windows to a
//!   worker thread and hands back instrumental audio delayed by a fixed
//!   number of frames, so a slow model never stalls the audio thread.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
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
    /// Core ML on CPU + GPU. The Neural Engine cannot compile the UVR
    /// models' unbounded batch dimension, so `All` silently lands on the
    /// CPU; GPU is the fast path until a static-shape model is shipped.
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
}

impl MdxModel {
    pub fn load(path: &Path, params: MdxParams, device: Device, threads: usize) -> Result<Self> {
        let mut builder = Session::builder().context("ort session builder")?;
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
            return self.run_window(chunk);
        }
        let neg: Vec<f32> = chunk.iter().map(|x| -x).collect();
        let a = self.run_window(chunk)?;
        let b = self.run_window(&neg)?;
        Ok(a.iter().zip(&b).map(|(x, y)| 0.5 * (x - y)).collect())
    }

    fn run_window(&mut self, chunk: &[f32]) -> Result<Vec<f32>> {
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

/// How the streaming wrapper slices audio into model windows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOpts {
    /// New input per inference, frames. Smaller = lower latency, more
    /// inferences (cost ∝ window / hop). `0` = one full window per
    /// inference (cheapest, ~6 s latency).
    pub hop_frames: usize,
    /// Future context required before a sample is finalised, frames.
    /// `0` = the model's edge trim only.
    pub lookahead_frames: usize,
    /// Extra latency budget for inference time and hop granularity.
    pub slack_frames: usize,
    /// Run each window twice (x and −x) and average: fewer artefacts,
    /// double the cost.
    pub denoise: bool,
    /// Inference threads, each with its own model session. Windows are
    /// processed concurrently so the accelerator is not idle while one
    /// window's CPU-side STFT/iSTFT and transfers run.
    pub workers: usize,
}

impl Default for StreamOpts {
    fn default() -> Self {
        Self {
            hop_frames: 0,
            lookahead_frames: 0,
            slack_frames: 44_100 * 2,
            denoise: false,
            workers: 1,
        }
    }
}

struct Job {
    generation: u64,
    seq: u64,
    start: i64,
    window: Vec<f32>,
}

struct Done {
    generation: u64,
    seq: u64,
    start: i64,
    inst: Vec<f32>,
}

/// Streaming wrapper: overlapping windows go to a worker thread, results
/// are blended with a weighted overlap-add, and finalised audio comes back
/// delayed by exactly `latency_frames()`.
pub struct MdxSeparator {
    params: MdxParams,
    hop: usize,
    lookahead: usize,
    latency: usize,
    /// Interleaved input history; `history_start` is the absolute frame
    /// index of its first frame.
    history: VecDeque<f32>,
    history_start: i64,
    input_frames: i64,
    next_window_end: i64,
    /// Weighted accumulation for frames not yet finalised, from `acc_start`.
    acc: VecDeque<[f32; 2]>,
    wacc: VecDeque<f32>,
    acc_start: i64,
    /// Finalised instrumental awaiting hand-out, with per-frame validity.
    ready: VecDeque<f32>,
    ready_valid: VecDeque<bool>,
    weights: Vec<f32>,
    to_worker: mpsc::Sender<Job>,
    from_worker: mpsc::Receiver<Done>,
    workers: Vec<JoinHandle<()>>,
    /// Bumped on reset so results from a previous track are discarded.
    generation: u64,
    /// Next sequence number to hand out / to accumulate. Results may
    /// finish out of order with several workers; they are held in
    /// `pending` until their turn so finalisation never runs ahead.
    dispatch_seq: u64,
    next_seq: u64,
    pending: BTreeMap<u64, Done>,
    /// Windows dispatched but not yet returned; bounded so a slow
    /// accelerator drops windows instead of falling ever further behind.
    in_flight: usize,
    max_in_flight: usize,
    dropped: u64,
    starved: bool,
    provider: &'static str,
}

/// Runs one window → instrumental. Boxed so the streaming logic can be
/// tested without an ONNX session.
pub type WindowRunner = Box<dyn FnMut(&[f32]) -> Result<Vec<f32>> + Send>;

impl MdxSeparator {
    pub fn new(model: MdxModel, opts: StreamOpts) -> Self {
        Self::with_models(vec![model], opts)
    }

    /// One worker thread per model session.
    pub fn with_models(models: Vec<MdxModel>, opts: StreamOpts) -> Self {
        assert!(!models.is_empty(), "at least one model session");
        let params = models[0].params().clone();
        let provider = models[0].provider();
        let denoise = opts.denoise;
        let runners = models
            .into_iter()
            .map(|mut model| -> WindowRunner {
                Box::new(move |w| model.instrumental_for_window_opt(w, denoise))
            })
            .collect();
        Self::with_runners(params, provider, runners, opts)
    }

    pub fn with_runner(
        params: MdxParams,
        provider: &'static str,
        runner: WindowRunner,
        opts: StreamOpts,
    ) -> Self {
        Self::with_runners(params, provider, vec![runner], opts)
    }

    pub fn with_runners(
        params: MdxParams,
        provider: &'static str,
        runners: Vec<WindowRunner>,
        opts: StreamOpts,
    ) -> Self {
        assert!(!runners.is_empty(), "at least one runner");
        let chunk = params.chunk_size();
        let trim = params.trim();
        let hop = if opts.hop_frames == 0 {
            params.gen_size()
        } else {
            opts.hop_frames.min(params.gen_size())
        };
        let lookahead = opts.lookahead_frames.max(trim).min(chunk - trim - 1);
        let latency = lookahead + hop + opts.slack_frames;

        // Raised-cosine weight over the valid region with a floor so that
        // single-window coverage near the finalisation edge still counts.
        let valid = chunk - 2 * trim;
        let weights: Vec<f32> = (0..chunk)
            .map(|i| {
                if i < trim || i >= chunk - trim {
                    0.0
                } else {
                    let x = (i - trim) as f32 / valid as f32;
                    0.15 + 0.85 * (std::f32::consts::PI * x).sin().powi(2)
                }
            })
            .collect();

        let (to_worker, jobs) = mpsc::channel::<Job>();
        let jobs = Arc::new(Mutex::new(jobs));
        let (results, from_worker) = mpsc::channel::<Done>();
        let denoise = opts.denoise;
        let n_workers = runners.len();
        let workers = runners
            .into_iter()
            .enumerate()
            .map(|(i, mut runner)| {
                let jobs = Arc::clone(&jobs);
                let results = results.clone();
                thread::Builder::new()
                    .name(format!("stems-mdx-{i}"))
                    .spawn(move || {
                        loop {
                            let job = match jobs.lock() {
                                Ok(rx) => rx.recv(),
                                Err(_) => break,
                            };
                            let Ok(job) = job else { break };
                            let inst = match runner(&job.window) {
                                Ok(inst) => inst,
                                Err(e) => {
                                    tracing::error!("mdx inference failed: {e:#}");
                                    job.window.clone()
                                }
                            };
                            let done = Done {
                                generation: job.generation,
                                seq: job.seq,
                                start: job.start,
                                inst,
                            };
                            if results.send(done).is_err() {
                                break;
                            }
                        }
                    })
                    .expect("spawn mdx worker")
            })
            .collect();

        tracing::info!(
            hop_ms = hop as u64 * 1000 / 44_100,
            lookahead_ms = lookahead as u64 * 1000 / 44_100,
            latency_ms = latency as u64 * 1000 / 44_100,
            inferences_per_window = format!("{:.1}", params.gen_size() as f32 / hop as f32),
            denoise,
            workers = n_workers,
            "mdx streaming"
        );

        let mut s = Self {
            params,
            hop,
            lookahead,
            latency,
            history: VecDeque::new(),
            history_start: 0,
            input_frames: 0,
            next_window_end: 0,
            acc: VecDeque::new(),
            wacc: VecDeque::new(),
            acc_start: 0,
            ready: VecDeque::new(),
            ready_valid: VecDeque::new(),
            weights,
            to_worker,
            from_worker,
            workers,
            generation: 0,
            dispatch_seq: 0,
            next_seq: 0,
            pending: BTreeMap::new(),
            in_flight: 0,
            // one queued window per worker on top of those running
            max_in_flight: n_workers * 2,
            dropped: 0,
            starved: false,
            provider,
        };
        s.prime();
        s
    }

    fn prime(&mut self) {
        self.history.clear();
        self.history_start = 0;
        self.input_frames = 0;
        // First window ends once hop + lookahead real frames exist; it is
        // zero-padded on the left. Its finalised region is [0, hop).
        self.next_window_end = (self.hop + self.lookahead) as i64;
        self.acc.clear();
        self.wacc.clear();
        self.acc_start = 0;
        self.ready.clear();
        self.ready_valid.clear();
        self.ready.resize(self.latency * 2, 0.0);
        self.ready_valid.resize(self.latency, false);
        self.pending.clear();
        self.next_seq = self.dispatch_seq;
        self.in_flight = 0;
    }

    fn push_input(&mut self, input: &[f32]) {
        self.history.extend(input.iter().copied());
        self.input_frames += (input.len() / 2) as i64;
        // keep just over one window of history
        let keep = (self.params.chunk_size() + self.hop) as i64;
        let excess = (self.input_frames - self.history_start) - keep;
        if excess > 0 {
            self.history.drain(..(excess as usize) * 2);
            self.history_start += excess;
        }
    }

    fn dispatch_windows(&mut self) {
        let chunk = self.params.chunk_size() as i64;
        while self.input_frames >= self.next_window_end {
            let end = self.next_window_end;
            let start = end - chunk;
            self.next_window_end += self.hop as i64;
            if self.in_flight >= self.max_in_flight {
                // Falling behind: skip this window rather than queue it.
                // Frames only it covered stay invalid → dry fallback.
                if self.dropped == 0 {
                    tracing::warn!("mdx inference too slow: dropping windows");
                }
                self.dropped += 1;
                continue;
            }
            let mut window = vec![0.0f32; chunk as usize * 2];
            for f in start.max(self.history_start)..end {
                let src = ((f - self.history_start) as usize) * 2;
                let dst = ((f - start) as usize) * 2;
                window[dst] = self.history[src];
                window[dst + 1] = self.history[src + 1];
            }
            let job = Job {
                generation: self.generation,
                seq: self.dispatch_seq,
                start,
                window,
            };
            if self.to_worker.send(job).is_err() {
                break;
            }
            self.dispatch_seq += 1;
            self.in_flight += 1;
        }
    }

    fn collect_results(&mut self) {
        while let Ok(done) = self.from_worker.try_recv() {
            if done.generation != self.generation {
                continue;
            }
            self.in_flight = self.in_flight.saturating_sub(1);
            self.pending.insert(done.seq, done);
        }
        while let Some(done) = self.pending.remove(&self.next_seq) {
            self.next_seq += 1;
            self.accumulate(done.start, &done.inst);
            let end = done.start + self.params.chunk_size() as i64;
            self.finalize_upto(end - self.lookahead as i64);
        }
    }

    fn accumulate(&mut self, start: i64, inst: &[f32]) {
        let chunk = self.params.chunk_size();
        let trim = self.params.trim();
        for i in trim..chunk - trim {
            let x = start + i as i64;
            if x < self.acc_start {
                continue;
            }
            let idx = (x - self.acc_start) as usize;
            if idx >= self.acc.len() {
                self.acc.resize(idx + 1, [0.0; 2]);
                self.wacc.resize(idx + 1, 0.0);
            }
            let w = self.weights[i];
            self.acc[idx][0] += w * inst[2 * i];
            self.acc[idx][1] += w * inst[2 * i + 1];
            self.wacc[idx] += w;
        }
    }

    fn finalize_upto(&mut self, frontier: i64) {
        while self.acc_start < frontier {
            let (sum, w) = match (self.acc.pop_front(), self.wacc.pop_front()) {
                (Some(s), Some(w)) => (s, w),
                _ => ([0.0; 2], 0.0),
            };
            if w > 1e-6 {
                self.ready.push_back(sum[0] / w);
                self.ready.push_back(sum[1] / w);
                self.ready_valid.push_back(true);
            } else {
                self.ready.push_back(0.0);
                self.ready.push_back(0.0);
                self.ready_valid.push_back(false);
            }
            self.acc_start += 1;
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
        self.push_input(input);
        self.dispatch_windows();
        self.collect_results();
        let frames = instrumental.len() / 2;
        if self.ready_valid.len() < frames {
            if !self.starved {
                tracing::warn!("mdx separator starved: inference slower than real time");
                self.starved = true;
            }
            instrumental.fill(0.0);
            return false;
        }
        if self.starved {
            tracing::info!(dropped = self.dropped, "mdx separator caught up");
            self.starved = false;
            self.dropped = 0;
        }
        let mut valid = true;
        for o in instrumental.iter_mut() {
            *o = self.ready.pop_front().unwrap_or(0.0);
        }
        for _ in 0..frames {
            valid &= self.ready_valid.pop_front().unwrap_or(false);
        }
        valid
    }

    fn reset(&mut self) {
        self.generation += 1;
        while self.from_worker.try_recv().is_ok() {}
        self.prime();
    }
}

impl Drop for MdxSeparator {
    fn drop(&mut self) {
        let (tx, _) = mpsc::channel();
        self.to_worker = tx;
        for w in self.workers.drain(..) {
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

    /// Small geometry so the test runs fast: window 4096, trim 256, hop 1024.
    fn small_params() -> MdxParams {
        MdxParams {
            n_fft: 512,
            hop: 16,
            dim_f: 256,
            dim_t: 257,
            primary_stem: PrimaryStem::Instrumental,
            compensate: 1.0,
        }
    }

    fn run_stream(opts: StreamOpts, frames: usize) -> (Vec<f32>, Vec<f32>, usize, Vec<bool>) {
        let params = small_params();
        // identity runners; with several, odd ones are slower so results
        // come back out of order and must be re-sequenced
        let runners = (0..opts.workers.max(1))
            .map(|i| -> WindowRunner {
                Box::new(move |w: &[f32]| {
                    if i % 2 == 1 {
                        std::thread::sleep(std::time::Duration::from_micros(500));
                    }
                    Ok(w.to_vec())
                })
            })
            .collect();
        let mut sep = MdxSeparator::with_runners(params, "test", runners, opts);
        let latency = sep.latency_frames();
        let input: Vec<f32> = (0..frames)
            .flat_map(|i| [(i % 1000) as f32 / 1000.0, -((i % 777) as f32) / 777.0])
            .collect();
        let mut out = Vec::new();
        let mut valid = Vec::new();
        for b in input.chunks(128 * 2) {
            // the worker is effectively instant; the slack covers its latency
            std::thread::sleep(std::time::Duration::from_micros(200));
            let mut o = vec![0.0; b.len()];
            valid.push(sep.process(b, &mut o));
            out.extend(o);
        }
        (input, out, latency, valid)
    }

    #[test]
    fn identity_model_reproduces_input_delayed_by_latency_with_overlap() {
        let params = small_params();
        let opts = StreamOpts {
            hop_frames: 1024,
            lookahead_frames: 512,
            slack_frames: 1536,
            denoise: false,
            workers: 1,
        };
        assert_eq!(params.chunk_size(), 4096);
        check_identity(opts);
    }

    #[test]
    fn parallel_workers_resequence_results() {
        check_identity(StreamOpts {
            hop_frames: 1024,
            lookahead_frames: 512,
            slack_frames: 1536,
            denoise: false,
            workers: 3,
        });
    }

    fn check_identity(opts: StreamOpts) {
        let frames = 40_000;
        let (input, out, latency, valid) = run_stream(opts, frames);
        assert_eq!(latency, 512 + 1024 + 1536);
        // after priming, every block must carry a usable instrumental
        let priming_blocks = latency / 128 + 2;
        assert!(
            valid[priming_blocks..].iter().all(|v| *v),
            "invalid blocks after priming"
        );
        // compare a region well inside the stream
        let from = 8_000usize;
        let to = frames - latency - 2048;
        let mut max_err = 0.0f32;
        for f in from..to {
            for c in 0..2 {
                let a = input[2 * f + c];
                let b = out[2 * (f + latency) + c];
                max_err = max_err.max((a - b).abs());
            }
        }
        assert!(max_err < 1e-4, "max err {max_err}");
    }

    #[test]
    fn window_geometry() {
        let (_, p) = known_model("UVR-MDX-NET-Inst_HQ_3").unwrap();
        assert_eq!(p.chunk_size(), 261_120);
        assert_eq!(p.trim(), 3072);
        assert_eq!(p.gen_size(), 261_120 - 6144);
    }
}
