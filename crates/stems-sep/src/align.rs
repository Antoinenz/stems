//! Word-level lyric timing by CTC forced alignment on the vocal stem.
//!
//! A wav2vec2 CTC model (characters, 16 kHz, 20 ms frames) turns audio into
//! per-frame letter probabilities; a Viterbi pass over the known lyric text
//! finds the most likely frame for every letter, and words inherit their
//! first and last letters' frames. Works per lyric line on the vocals
//! (mix − instrumental), which is why it lives behind pre-buffering.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use ort::session::Session;
use ort::value::Tensor;

/// Model sample rate and frame hop.
pub const ALIGN_RATE: u32 = 16_000;
const FRAME_MS: f32 = 20.0;
const BLANK: usize = 0;
const WORD_SEP: usize = 4;

/// wav2vec2-base-960h vocabulary (index = token id).
const VOCAB: [char; 32] = [
    '\0', '\0', '\0', '\0', '|', 'E', 'T', 'A', 'O', 'N', 'I', 'H', 'S', 'R', 'D', 'L', 'U', 'M',
    'W', 'C', 'F', 'G', 'Y', 'P', 'B', 'V', 'K', '\'', 'X', 'J', 'Q', 'Z',
];

/// A word's span inside the audio handed to [`Aligner::align`].
#[derive(Debug, Clone, PartialEq)]
pub struct WordSpan {
    pub start_ms: u32,
    pub end_ms: u32,
    /// Mean log-probability of the word's letters along the path.
    pub score: f32,
}

pub struct Aligner {
    session: Session,
    input_name: String,
}

/// Known model: download source.
pub const MODEL_NAME: &str = "wav2vec2-base-960h";
pub const MODEL_URL: &str =
    "https://huggingface.co/Xenova/wav2vec2-base-960h/resolve/main/onnx/model_quantized.onnx";

impl Aligner {
    /// Loads on the CPU provider: alignment runs offline and the quantized
    /// model has no GPU kernels anyway.
    pub fn load(path: &Path, threads: usize) -> Result<Self> {
        let mut builder = Session::builder().context("ort session builder")?;
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
        tracing::info!(model = %path.display(), input = input_name, "aligner loaded");
        Ok(Self {
            session,
            input_name,
        })
    }

    /// Per-frame log-probabilities `[frames][32]` for mono 16 kHz audio.
    pub fn log_probs(&mut self, audio16k: &[f32]) -> Result<Vec<Vec<f32>>> {
        // wav2vec2 expects zero-mean, unit-variance input
        let n = audio16k.len().max(1) as f32;
        let mean = audio16k.iter().sum::<f32>() / n;
        let var = audio16k
            .iter()
            .map(|x| (x - mean) * (x - mean))
            .sum::<f32>()
            / n;
        let norm: Vec<f32> = audio16k
            .iter()
            .map(|x| (x - mean) / (var + 1e-7).sqrt())
            .collect();
        let len = norm.len();
        let tensor = Tensor::from_array((vec![1i64, len as i64], norm)).context("input tensor")?;
        let started = Instant::now();
        let outputs = self
            .session
            .run(ort::inputs![self.input_name.as_str() => tensor])
            .context("model run")?;
        let (shape, logits) = outputs[0]
            .try_extract_tensor::<f32>()
            .context("output tensor")?;
        let dims: Vec<i64> = shape.iter().copied().collect();
        anyhow::ensure!(
            dims.len() == 3 && dims[2] == 32,
            "unexpected logits shape {dims:?}"
        );
        let frames = dims[1] as usize;
        tracing::debug!(
            ms = started.elapsed().as_millis() as u64,
            audio_ms = (len as f32 / ALIGN_RATE as f32 * 1000.0) as u64,
            frames,
            "aligner inference"
        );
        let mut out = Vec::with_capacity(frames);
        for t in 0..frames {
            let row = &logits[t * 32..(t + 1) * 32];
            let max = row.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            let lse = max + row.iter().map(|v| (v - max).exp()).sum::<f32>().ln();
            out.push(row.iter().map(|v| v - lse).collect());
        }
        Ok(out)
    }

    /// Align `words` to `audio16k` (mono, 16 kHz). Returns one span per
    /// word, or `None` when the text cannot be placed convincingly.
    pub fn align(&mut self, audio16k: &[f32], words: &[String]) -> Result<Option<Vec<WordSpan>>> {
        let lp = self.log_probs(audio16k)?;
        Ok(forced_align(&lp, words))
    }
}

/// Map a word to token ids; letters outside the vocabulary are dropped.
fn tokenize(word: &str) -> Vec<usize> {
    word.chars()
        .filter_map(|c| {
            let c = c.to_ascii_uppercase();
            VOCAB
                .iter()
                .position(|v| *v == c)
                .filter(|i| *i >= 5 || c == '\'')
        })
        .collect()
}

/// CTC forced alignment (Viterbi over the blank-interleaved token
/// sequence). Public for tests; `log_probs` is `[frames][vocab]`.
pub fn forced_align(log_probs: &[Vec<f32>], words: &[String]) -> Option<Vec<WordSpan>> {
    // token sequence: w1 | w2 | w3 …, remembering which word each token is
    let mut tokens: Vec<usize> = Vec::new();
    let mut owner: Vec<usize> = Vec::new(); // word index per token, usize::MAX for '|'
    for (wi, w) in words.iter().enumerate() {
        let t = tokenize(w);
        if t.is_empty() {
            continue;
        }
        if !tokens.is_empty() {
            tokens.push(WORD_SEP);
            owner.push(usize::MAX);
        }
        for id in t {
            tokens.push(id);
            owner.push(wi);
        }
    }
    let t_frames = log_probs.len();
    if tokens.is_empty() || t_frames == 0 {
        return None;
    }
    // extended sequence with blanks: b t1 b t2 b … tn b
    let s_len = tokens.len() * 2 + 1;
    let tok_at = |s: usize| if s % 2 == 1 { tokens[s / 2] } else { BLANK };
    if s_len > t_frames * 2 + 1 {
        return None; // text longer than the audio could carry
    }
    let neg = f32::NEG_INFINITY;
    let mut score = vec![vec![neg; s_len]; t_frames];
    let mut back = vec![vec![0u8; s_len]; t_frames]; // 0: stay, 1: from s-1, 2: from s-2
    score[0][0] = log_probs[0][BLANK];
    if s_len > 1 {
        score[0][1] = log_probs[0][tok_at(1)];
    }
    for t in 1..t_frames {
        for s in 0..s_len {
            let tok = tok_at(s);
            let mut best = score[t - 1][s];
            let mut from = 0u8;
            if s >= 1 && score[t - 1][s - 1] > best {
                best = score[t - 1][s - 1];
                from = 1;
            }
            // skip a blank between two different non-blank tokens
            if s >= 2 && tok != BLANK && tok != tok_at(s - 2) && score[t - 1][s - 2] > best {
                best = score[t - 1][s - 2];
                from = 2;
            }
            if best > neg {
                score[t][s] = best + log_probs[t][tok];
                back[t][s] = from;
            }
        }
    }
    // end in the final token or the trailing blank
    let (mut s, final_score) =
        if s_len >= 2 && score[t_frames - 1][s_len - 2] > score[t_frames - 1][s_len - 1] {
            (s_len - 2, score[t_frames - 1][s_len - 2])
        } else {
            (s_len - 1, score[t_frames - 1][s_len - 1])
        };
    if final_score == neg {
        return None;
    }
    // backtrack: frame of every state visit
    let mut path = vec![0usize; t_frames]; // state at each frame
    for t in (0..t_frames).rev() {
        path[t] = s;
        if t > 0 {
            s -= back[t][s] as usize;
        }
    }
    // per word: first and last frame of any of its letters, and mean letter log-prob
    let mut spans: Vec<Option<(usize, usize, f32, u32)>> = vec![None; words.len()];
    for (t, &st) in path.iter().enumerate() {
        if st % 2 == 0 {
            continue;
        }
        let wi = owner[st / 2];
        if wi == usize::MAX {
            continue;
        }
        let lp = log_probs[t][tok_at(st)];
        let e = spans[wi].get_or_insert((t, t, 0.0, 0));
        e.0 = e.0.min(t);
        e.1 = e.1.max(t);
        e.2 += lp;
        e.3 += 1;
    }
    let mut out = Vec::with_capacity(words.len());
    let mut last_end = 0u32;
    for sp in spans {
        match sp {
            Some((a, b, sum, n)) => {
                let start = (a as f32 * FRAME_MS) as u32;
                let end = ((b + 1) as f32 * FRAME_MS) as u32;
                last_end = end;
                out.push(WordSpan {
                    start_ms: start,
                    end_ms: end,
                    score: sum / n.max(1) as f32,
                });
            }
            // word had no alignable letters (e.g. "…"): zero-length at the previous end
            None => out.push(WordSpan {
                start_ms: last_end,
                end_ms: last_end,
                score: 0.0,
            }),
        }
    }
    Some(out)
}

/// Cheap 44.1 kHz → 16 kHz conversion for the aligner: a windowed-sinc
/// low-pass at 7 kHz followed by linear interpolation.
pub fn to_16k(mono44k: &[f32]) -> Vec<f32> {
    const TAPS: usize = 47;
    const CUTOFF: f32 = 7_000.0 / 44_100.0;
    let half = TAPS / 2;
    let kernel: Vec<f32> = (0..TAPS)
        .map(|i| {
            let x = i as f32 - half as f32;
            let sinc = if x == 0.0 {
                2.0 * CUTOFF
            } else {
                (2.0 * std::f32::consts::PI * CUTOFF * x).sin() / (std::f32::consts::PI * x)
            };
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (TAPS - 1) as f32).cos();
            sinc * w
        })
        .collect();
    let gain: f32 = kernel.iter().sum();
    let n = mono44k.len();
    let lowpassed: Vec<f32> = (0..n)
        .map(|i| {
            let mut acc = 0.0;
            for (k, w) in kernel.iter().enumerate() {
                let j = i as isize + k as isize - half as isize;
                if j >= 0 && (j as usize) < n {
                    acc += w * mono44k[j as usize];
                }
            }
            acc / gain
        })
        .collect();
    let ratio = 44_100.0 / ALIGN_RATE as f64;
    let out_len = (n as f64 / ratio) as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let j = pos as usize;
            let frac = (pos - j as f64) as f32;
            let a = lowpassed[j];
            let b = lowpassed[(j + 1).min(n - 1)];
            a + frac * (b - a)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic log-probs: each frame strongly prefers one token.
    fn probs(seq: &[usize]) -> Vec<Vec<f32>> {
        seq.iter()
            .map(|&tok| {
                let mut row = vec![-8.0f32; 32];
                row[tok] = -0.05;
                row
            })
            .collect()
    }

    #[test]
    fn aligns_two_words_to_their_frames() {
        // "HI | ON" with blanks/repeats: frames 0-1 blank, H H I, blank, |, O N N, blank
        let seq = [0, 0, 11, 11, 10, 0, 4, 8, 9, 9, 0];
        let spans = forced_align(&probs(&seq), &["hi".into(), "on".into()]).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].start_ms, 40); // frame 2
        assert_eq!(spans[0].end_ms, 100); // frame 4 + 1
        assert_eq!(spans[1].start_ms, 140); // frame 7
        assert_eq!(spans[1].end_ms, 200); // frame 9 + 1
        assert!(spans.iter().all(|s| s.score > -0.1));
    }

    #[test]
    fn rejects_text_longer_than_audio() {
        assert!(forced_align(&probs(&[0, 5]), &["elephant".into()]).is_none());
    }

    #[test]
    fn resampler_keeps_a_low_tone() {
        let n = 44_100;
        let tone: Vec<f32> = (0..n)
            .map(|i| (i as f32 / 44_100.0 * 440.0 * std::f32::consts::TAU).sin())
            .collect();
        let out = to_16k(&tone);
        assert!((out.len() as i64 - 16_000).abs() <= 2);
        let rms = (out.iter().map(|x| x * x).sum::<f32>() / out.len() as f32).sqrt();
        assert!((rms - 0.707).abs() < 0.05, "rms {rms}");
    }
}

/// Background word alignment of whole tracks from their stem buffers.
pub mod worker {
    use std::collections::VecDeque;
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use stems_core::{LyricLine, LyricWord, TrackId, frames_to_ms, ms_to_frames};

    use super::{Aligner, to_16k};
    use crate::offline::StemBuffer;

    /// One line's alignment result.
    pub struct Aligned {
        pub track: TrackId,
        pub line: usize,
        pub words: Vec<LyricWord>,
    }

    /// Receives each aligned line; called from the worker thread.
    pub type OnAligned = Box<dyn Fn(Aligned) + Send>;

    struct Job {
        track: TrackId,
        buf: Arc<StemBuffer>,
        lines: Vec<LyricLine>,
    }

    #[derive(Default)]
    struct Queue {
        jobs: VecDeque<Job>,
        /// Tracks worth aligning (playing + upcoming); anything else is dropped.
        keep: Vec<TrackId>,
        closed: bool,
    }

    /// Aligns tracks one at a time in submission order, sending word
    /// timings back one line at a time. Lines are grouped into chunks of
    /// up to [`CHUNK_MS`] so a line's own timestamp only has to be roughly
    /// right — the model places the words within the chunk.
    pub struct AlignWorker {
        queue: Arc<(Mutex<Queue>, Condvar)>,
        thread: Option<JoinHandle<()>>,
    }

    /// Audio before the first line of a chunk and after the next chunk's first line.
    const PRE_MS: u64 = 1_500;
    const POST_MS: u64 = 1_000;
    /// Target chunk length handed to the model.
    const CHUNK_MS: u64 = 20_000;
    /// Mean letter log-probability below which a line is left unaligned.
    /// Rejected lines in field logs sat at −3.1…−5.4; the ones just under
    /// −3 were usually harmonies or effected vocals that still placed well.
    const MIN_SCORE: f32 = -3.5;

    impl AlignWorker {
        pub fn new(aligner: Aligner, results: OnAligned) -> Self {
            let queue: Arc<(Mutex<Queue>, Condvar)> = Arc::default();
            let q = Arc::clone(&queue);
            let thread = thread::Builder::new()
                .name("stems-align".into())
                .spawn(move || {
                    let mut aligner = aligner;
                    loop {
                        let job = {
                            let (lock, cv) = &*q;
                            let mut g = lock.lock().expect("align queue");
                            loop {
                                if let Some(j) = g.jobs.pop_front() {
                                    break j;
                                }
                                if g.closed {
                                    return;
                                }
                                g = cv.wait(g).expect("align queue");
                            }
                        };
                        run_job(&mut aligner, &job, &q, &results);
                    }
                })
                .expect("spawn align worker");
            Self {
                queue,
                thread: Some(thread),
            }
        }

        /// Queue `track` for alignment (and keep it).
        pub fn submit(&self, track: TrackId, buf: Arc<StemBuffer>, lines: Vec<LyricLine>) {
            let (lock, cv) = &*self.queue;
            let mut g = lock.lock().expect("align queue");
            if !g.keep.contains(&track) {
                g.keep.push(track.clone());
            }
            g.jobs.retain(|j| j.track != track);
            g.jobs.push_back(Job { track, buf, lines });
            cv.notify_one();
        }

        /// Drop queued work and abort the running job for every track not
        /// in `keep`.
        pub fn focus(&self, keep: &[&TrackId]) {
            let (lock, _) = &*self.queue;
            let mut g = lock.lock().expect("align queue");
            g.keep = keep.iter().map(|t| (*t).clone()).collect();
            g.jobs.retain(|j| keep.contains(&&j.track));
        }
    }

    /// Align a whole track on the calling thread and return every line
    /// that scored well enough. `buf` should be complete (offline import);
    /// regions without stems are skipped rather than waited for.
    pub fn align_blocking(
        aligner: &mut Aligner,
        track: TrackId,
        buf: Arc<StemBuffer>,
        lines: Vec<LyricLine>,
    ) -> anyhow::Result<Vec<Aligned>> {
        let job = Job { track, buf, lines };
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&out);
        let results: OnAligned = Box::new(move |a| sink.lock().expect("aligned").push(a));
        for chunk in plan(&job) {
            let Some(vocals) = job.buf.read_vocals_mono(chunk.frames.0, chunk.frames.1) else {
                continue;
            };
            align_chunk(aligner, &job, &chunk, &vocals, &results)?;
        }
        drop(results);
        let mut v = std::mem::take(&mut *out.lock().expect("aligned"));
        v.sort_by_key(|a| a.line);
        Ok(v)
    }

    impl Drop for AlignWorker {
        fn drop(&mut self) {
            let (lock, cv) = &*self.queue;
            if let Ok(mut g) = lock.lock() {
                g.closed = true;
                g.keep.clear();
            }
            cv.notify_all();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn wanted(q: &Arc<(Mutex<Queue>, Condvar)>, track: &TrackId) -> bool {
        let (lock, _) = &**q;
        lock.lock().map(|g| g.keep.contains(track)).unwrap_or(false)
    }

    /// Consecutive lines grouped so each group spans about `CHUNK_MS`.
    fn chunks(lines: &[LyricLine]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut start = 0;
        while start < lines.len() {
            let t0 = lines[start].time_ms;
            let mut end = start + 1;
            while end < lines.len() && lines[end].time_ms.saturating_sub(t0) < CHUNK_MS {
                end += 1;
            }
            out.push((start, end));
            start = end;
        }
        out
    }

    /// One chunk of consecutive lines with the audio span it is aligned in.
    struct Chunk {
        lines: (usize, usize),
        words: Vec<String>,
        /// line index each word belongs to
        owner: Vec<usize>,
        start_ms: u64,
        frames: (usize, usize),
    }

    fn plan(job: &Job) -> Vec<Chunk> {
        let total_ms = frames_to_ms(job.buf.total_frames() as u64);
        let mut out = Vec::new();
        for (a, b) in chunks(&job.lines) {
            let mut words: Vec<String> = Vec::new();
            let mut owner: Vec<usize> = Vec::new();
            for (i, line) in job.lines[a..b].iter().enumerate() {
                for w in line.text.split_whitespace() {
                    words.push(w.to_string());
                    owner.push(a + i);
                }
            }
            if words.is_empty() {
                continue;
            }
            let next = job.lines.get(b).map(|l| l.time_ms).unwrap_or(total_ms);
            let start = job.lines[a].time_ms.saturating_sub(PRE_MS);
            let end = (next + POST_MS).min(total_ms);
            if end <= start + 500 {
                continue;
            }
            out.push(Chunk {
                lines: (a, b),
                words,
                owner,
                start_ms: start,
                frames: (ms_to_frames(start) as usize, ms_to_frames(end) as usize),
            });
        }
        out
    }

    /// Align every chunk whose stems exist, in whatever order they become
    /// available (the stems job starts at the playhead, so the opening
    /// chunk may be the last to land), until all are done or the track is
    /// no longer wanted.
    fn run_job(
        aligner: &mut Aligner,
        job: &Job,
        q: &Arc<(Mutex<Queue>, Condvar)>,
        results: &OnAligned,
    ) {
        let n = job.lines.len();
        let mut aligned = 0usize;
        let mut pending = plan(job);
        while !pending.is_empty() {
            if !wanted(q, &job.track) || job.buf.is_cancelled() {
                return;
            }
            let Some(i) = pending
                .iter()
                .position(|c| job.buf.is_covered(c.frames.0, c.frames.1))
            else {
                thread::sleep(Duration::from_millis(200));
                continue;
            };
            let chunk = pending.remove(i);
            let Some(vocals) = job.buf.read_vocals_mono(chunk.frames.0, chunk.frames.1) else {
                continue;
            };
            match align_chunk(aligner, job, &chunk, &vocals, results) {
                Ok(k) => aligned += k,
                Err(e) => {
                    tracing::warn!("alignment failed: {e:#}");
                    return;
                }
            }
        }
        tracing::info!(track = ?job.track, lines = n, aligned, "lyrics aligned to vocals");
    }

    /// Align one chunk and hand back each line whose words scored well
    /// enough. Returns how many lines were accepted.
    fn align_chunk(
        aligner: &mut Aligner,
        job: &Job,
        chunk: &Chunk,
        vocals: &[f32],
        results: &OnAligned,
    ) -> anyhow::Result<usize> {
        let (a, b) = chunk.lines;
        let audio = to_16k(vocals);
        let Some(spans) = aligner.align(&audio, &chunk.words)? else {
            tracing::debug!(lines = ?(a, b), "chunk not alignable");
            return Ok(0);
        };
        let mut accepted = 0;
        for li in a..b {
            let idx: Vec<usize> = (0..chunk.words.len())
                .filter(|k| chunk.owner[*k] == li)
                .collect();
            if idx.is_empty() {
                continue;
            }
            let mean = idx.iter().map(|k| spans[*k].score).sum::<f32>() / idx.len() as f32;
            if mean < MIN_SCORE {
                tracing::debug!(line = li, score = format!("{mean:.2}"), "line not aligned");
                continue;
            }
            let words_out: Vec<LyricWord> = idx
                .iter()
                .map(|k| LyricWord {
                    time_ms: chunk.start_ms + spans[*k].start_ms as u64,
                    text: chunk.words[*k].clone(),
                })
                .collect();
            accepted += 1;
            results(Aligned {
                track: job.track.clone(),
                line: li,
                words: words_out,
            });
        }
        Ok(accepted)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn chunks_group_lines_by_duration() {
            let lines: Vec<LyricLine> = (0..10)
                .map(|i| LyricLine {
                    time_ms: i * 6_000,
                    text: "la".into(),
                    words: vec![],
                })
                .collect();
            assert_eq!(chunks(&lines), vec![(0, 4), (4, 8), (8, 10)]);
        }
    }
}
