//! Pre-buffered separation: whole tracks are separated ahead of playback
//! (as fast as the device allows) into [`StemBuffer`]s, and
//! [`CachedStemSeparator`] serves the instrumental by track position with
//! zero latency. Frames the job has not reached yet fall back to dry.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use stems_core::{CHANNELS, Separator, TrackId, TrackPos};

use crate::params::MdxParams;
use crate::stream::{MdxSeparator, StreamOpts, WindowRunner};

/// Instrumental for one track. Pre-allocated; regions are marked as
/// they are computed, starting wherever the playhead is (`wanted`).
pub struct StemBuffer {
    data: Mutex<Vec<f32>>,
    /// Computed frame ranges, sorted and merged.
    covered: Mutex<Vec<(usize, usize)>>,
    /// Frames covered in total (for progress).
    ready: AtomicUsize,
    total_frames: usize,
    done: AtomicBool,
    cancel: AtomicBool,
    /// Where playback is; the job works from here first.
    wanted: AtomicUsize,
}

impl StemBuffer {
    pub fn new(total_frames: usize) -> Self {
        Self {
            data: Mutex::new(vec![0.0; total_frames * CHANNELS as usize]),
            covered: Mutex::new(Vec::new()),
            ready: AtomicUsize::new(0),
            total_frames,
            done: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            wanted: AtomicUsize::new(0),
        }
    }

    /// Frames computed so far (not necessarily contiguous).
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

    /// Tell the job where playback is so it computes that region first.
    pub fn set_wanted(&self, frame: u64) {
        self.wanted
            .store((frame as usize).min(self.total_frames), Ordering::Release);
    }

    pub fn wanted(&self) -> usize {
        self.wanted.load(Ordering::Acquire)
    }

    fn write(&self, from: usize, samples: &[f32]) {
        let ch = CHANNELS as usize;
        let frames = samples.len() / ch;
        let to = (from + frames).min(self.total_frames);
        if to <= from {
            return;
        }
        {
            let mut d = self.data.lock().expect("stem data");
            d[from * ch..to * ch].copy_from_slice(&samples[..(to - from) * ch]);
        }
        let mut c = self.covered.lock().expect("stem ranges");
        c.push((from, to));
        c.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(c.len());
        for &(a, b) in c.iter() {
            match merged.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        *c = merged;
        let sum: usize = c.iter().map(|(a, b)| b - a).sum();
        self.ready.store(sum, Ordering::Release);
        if sum >= self.total_frames {
            self.done.store(true, Ordering::Release);
        }
    }

    fn is_covered(&self, from: usize, to: usize) -> bool {
        let c = self.covered.lock().expect("stem ranges");
        c.iter().any(|&(a, b)| a <= from && to <= b)
    }

    /// The uncovered range to work on next: the one containing `wanted`
    /// (from `wanted` on), else the first one after it, else the first.
    fn next_gap(&self, wanted: usize) -> Option<(usize, usize)> {
        let c = self.covered.lock().expect("stem ranges");
        let mut gaps: Vec<(usize, usize)> = Vec::new();
        let mut cursor = 0;
        for &(a, b) in c.iter() {
            if a > cursor {
                gaps.push((cursor, a));
            }
            cursor = cursor.max(b);
        }
        if cursor < self.total_frames {
            gaps.push((cursor, self.total_frames));
        }
        if let Some(&(a, b)) = gaps.iter().find(|(a, b)| *a <= wanted && wanted < *b) {
            return Some((wanted.max(a), b));
        }
        gaps.iter()
            .find(|(a, _)| *a > wanted)
            .or(gaps.first())
            .copied()
    }

    /// Copy `out.len() / 2` frames starting at `frame`; `false` if not
    /// (yet) available.
    pub fn read(&self, frame: u64, out: &mut [f32]) -> bool {
        let ch = CHANNELS as usize;
        let frames = out.len() / ch;
        let start = frame as usize;
        if start + frames > self.total_frames || !self.is_covered(start, start + frames) {
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

    pub fn remove(&self, id: &TrackId) -> Option<Arc<StemBuffer>> {
        let mut g = self.inner.lock().expect("stem store");
        g.order.retain(|k| k != id);
        g.map.remove(id)
    }
}

struct Job {
    id: TrackId,
    pcm: Arc<Vec<f32>>,
    buf: Arc<StemBuffer>,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}

/// One thread that separates whole tracks in priority order, writing into
/// the store. Model sessions live in its runners.
pub struct OfflineSeparator {
    queue: Arc<(Mutex<Queue>, Condvar)>,
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
        let queue: Arc<(Mutex<Queue>, Condvar)> = Arc::default();
        let opts = StreamOpts {
            realtime: false,
            // just enough slack for hop granularity; we block for results
            slack_frames: opts.hop_frames.max(1024),
            ..opts
        };
        let q = Arc::clone(&queue);
        let thread = thread::Builder::new()
            .name("stems-offline".into())
            .spawn(move || {
                let mut sep = MdxSeparator::with_runners(params, provider, runners, opts);
                loop {
                    let job = {
                        let (lock, cv) = &*q;
                        let mut g = lock.lock().expect("job queue");
                        loop {
                            if let Some(j) = g.jobs.pop_front() {
                                break j;
                            }
                            if g.closed {
                                return;
                            }
                            g = cv.wait(g).expect("job queue");
                        }
                    };
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
                            job.buf.ready_frames() as f32
                                / 44_100.0
                                / started.elapsed().as_secs_f32().max(1e-3)
                        ),
                        cancelled = job.buf.is_cancelled(),
                        "stems ready"
                    );
                }
            })
            .expect("spawn offline separator");
        Self {
            queue,
            store,
            thread: Some(thread),
        }
    }

    pub fn store(&self) -> Arc<StemStore> {
        Arc::clone(&self.store)
    }

    /// Queue a whole-track separation starting around `wanted_frame`.
    /// `urgent` (the playing track) goes to the front. No-op if the track
    /// already has stems.
    pub fn submit(
        &self,
        id: TrackId,
        pcm: Arc<Vec<f32>>,
        wanted_frame: u64,
        urgent: bool,
    ) -> Arc<StemBuffer> {
        if let Some(b) = self.store.get(&id) {
            b.set_wanted(wanted_frame);
            return b;
        }
        let buf = Arc::new(StemBuffer::new(pcm.len() / CHANNELS as usize));
        buf.set_wanted(wanted_frame);
        self.store.insert(id.clone(), Arc::clone(&buf));
        let job = Job {
            id,
            pcm,
            buf: Arc::clone(&buf),
        };
        let (lock, cv) = &*self.queue;
        let mut g = lock.lock().expect("job queue");
        if urgent {
            g.jobs.push_front(job);
        } else {
            g.jobs.push_back(job);
        }
        cv.notify_one();
        buf
    }

    /// Where playback is on `id`; the job (re)targets that region.
    pub fn set_wanted(&self, id: &TrackId, frame: u64) {
        if let Some(b) = self.store.get(id) {
            b.set_wanted(frame);
        }
    }

    /// Cancel and forget every track not in `keep` (queued or running),
    /// so the playing and upcoming tracks get the device.
    pub fn focus(&self, keep: &[&TrackId]) {
        let (lock, _) = &*self.queue;
        let mut g = lock.lock().expect("job queue");
        g.jobs.retain(|j| {
            let k = keep.contains(&&j.id);
            if !k {
                j.buf.cancel();
            }
            k
        });
        drop(g);
        // finished stems stay cached (LRU); unfinished ones are dropped
        let ids: Vec<TrackId> = {
            let inner = self.store.inner.lock().expect("stem store");
            inner
                .map
                .iter()
                .filter(|(id, b)| !keep.contains(id) && !b.is_done())
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            if let Some(b) = self.store.remove(&id) {
                b.cancel();
                tracing::info!(track = ?id, "stems job dropped");
            }
        }
    }
}

impl Drop for OfflineSeparator {
    fn drop(&mut self) {
        let (lock, cv) = &*self.queue;
        if let Ok(mut g) = lock.lock() {
            g.closed = true;
            for j in &g.jobs {
                j.buf.cancel();
            }
        }
        cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Work through the track in segments, always starting at the region the
/// playhead wants; a seek re-targets after the current window.
fn run_job(sep: &mut MdxSeparator, job: &Job) {
    let total = job.pcm.len() / CHANNELS as usize;
    let mut last_wanted = job.buf.wanted();
    while !job.buf.is_cancelled() {
        let wanted = job.buf.wanted();
        let Some((from, to)) = job.buf.next_gap(wanted) else {
            break;
        };
        // segment boundaries on the hop grid keep window edges consistent
        let from = if from == wanted {
            from
        } else {
            from.min(total)
        };
        run_segment(sep, job, from, to, &mut last_wanted);
    }
    if job.buf.ready_frames() >= total {
        job.buf.done.store(true, Ordering::Release);
    }
}

/// Separate `[from, to)`, writing as it goes. Returns early (leaving the
/// rest uncovered) when the playhead jumps somewhere this segment will
/// not reach soon.
fn run_segment(sep: &mut MdxSeparator, job: &Job, from: usize, to: usize, last_wanted: &mut usize) {
    const BLOCK: usize = 4096;
    let ch = CHANNELS as usize;
    sep.reset();
    let latency = sep.latency_frames();
    let len = to - from;
    let mut out = vec![0.0f32; BLOCK * ch];
    let mut produced: usize = 0; // frames popped from the separator
    let mut written: usize = 0; // frames written to the buffer
    let pcm = &job.pcm[from * ch..to * ch];
    let mut feed =
        |input: &[f32], sep: &mut MdxSeparator, produced: &mut usize, written: &mut usize| {
            let frames = input.len() / ch;
            let o = &mut out[..input.len()];
            let valid = sep.process(input, o); // blocks until available
            let first = *produced;
            *produced += frames;
            // output frame k is input frame k - latency
            let keep_from = latency.saturating_sub(first).min(frames);
            let keep_to = frames.min((latency + len).saturating_sub(first));
            if keep_from < keep_to {
                let dst = from + first + keep_from - latency;
                if valid {
                    job.buf.write(dst, &o[keep_from * ch..keep_to * ch]);
                } else {
                    // should not happen offline; keep alignment with dry audio
                    job.buf
                        .write(dst, &job.pcm[dst * ch..(dst + keep_to - keep_from) * ch]);
                }
                *written += keep_to - keep_from;
            }
        };
    let zeros = vec![0.0f32; BLOCK * ch];
    let mut fed = 0;
    while written < len && !job.buf.is_cancelled() {
        let block: &[f32] = if fed < pcm.len() {
            let end = (fed + BLOCK * ch).min(pcm.len());
            let b = &pcm[fed..end];
            fed = end;
            b
        } else {
            &zeros
        };
        feed(block, sep, &mut produced, &mut written);
        // Re-target on seek: the wanted region is neither done nor about
        // to be reached by this segment.
        let wanted = job.buf.wanted();
        if wanted != *last_wanted {
            *last_wanted = wanted;
            let ahead = from + written;
            let soon = wanted >= ahead && wanted < ahead + 5 * 44_100;
            if !soon
                && !job
                    .buf
                    .is_covered(wanted, (wanted + BLOCK).min(job.buf.total_frames))
            {
                tracing::debug!(wanted, at = ahead, "stems job re-targeting to playhead");
                return;
            }
        }
    }
}

/// Zero-latency backend reading pre-computed stems by track position.
pub struct CachedStemSeparator {
    store: Arc<StemStore>,
    current: Option<(Arc<TrackId>, Arc<StemBuffer>)>,
    misses: u64,
    hits: u64,
}

impl CachedStemSeparator {
    pub fn new(store: Arc<StemStore>) -> Self {
        Self {
            store,
            current: None,
            misses: 0,
            hits: 0,
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
            self.misses += 1;
            if self.misses % 500 == 1 {
                tracing::debug!("prebuffer: no track position yet");
            }
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
            Some((_, buf)) if buf.read(pos.frame, instrumental) => {
                self.hits += 1;
                if self.hits % 2000 == 1 {
                    tracing::debug!(
                        frame = pos.frame,
                        ready = buf.ready_frames(),
                        "prebuffer: serving stems"
                    );
                }
                true
            }
            cur => {
                self.misses += 1;
                if self.misses % 500 == 1 {
                    match cur {
                        Some((_, buf)) => tracing::debug!(
                            frame = pos.frame,
                            ready = buf.ready_frames(),
                            "prebuffer: stems not ready for this position"
                        ),
                        None => {
                            tracing::debug!(track = ?pos.track, "prebuffer: no stems for track")
                        }
                    }
                }
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
    fn seek_retargets_the_running_job() {
        let runners: Vec<WindowRunner> = (0..1)
            .map(|_| -> WindowRunner {
                Box::new(|w: &[f32]| {
                    std::thread::sleep(std::time::Duration::from_millis(3));
                    Ok(w.to_vec())
                })
            })
            .collect();
        let opts = StreamOpts {
            hop_frames: 1024,
            lookahead_frames: 512,
            slack_frames: 0,
            denoise: false,
            workers: 1,
            realtime: false,
        };
        let off = OfflineSeparator::new(params(), "test", runners, opts);
        let frames = 400_000; // ~390 windows × 3 ms ≈ 1.2 s of work
        let pcm: Arc<Vec<f32>> = Arc::new(vec![0.25; frames * 2]);
        let id = TrackId::Spotify("t2".into());
        let buf = off.submit(id.clone(), Arc::clone(&pcm), 0, true);
        std::thread::sleep(std::time::Duration::from_millis(60));
        off.set_wanted(&id, 300_000);
        let t0 = Instant::now();
        let mut out = vec![0.0f32; 512 * 2];
        while !buf.read(300_000, &mut out) {
            assert!(t0.elapsed().as_secs() < 20, "never reached the seek target");
            assert!(
                !buf.is_done(),
                "finished the whole track before the seek target"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            buf.ready_frames() < frames - 50_000,
            "seek target reached only after nearly everything: {}",
            buf.ready_frames()
        );
        while !buf.is_done() {
            assert!(t0.elapsed().as_secs() < 30);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(buf.ready_frames(), frames);
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
        let buf = off.submit(id.clone(), Arc::clone(&pcm), 20_000, true);
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
        // the segment before `wanted` was filled in afterwards
        let early = TrackPos {
            track: pos.track.clone(),
            frame: 1_000,
        };
        assert!(sep.process_at(Some(&early), &[], &mut out));
        assert!((out[0] - pcm[1_000 * 2] * 0.5).abs() < 1e-4);
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
