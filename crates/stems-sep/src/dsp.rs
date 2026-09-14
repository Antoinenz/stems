//! Centre-channel cancellation ("out-of-phase" karaoke).
//!
//! Vocals are usually mixed dead-centre, so `L - R` removes them — along
//! with everything else in the centre (bass, kick, snare). To keep the low
//! end we split the mid signal with a one-pole low-pass and add the bass
//! part back in. Quality is what it is; it exists so the whole pipeline
//! (toggle, crossfade, remote) works before any model is loaded.

use stems_core::SAMPLE_RATE;

use stems_core::Separator;

pub struct CentreCancel {
    /// Low-pass cutoff for the mid signal that is kept, Hz.
    bass_cutoff_hz: f32,
    alpha: f32,
    lp_state: f32,
}

impl Default for CentreCancel {
    fn default() -> Self {
        Self::new(180.0)
    }
}

impl CentreCancel {
    pub fn new(bass_cutoff_hz: f32) -> Self {
        let dt = 1.0 / SAMPLE_RATE as f32;
        let rc = 1.0 / (2.0 * std::f32::consts::PI * bass_cutoff_hz);
        Self {
            bass_cutoff_hz,
            alpha: dt / (rc + dt),
            lp_state: 0.0,
        }
    }

    pub fn bass_cutoff_hz(&self) -> f32 {
        self.bass_cutoff_hz
    }
}

impl Separator for CentreCancel {
    fn name(&self) -> &'static str {
        "dsp"
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn process(&mut self, input: &[f32], instrumental: &mut [f32]) {
        debug_assert_eq!(input.len(), instrumental.len());
        for (i, o) in input.chunks_exact(2).zip(instrumental.chunks_exact_mut(2)) {
            let (l, r) = (i[0], i[1]);
            let mid = 0.5 * (l + r);
            let side = 0.5 * (l - r);
            self.lp_state += self.alpha * (mid - self.lp_state);
            let bass = self.lp_state;
            // side keeps the stereo width; bass restores the centre lows
            o[0] = side + bass;
            o[1] = -side + bass;
        }
    }

    fn reset(&mut self) {
        self.lp_state = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(s: &[f32]) -> f32 {
        (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
    }

    #[test]
    fn removes_centred_high_tone_keeps_wide_signal() {
        let n = 44_100;
        let mut input = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f32 / SAMPLE_RATE as f32;
            let vocal = (t * 1000.0 * std::f32::consts::TAU).sin() * 0.5; // centred 1 kHz
            let wide = (t * 300.0 * std::f32::consts::TAU).sin() * 0.3; // left only
            input.push(vocal + wide);
            input.push(vocal);
        }
        let mut out = vec![0.0; input.len()];
        CentreCancel::default().process(&input, &mut out);
        let input_rms = rms(&input);
        let out_rms = rms(&out);
        assert!(out_rms < input_rms * 0.6, "in {input_rms} out {out_rms}");
        assert!(out_rms > 0.05, "wide content should survive: {out_rms}");
    }

    #[test]
    fn keeps_centred_bass() {
        let n = 44_100;
        let input: Vec<f32> = (0..n)
            .flat_map(|i| {
                let v = (i as f32 / SAMPLE_RATE as f32 * 60.0 * std::f32::consts::TAU).sin() * 0.5;
                [v, v]
            })
            .collect();
        let mut out = vec![0.0; input.len()];
        CentreCancel::default().process(&input, &mut out);
        assert!(rms(&out) > rms(&input) * 0.8, "bass should be kept");
    }
}
