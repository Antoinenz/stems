//! Streaming sample-rate conversion to the internal 44.1 kHz, built on
//! [`rubato`]'s asynchronous polynomial resampler (cubic; no optional
//! features, works on every target).

use anyhow::Context;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Indexing, PolynomialDegree, Resampler};
use stems_core::{CHANNELS, SAMPLE_RATE};

const CHUNK_FRAMES: usize = 1024;

/// Converts interleaved stereo f32 from an arbitrary input rate to
/// [`SAMPLE_RATE`]. Feed any number of frames with [`push`](Self::push);
/// resampled output is appended to the `out` vector you pass in.
pub struct StreamResampler {
    inner: Option<Async<f32>>,
    pending: Vec<f32>,
    scratch: Vec<f32>,
}

impl StreamResampler {
    pub fn new(in_rate: u32) -> anyhow::Result<Self> {
        if in_rate == SAMPLE_RATE {
            return Ok(Self {
                inner: None,
                pending: Vec::new(),
                scratch: Vec::new(),
            });
        }
        let ratio = SAMPLE_RATE as f64 / in_rate as f64;
        let inner = Async::<f32>::new_poly(
            ratio,
            1.1,
            PolynomialDegree::Cubic,
            CHUNK_FRAMES,
            CHANNELS as usize,
            FixedAsync::Input,
        )
        .with_context(|| format!("create resampler {in_rate} -> {SAMPLE_RATE}"))?;
        let scratch = vec![0.0; inner.output_frames_max() * CHANNELS as usize];
        Ok(Self {
            inner: Some(inner),
            pending: Vec::new(),
            scratch,
        })
    }

    pub fn is_identity(&self) -> bool {
        self.inner.is_none()
    }

    /// Push interleaved stereo input; resampled frames are appended to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> anyhow::Result<()> {
        let Some(rs) = self.inner.as_mut() else {
            out.extend_from_slice(input);
            return Ok(());
        };
        let ch = CHANNELS as usize;
        self.pending.extend_from_slice(input);
        while self.pending.len() >= CHUNK_FRAMES * ch {
            let inp = InterleavedSlice::new(&self.pending[..CHUNK_FRAMES * ch], ch, CHUNK_FRAMES)
                .context("input adapter")?;
            let cap = self.scratch.len() / ch;
            let mut outp =
                InterleavedSlice::new_mut(&mut self.scratch, ch, cap).context("output adapter")?;
            let (_read, written) = rs
                .process_into_buffer(&inp, &mut outp, Some(&Indexing::new()))
                .context("resample")?;
            out.extend_from_slice(&self.scratch[..written * ch]);
            self.pending.drain(..CHUNK_FRAMES * ch);
        }
        Ok(())
    }

    /// Flush the trailing partial chunk (end of stream).
    pub fn finish(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        let Some(rs) = self.inner.as_mut() else {
            return Ok(());
        };
        let ch = CHANNELS as usize;
        let partial = self.pending.len() / ch;
        if partial == 0 {
            return Ok(());
        }
        self.pending.resize(CHUNK_FRAMES * ch, 0.0);
        let inp =
            InterleavedSlice::new(&self.pending, ch, CHUNK_FRAMES).context("input adapter")?;
        let cap = self.scratch.len() / ch;
        let mut outp =
            InterleavedSlice::new_mut(&mut self.scratch, ch, cap).context("output adapter")?;
        let (_read, written) = rs
            .process_into_buffer(&inp, &mut outp, Some(&Indexing::new().partial_len(partial)))
            .context("resample tail")?;
        out.extend_from_slice(&self.scratch[..written * ch]);
        self.pending.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_passthrough() {
        let mut r = StreamResampler::new(SAMPLE_RATE).unwrap();
        let mut out = Vec::new();
        r.push(&[1.0, 2.0, 3.0, 4.0], &mut out).unwrap();
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn converts_48k_to_44k1_length() {
        let mut r = StreamResampler::new(48_000).unwrap();
        let frames = 48_000; // 1 s
        let input: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin();
                [v, v]
            })
            .collect();
        let mut out = Vec::new();
        r.push(&input, &mut out).unwrap();
        r.finish(&mut out).unwrap();
        let out_frames = out.len() / 2;
        // within ~2 chunks of the ideal 44_100 (resampler delay + tail handling)
        assert!(
            (out_frames as i64 - 44_100).abs() < 2_200,
            "got {out_frames}"
        );
    }
}
