//! Output mixer: master volume plus a click-free crossfade between the
//! original signal and a processed (karaoke) signal.
//!
//! Phase 1 has no real separator yet, so the "processed" input is optional;
//! when absent the mixer simply applies volume. The crossfade machinery is
//! here now so the hot-path shape does not change when separation lands.

use stems_core::{CHANNELS, SAMPLE_RATE};

/// Default crossfade length when toggling karaoke: 50 ms.
pub const DEFAULT_FADE_MS: f32 = 50.0;

#[derive(Debug, Clone)]
pub struct Mixer {
    volume: f32,
    /// 0.0 = original signal, 1.0 = processed signal.
    mix: f32,
    target_mix: f32,
    /// Per-frame step for `mix` while fading.
    step: f32,
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer {
    pub fn new() -> Self {
        Self {
            volume: 1.0,
            mix: 0.0,
            target_mix: 0.0,
            step: 1.0 / (DEFAULT_FADE_MS / 1000.0 * SAMPLE_RATE as f32),
        }
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Start fading towards the processed signal (`true`) or the original (`false`).
    pub fn set_processed(&mut self, on: bool) {
        self.target_mix = if on { 1.0 } else { 0.0 };
    }

    /// Current blend, 0.0 (original) ..= 1.0 (processed).
    pub fn mix(&self) -> f32 {
        self.mix
    }

    pub fn is_fading(&self) -> bool {
        self.mix != self.target_mix
    }

    /// Mix `original` and optional `processed` into `out`, all interleaved
    /// stereo and equal length. `processed == None` means "no separator
    /// running": the fade still advances but only volume is applied.
    pub fn process(&mut self, original: &[f32], processed: Option<&[f32]>, out: &mut [f32]) {
        debug_assert_eq!(original.len(), out.len());
        let ch = CHANNELS as usize;
        match processed {
            None => {
                for (o, i) in out.iter_mut().zip(original) {
                    *o = i * self.volume;
                }
                // keep the fade state coherent for when a processor appears
                for _ in 0..original.len() / ch {
                    self.advance();
                }
            }
            Some(p) => {
                debug_assert_eq!(p.len(), out.len());
                for frame in 0..original.len() / ch {
                    let m = self.mix;
                    for c in 0..ch {
                        let i = frame * ch + c;
                        out[i] = (original[i] * (1.0 - m) + p[i] * m) * self.volume;
                    }
                    self.advance();
                }
            }
        }
    }

    #[inline]
    fn advance(&mut self) {
        if self.mix < self.target_mix {
            self.mix = (self.mix + self.step).min(self.target_mix);
        } else if self.mix > self.target_mix {
            self.mix = (self.mix - self.step).max(self.target_mix);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_only_when_no_processor() {
        let mut m = Mixer::new();
        m.set_volume(0.5);
        let input = [1.0, -1.0, 0.5, -0.5];
        let mut out = [0.0; 4];
        m.process(&input, None, &mut out);
        assert_eq!(out, [0.5, -0.5, 0.25, -0.25]);
    }

    #[test]
    fn crossfade_is_monotonic_and_completes() {
        let mut m = Mixer::new();
        m.set_processed(true);
        let frames = (DEFAULT_FADE_MS / 1000.0 * SAMPLE_RATE as f32) as usize + 10;
        let original = vec![1.0; frames * 2];
        let processed = vec![0.0; frames * 2];
        let mut out = vec![0.0; frames * 2];
        m.process(&original, Some(&processed), &mut out);
        // left channel samples must decrease monotonically from 1 towards 0
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!(left.windows(2).all(|w| w[1] <= w[0]));
        assert_eq!(*left.last().unwrap(), 0.0);
        assert!(!m.is_fading());
        assert_eq!(m.mix(), 1.0);
    }

    #[test]
    fn fade_back_to_original() {
        let mut m = Mixer::new();
        m.set_processed(true);
        let n = 44_100 * 2;
        let mut out = vec![0.0; n];
        m.process(&vec![0.0; n], Some(&vec![0.0; n]), &mut out);
        assert_eq!(m.mix(), 1.0);
        m.set_processed(false);
        m.process(&vec![0.0; n], Some(&vec![0.0; n]), &mut out);
        assert_eq!(m.mix(), 0.0);
    }
}
