//! STFT / iSTFT matching `torch.stft(center=True, pad_mode="reflect")` with a
//! periodic Hann window, as used by the MDX-Net (UVR) models.

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::sync::Arc;

pub struct Stft {
    n_fft: usize,
    hop: usize,
    window: Vec<f32>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    scratch_in: Vec<f32>,
    scratch_out: Vec<Complex32>,
}

impl Stft {
    pub fn new(n_fft: usize, hop: usize) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let window = (0..n_fft)
            .map(|n| 0.5 - 0.5 * (std::f32::consts::TAU * n as f32 / n_fft as f32).cos())
            .collect();
        Self {
            n_fft,
            hop,
            window,
            fwd: planner.plan_fft_forward(n_fft),
            inv: planner.plan_fft_inverse(n_fft),
            scratch_in: vec![0.0; n_fft],
            scratch_out: vec![Complex32::default(); n_fft / 2 + 1],
        }
    }

    pub fn n_bins(&self) -> usize {
        self.n_fft / 2 + 1
    }

    /// Number of frames `forward` produces for `len` samples (centre padded).
    pub fn frames_for(&self, len: usize) -> usize {
        1 + len / self.hop
    }

    /// Forward transform of one channel. Returns `frames × n_bins` complex
    /// values, frame-major.
    pub fn forward(&mut self, x: &[f32]) -> Vec<Complex32> {
        let half = self.n_fft / 2;
        let frames = self.frames_for(x.len());
        let n_bins = self.n_bins();
        let padded = reflect_pad(x, half);
        let mut out = vec![Complex32::default(); frames * n_bins];
        for t in 0..frames {
            let start = t * self.hop;
            for (i, v) in self.scratch_in.iter_mut().enumerate() {
                *v = padded.get(start + i).copied().unwrap_or(0.0) * self.window[i];
            }
            self.fwd
                .process(&mut self.scratch_in, &mut self.scratch_out)
                .expect("fft sizes match");
            out[t * n_bins..(t + 1) * n_bins].copy_from_slice(&self.scratch_out);
        }
        out
    }

    /// Inverse of [`forward`](Self::forward): `frames × n_bins` → `len` samples.
    pub fn inverse(&mut self, spec: &[Complex32], len: usize) -> Vec<f32> {
        let half = self.n_fft / 2;
        let n_bins = self.n_bins();
        let frames = spec.len() / n_bins;
        let total = (frames - 1) * self.hop + self.n_fft;
        let mut acc = vec![0.0f32; total];
        let mut norm = vec![0.0f32; total];
        let mut frame = vec![0.0f32; self.n_fft];
        let scale = 1.0 / self.n_fft as f32;
        for t in 0..frames {
            self.scratch_out
                .copy_from_slice(&spec[t * n_bins..(t + 1) * n_bins]);
            // realfft requires imag of DC and Nyquist to be zero
            self.scratch_out[0].im = 0.0;
            self.scratch_out[n_bins - 1].im = 0.0;
            self.inv
                .process(&mut self.scratch_out, &mut frame)
                .expect("fft sizes match");
            let start = t * self.hop;
            for i in 0..self.n_fft {
                let w = self.window[i];
                acc[start + i] += frame[i] * scale * w;
                norm[start + i] += w * w;
            }
        }
        let mut out = vec![0.0f32; len];
        for (i, o) in out.iter_mut().enumerate() {
            let j = i + half;
            if j < total && norm[j] > 1e-8 {
                *o = acc[j] / norm[j];
            }
        }
        out
    }
}

fn reflect_pad(x: &[f32], pad: usize) -> Vec<f32> {
    let n = x.len();
    let mut out = Vec::with_capacity(n + 2 * pad);
    for i in 0..pad {
        let idx = pad - i;
        out.push(if idx < n { x[idx] } else { 0.0 });
    }
    out.extend_from_slice(x);
    for i in 0..pad {
        let idx = n.saturating_sub(2 + i);
        out.push(if n >= 2 && idx < n { x[idx] } else { 0.0 });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_reconstructs_signal() {
        let n_fft = 6144;
        let hop = 1024;
        let len = hop * 255;
        let x: Vec<f32> = (0..len)
            .map(|i| {
                let t = i as f32 / 44_100.0;
                (t * 440.0 * std::f32::consts::TAU).sin() * 0.5
                    + (t * 3_000.0 * std::f32::consts::TAU).sin() * 0.2
            })
            .collect();
        let mut stft = Stft::new(n_fft, hop);
        let spec = stft.forward(&x);
        assert_eq!(spec.len(), 256 * stft.n_bins());
        let y = stft.inverse(&spec, len);
        // ignore the outermost n_fft/2 samples where the window sum is low
        let m = n_fft / 2;
        let err = x[m..len - m]
            .iter()
            .zip(&y[m..len - m])
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 1e-3, "max err {err}");
    }
}
