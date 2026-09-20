//! Pre-buffered separation: whole tracks are separated ahead of playback
//! (as fast as the device allows) into [`StemBuffer`]s, and
//! [`CachedStemSeparator`] serves the instrumental by track position with
//! zero latency. Frames the job has not reached yet fall back to dry.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use stems_core::{CHANNELS, Separator, TrackId, TrackPos};

use crate::params::MdxParams;
use crate::stream::{MdxSeparator, StreamOpts, WindowRunner};

/// Instrumental for one track, filled progressively from the start.
pub struct StemBuffer {
    data: Mutex<Vec<f32>>,
    /// Frames of `data` that are final.
    ready: AtomicUsize,
    total_frames: usize,
    done: AtomicBool,
    cancel: AtomicBool,
}

impl StemBuffer {
    pub fn new(total_frames: usize) -> Self {
        Self {
            data: Mutex::new(Vec::with_capacity(total_frames * CHANNELS as usize)),
            ready: AtomicUsize::new(0),
            total_frames,
            done: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        }
    }

    pub fn ready_frames(&self) -> usize {
        self.ready.load(Ordering::Acquire)
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    fn append(&self, samples: &[f32]) {
        let mut d = self.data.lock().expect("stem data");
        d.extend_from_slice(samples);
        let frames = d.len() / CHANNELS as usize;
        drop(d);
        self.ready.store(frames, Ordering::Release);
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
    }

    /// Copy `out.len() / 2` frames starting at `frame`; `false` if not
    /// (yet) available.
    pub fn read(&self, frame: u64, out: &mut [f32]) -> bool {
        let ch = CHANNELS as usize;
        let frames = out.len() / ch;
        let start = frame as usize;
        if start + frames > self.ready_frames() {
            return false;
        }
        let d = self.data.lock().expect("stem data");
        out.copy_from_slice(&d[start * ch..(start + frames) * ch]);
        true
    }
}

/// Stems by track, bounded; the newest few tracks stay resident.
#[derive(Default)]
pub struct StemStore {
    inner: Mutex<StoreInner>,
}

#[derive(Default)]
struct StoreInner {
    map: HashMap<TrackId, Arc<StemBuffer>>,
    order: VecDeque<TrackId>,
}

const STORE_CAP: usize = 3;

impl StemStore {
    pub fn get(&self, id: &TrackId) -> Option<Arc<StemBuffer>> {
        self.inner.lock().expect("stem store").map.get(id).cloned()
    }

    /// Register a buffer for `id`, evicting the oldest beyond the cap.
    pub fn insert(&self, id: TrackId, buf: Arc<StemBuffer>) {
        let mut g = self.inner.lock().expect("stem store");
        if g.map.insert(id.clone(), buf).is_none() {
            g.order.push_back(id);
        }
        while g.order.len() > STORE_CAP {
            if let Some(old) = g.order.pop_front() {
                if let Some(b) = g.map.remove(&old) {
                    b.cancel();
                }
            }
        }
    }

    pub fn contains(&self, id: &TrackId) -> bool {
        self.inner.lock().expect("stem store").map.contains_key(id)
    }
}

struct Job {
    id: TrackId,
    pcm: Arc<Vec<f32>>,
    buf: Arc<StemBuffer>,
}

/// One thread that separates whole tracks, one at a time, in submission
/// order, writing into the store. Model sessions live in its runners.
pub struct OfflineSeparator {
    tx: mpsc::Sender<Job>,
    store: Arc<StemStore>,
    thread: Option<JoinHandle<()>>,
}

impl OfflineSeparator {
    pub fn new(
        params: MdxParams,
        provider: &'static str,
        runners: Vec<WindowRunner>,
        opts: StreamOpts,
    ) -> Self {
        let store = Arc::new(StemStore::default());
        let (tx, rx) = mpsc::channel::<Job>();
        let opts = StreamOpts {
            realtime: false,
            // just enough slack for hop granularity; we block for results
            slack_frames: opts.hop_frames.max(1024),
            ..opts
        };
        let thread = thread::Builder::new()
            .name("stems-offline".into())
            .spawn(move || {
                let mut sep = MdxSeparator::with_runners(params, provider, runners, opts);
                for job in rx {
                    if job.buf.is_cancelled() {
                        continue;
                    }
                    let started = Instant::now();
                    run_job(&mut sep, &job);
                    tracing::info!(
                        track = ?job.id,
                        frames = job.buf.ready_frames(),
                        secs = format!("{:.1}", started.elapsed().as_secs_f32()),
                        speed = format!(
                            "{:.1}x",
                            job.buf.ready_frames() as f32 / 44_100.0 / started.elapsed().as_secs_f32().max(1e-3)
                        ),
                        cancelled = job.buf.is_cancelled(),
                        "stems ready"
                    );
                }
            })
            .expect("spawn offline separator");
        Self {
            tx,
            store,
            thread: Some(thread),
        }
    }

    pub fn store(&self) -> Arc<StemStore> {
        Arc::clone(&self.store)
    }

    /// Queue a whole-track separation; `pcm` is interleaved stereo 44.1 kHz.
    /// No-op if the track already has stems.
    pub fn submit(&self, id: TrackId, pcm: Arc<Vec<f32>>) -> Arc<StemBuffer> {
        if let Some(b) = self.store.get(&id) {
            return b;
        }
        let buf = Arc::new(StemBuffer::new(pcm.len() / CHANNELS as usize));
        self.store.insert(id.clone(), Arc::clone(&buf));
        let _ = self.tx.send(Job {
            id,
            pcm,
            buf: Arc::clone(&buf),
        });
        buf
    }
}

impl Drop for OfflineSeparator {
    fn drop(&mut self) {
        let (tx, _) = mpsc::channel();
        self.tx = tx;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Push the track through the streaming wrapper in blocks, discarding its
/// priming delay, then flush with silence so the tail comes out.
fn run_job(sep: &mut MdxSeparator, job: &Job) {
    const BLOCK: usize = 4096;
    let ch = CHANNELS as usize;
    sep.reset();
    let latency = sep.latency_frames();
    let total = job.pcm.len() / ch;
    let mut out = vec![0.0f32; BLOCK * ch];
    let mut produced: usize = 0; // frames popped from the separator
    let mut written: usize = 0; // frames appended to the buffer
    let mut feed =
        |input: &[f32], sep: &mut MdxSeparator, produced: &mut usize, written: &mut usize| {
            let frames = input.len() / ch;
            let o = &mut out[..input.len()];
            // offline mode blocks until the block is available
            let valid = sep.process(input, o);
            // output frame k corresponds to input frame k - latency
            let first = *produced;
            *produced += frames;
            let keep_from = latency.saturating_sub(first).min(frames);
            let keep_to = frames.min(latency + total - first);
            if keep_from < keep_to {
                let slice = &o[keep_from * ch..keep_to * ch];
                if valid {
                    job.buf.append(slice);
                } else {
                    // should not happen offline; keep alignment with dry audio
                    let src_from = first + keep_from - latency;
                    job.buf
                        .append(&job.pcm[src_from * ch..(src_from + keep_to - keep_from) * ch]);
                }
                *written += keep_to - keep_from;
            }
        };
    for block in job.pcm.chunks(BLOCK * ch) {
        if job.buf.is_cancelled() {
            return;
        }
        feed(block, sep, &mut produced, &mut written);
    }
    let zeros = vec![0.0f32; BLOCK * ch];
    while written < total && !job.buf.is_cancelled() {
        feed(&zeros, sep, &mut produced, &mut written);
    }
    job.buf.finish();
}

/// Zero-latency backend reading pre-computed stems by track position.
pub struct CachedStemSeparator {
    store: Arc<StemStore>,
    current: Option<(Arc<TrackId>, Arc<StemBuffer>)>,
    misses: u64,
}

impl CachedStemSeparator {
    pub fn new(store: Arc<StemStore>) -> Self {
        Self {
            store,
            current: None,
            misses: 0,
        }
    }
}

impl Separator for CachedStemSeparator {
    fn name(&self) -> &'static str {
        "prebuffer"
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn process(&mut self, _input: &[f32], instrumental: &mut [f32]) -> bool {
        instrumental.fill(0.0);
        false
    }

    fn process_at(
        &mut self,
        pos: Option<&TrackPos>,
        _input: &[f32],
        instrumental: &mut [f32],
    ) -> bool {
        let Some(pos) = pos else {
            return false;
        };
        let stale = match &self.current {
            Some((id, _)) => !Arc::ptr_eq(id, &pos.track) && **id != *pos.track,
            None => true,
        };
        if stale {
            self.current = self
                .store
                .get(&pos.track)
                .map(|b| (Arc::clone(&pos.track), b));
        }
        match &self.current {
            Some((_, buf)) if buf.read(pos.frame, instrumental) => true,
            _ => {
                self.misses += 1;
                false
            }
        }
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::PrimaryStem;

    fn params() -> MdxParams {
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
    fn offline_job_reproduces_track_and_serves_by_position() {
        let runners: Vec<WindowRunner> = (0..2)
            .map(|_| -> WindowRunner {
                Box::new(|w: &[f32]| Ok(w.iter().map(|x| x * 0.5).collect()))
            })
            .collect();
        let opts = StreamOpts {
            hop_frames: 1024,
            lookahead_frames: 512,
            slack_frames: 0,
            denoise: false,
            workers: 2,
            realtime: false,
        };
        let off = OfflineSeparator::new(params(), "test", runners, opts);
        let frames = 30_000;
        let pcm: Arc<Vec<f32>> = Arc::new(
            (0..frames)
                .flat_map(|i| [(i % 500) as f32 / 500.0, -((i % 333) as f32) / 333.0])
                .collect(),
        );
        let id = TrackId::Spotify("t1".into());
        let buf = off.submit(id.clone(), Arc::clone(&pcm));
        let t0 = Instant::now();
        while !buf.is_done() {
            assert!(t0.elapsed().as_secs() < 30, "job did not finish");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(buf.ready_frames(), frames);

        let mut sep = CachedStemSeparator::new(off.store());
        let pos = TrackPos {
            track: Arc::new(id),
            frame: 10_000,
        };
        let mut out = vec![0.0f32; 512 * 2];
        assert!(sep.process_at(Some(&pos), &[], &mut out));
        for k in 0..512 {
            let expect = pcm[(10_000 + k) * 2] * 0.5;
            assert!((out[k * 2] - expect).abs() < 1e-4, "frame {k}");
        }
        // past the end → miss
        let late = TrackPos {
            track: pos.track.clone(),
            frame: frames as u64 - 100,
        };
        assert!(!sep.process_at(Some(&late), &[], &mut out));
        // unknown track → miss
        let other = TrackPos {
            track: Arc::new(TrackId::Spotify("nope".into())),
            frame: 0,
        };
        assert!(!sep.process_at(Some(&other), &[], &mut out));
    }
}
