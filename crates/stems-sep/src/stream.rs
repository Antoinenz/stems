//! Streaming wrapper around a window runner: overlapping windows go to
//! worker threads, results are blended with a weighted overlap-add, and
//! finalised audio comes back delayed by exactly `latency_frames()`.
//! Independent of ONNX so it also drives the remote worker client.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use stems_core::Separator;

use crate::params::MdxParams;

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
    /// `true` (playback): never block, thin or drop windows when late.
    /// `false` (offline): block for results and process every window.
    pub realtime: bool,
}

impl Default for StreamOpts {
    fn default() -> Self {
        Self {
            hop_frames: 0,
            lookahead_frames: 0,
            slack_frames: 44_100 * 2,
            denoise: false,
            workers: 1,
            realtime: true,
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
    /// Runner wall time, for the load predictor.
    elapsed_ms: f64,
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
    n_workers: usize,
    /// Smoothed runner time per window (ms); 0 until the first result.
    pred_ms: f64,
    /// Absolute end frame of the last window actually dispatched.
    last_dispatched_end: i64,
    /// Windows skipped because the predictor said they would miss their
    /// deadline (graceful degradation under thermal throttling).
    thinned: u64,
    thinned_episode: u64,
    /// Consecutive full-rate dispatches, for log hysteresis.
    since_thin: u32,
    thinning: bool,
    starved: bool,
    realtime: bool,
    provider: &'static str,
}

/// Runs one window → instrumental. Boxed so the streaming logic can be
/// tested without an ONNX session.
pub type WindowRunner = Box<dyn FnMut(&[f32]) -> Result<Vec<f32>> + Send>;

impl MdxSeparator {
    #[cfg(feature = "onnx")]
    pub fn new(model: crate::mdx::MdxModel, opts: StreamOpts) -> Self {
        Self::with_models(vec![model], opts)
    }

    /// One worker thread per model session.
    #[cfg(feature = "onnx")]
    pub fn with_models(models: Vec<crate::mdx::MdxModel>, opts: StreamOpts) -> Self {
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
                            let started = Instant::now();
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
                                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
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
            n_workers,
            pred_ms: 0.0,
            last_dispatched_end: i64::MIN / 2,
            thinned: 0,
            thinned_episode: 0,
            since_thin: 0,
            thinning: false,
            starved: false,
            realtime: opts.realtime,
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
        self.last_dispatched_end = i64::MIN / 2;
    }

    /// Would the window ending at `end` be finished in time? It finalises
    /// everything after the last dispatched window's frontier, and the
    /// first of those frames is due `hop + slack` after that window's
    /// end; queued windows ahead of it share the workers.
    fn will_miss_deadline(&self, end: i64) -> bool {
        if self.pred_ms <= 0.0 {
            return false;
        }
        let queued = self.in_flight as f64 / self.n_workers as f64;
        (queued + 1.0) * self.pred_ms > self.budget_ms(end)
    }

    /// Time the window ending at `end` may take, with a safety margin.
    fn budget_ms(&self, end: i64) -> f64 {
        let gap = (end - self.last_dispatched_end).max(self.hop as i64) as f64;
        let budget_frames = (self.latency - self.lookahead + self.hop) as f64 - gap;
        budget_frames * 1000.0 / 44_100.0 * 0.85
    }

    /// Thin when the workers cannot sustain one window per hop (space
    /// dispatches at least `pred / workers` apart, with margin), or when
    /// the queue already predicts a missed deadline and skipping lets
    /// the following window make its tighter one.
    fn should_thin(&self, end: i64) -> bool {
        if self.pred_ms <= 0.0 || !self.can_skip(end) {
            return false;
        }
        let gap_ms = (end - self.last_dispatched_end) as f64 * 1000.0 / 44_100.0;
        let sustain_ms = self.pred_ms / self.n_workers as f64 * 1.1;
        if gap_ms < sustain_ms {
            return true;
        }
        if !self.will_miss_deadline(end) {
            return false;
        }
        let hop_ms = self.hop as f64 * 1000.0 / 44_100.0;
        let queued = self.in_flight as f64 / self.n_workers as f64;
        let eta_next = (queued * self.pred_ms - hop_ms).max(0.0) + self.pred_ms;
        eta_next <= self.budget_ms(end + self.hop as i64)
    }

    /// Skipping the window ending at `end` leaves no uncovered frames as
    /// long as the next window still overlaps the last dispatched one.
    /// Windows skipped so far by the load predictor.
    pub fn thinned(&self) -> u64 {
        self.thinned
    }

    fn can_skip(&self, end: i64) -> bool {
        end + self.hop as i64 - self.last_dispatched_end <= self.params.gen_size() as i64
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
            if !self.realtime {
                // offline: bounded queue, every window processed
                while self.in_flight >= self.max_in_flight && self.collect_blocking() {}
                if !self.dispatch(start, end) {
                    break;
                }
                continue;
            }
            if self.should_thin(end) {
                // Predicted late: thin the overlap instead of queueing.
                // Neighbouring windows still cover these frames.
                if !self.thinning {
                    tracing::info!(
                        pred_ms = self.pred_ms as u64,
                        "mdx load high: thinning windows"
                    );
                    self.thinning = true;
                }
                self.thinned += 1;
                self.thinned_episode += 1;
                self.since_thin = 0;
                continue;
            }
            if self.in_flight >= self.max_in_flight {
                // Falling behind: skip this window rather than queue it.
                // Frames only it covered stay invalid → dry fallback.
                if self.dropped == 0 {
                    tracing::warn!("mdx inference too slow: dropping windows");
                }
                self.dropped += 1;
                continue;
            }
            self.since_thin += 1;
            if self.thinning && self.since_thin >= 8 {
                tracing::info!(
                    thinned = self.thinned_episode,
                    pred_ms = self.pred_ms as u64,
                    "mdx load normal: full overlap"
                );
                self.thinning = false;
                self.thinned_episode = 0;
            }
            if !self.dispatch(start, end) {
                break;
            }
        }
    }

    /// Send the window `[start, end)` to the workers.
    fn dispatch(&mut self, start: i64, end: i64) -> bool {
        let chunk = self.params.chunk_size() as i64;
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
            return false;
        }
        self.dispatch_seq += 1;
        self.in_flight += 1;
        self.last_dispatched_end = end;
        true
    }

    /// Block for one result (offline mode). `false` when the workers are gone
    /// or nothing is in flight.
    fn collect_blocking(&mut self) -> bool {
        if self.in_flight == 0 {
            return false;
        }
        match self.from_worker.recv() {
            Ok(done) => {
                self.on_result(done);
                self.drain_pending();
                true
            }
            Err(_) => false,
        }
    }

    fn on_result(&mut self, done: Done) {
        if done.generation != self.generation {
            return;
        }
        self.in_flight = self.in_flight.saturating_sub(1);
        self.pred_ms = if self.pred_ms <= 0.0 {
            done.elapsed_ms
        } else {
            // fast attack, slow decay: react to a throttle quickly,
            // ease back to full overlap once it clears
            let a = if done.elapsed_ms > self.pred_ms {
                0.5
            } else {
                0.1
            };
            self.pred_ms + a * (done.elapsed_ms - self.pred_ms)
        };
        self.pending.insert(done.seq, done);
    }

    fn drain_pending(&mut self) {
        while let Some(done) = self.pending.remove(&self.next_seq) {
            self.next_seq += 1;
            self.accumulate(done.start, &done.inst);
            let end = done.start + self.params.chunk_size() as i64;
            self.finalize_upto(end - self.lookahead as i64);
        }
    }

    fn collect_results(&mut self) {
        while let Ok(done) = self.from_worker.try_recv() {
            self.on_result(done);
        }
        self.drain_pending();
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
        if !self.realtime {
            while self.ready_valid.len() < frames && self.collect_blocking() {}
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{PrimaryStem, known_model};

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

    struct Run {
        input: Vec<f32>,
        out: Vec<f32>,
        latency: usize,
        valid: Vec<bool>,
        thinned: u64,
    }

    /// Identity runners taking `runner_ms` each; with several workers the
    /// odd ones are slower so results come back out of order. Blocks are
    /// paced at `block_us` per 128 frames (real time = 2902 µs).
    fn run_stream(opts: StreamOpts, frames: usize, runner_ms: u64, block_us: u64) -> Run {
        let params = small_params();
        let runners = (0..opts.workers.max(1))
            .map(|i| -> WindowRunner {
                Box::new(move |w: &[f32]| {
                    let extra = if i % 2 == 1 { 500 } else { 0 };
                    std::thread::sleep(std::time::Duration::from_micros(runner_ms * 1000 + extra));
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
            std::thread::sleep(std::time::Duration::from_micros(block_us));
            let mut o = vec![0.0; b.len()];
            valid.push(sep.process(b, &mut o));
            out.extend(o);
        }
        Run {
            input,
            out,
            latency,
            valid,
            thinned: sep.thinned(),
        }
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
            realtime: true,
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
            realtime: true,
        });
    }

    #[test]
    fn slow_runner_thins_overlap_without_holes() {
        // a 35 ms runner cannot do every 23 ms hop; windows must be
        // skipped. The slack is generous (186 ms) because CI runners
        // overshoot sleeps badly; the point is thinning, not tight timing.
        let opts = StreamOpts {
            hop_frames: 1024,
            lookahead_frames: 512,
            slack_frames: 8192,
            denoise: false,
            workers: 1,
            realtime: true,
        };
        let r = run_stream(opts, 120_000, 35, 2902);
        assert!(r.thinned > 5, "thinned {}", r.thinned);
        let priming_blocks = r.latency / 128 + 2;
        let invalid: Vec<usize> = r.valid[priming_blocks..]
            .iter()
            .enumerate()
            .filter(|(_, v)| !**v)
            .map(|(i, _)| i + priming_blocks)
            .collect();
        // a couple of blocks may still slip on a heavily loaded machine
        assert!(
            invalid.len() <= 2,
            "invalid blocks after priming: {invalid:?} of {}",
            r.valid.len()
        );
    }

    fn check_identity(opts: StreamOpts) {
        let frames = 40_000;
        let Run {
            input,
            out,
            latency,
            valid,
            ..
        } = run_stream(opts, frames, 0, 200);
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
