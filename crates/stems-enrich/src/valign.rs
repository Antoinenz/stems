//! Put a downloaded music video on the track's timeline by matching its
//! soundtrack to the track's audio. Both are reduced to a coarse loudness
//! envelope (`ENV_HZ` bins per second), whitened so only *changes* in
//! loudness count, and cross-correlated over a bounded range of lags.
//! Same recording ⇒ a sharp, high peak; a different performance (live,
//! remix, wrong video) ⇒ a low, flat maximum, which the caller rejects.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};

/// Envelope resolution. 50 Hz = 20 ms bins: finer than a video frame.
pub const ENV_HZ: u32 = 50;

/// Below this the best lag is not a match (unrelated material scores
/// ≈0.05–0.15; the same recording through YouTube's encoder ≥0.5).
pub const MIN_CORR: f32 = 0.3;

/// Decode a media file's audio to mono f32 at `rate` with ffmpeg.
pub fn decode_audio(path: &Path, rate: u32) -> Result<Vec<f32>> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-vn",
            "-ac",
            "1",
            "-ar",
            &rate.to_string(),
            "-f",
            "f32le",
            "-",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("run ffmpeg (is it installed?)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "ffmpeg exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

/// RMS loudness per `1/ENV_HZ` s of mono audio at `rate`.
pub fn envelope(pcm: &[f32], rate: u32) -> Vec<f32> {
    let hop = (rate / ENV_HZ).max(1) as usize;
    pcm.chunks(hop)
        .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
        .collect()
}

/// Log loudness minus its 1 s running mean: level-independent, and
/// sustained notes stop dominating the correlation.
fn whiten(env: &[f32]) -> Vec<f32> {
    let l: Vec<f32> = env.iter().map(|e| (e + 1e-4).ln()).collect();
    let mut prefix = Vec::with_capacity(l.len() + 1);
    prefix.push(0.0f64);
    for v in &l {
        let last = *prefix.last().unwrap();
        prefix.push(last + *v as f64);
    }
    let half = (ENV_HZ / 2) as usize;
    (0..l.len())
        .map(|i| {
            let a = i.saturating_sub(half);
            let b = (i + half + 1).min(l.len());
            let mean = (prefix[b] - prefix[a]) / (b - a) as f64;
            l[i] - mean as f32
        })
        .collect()
}

/// Lag (ms) to add to a track position to get the video time, and the
/// normalised correlation at that lag. `None` when there is not enough
/// overlap to say anything.
pub fn align(track_env: &[f32], video_env: &[f32], max_lag_s: f32) -> Option<(i64, f32)> {
    let a = whiten(track_env);
    let b = whiten(video_env);
    let max_lag = (max_lag_s * ENV_HZ as f32) as i64;
    let min_overlap = 20 * ENV_HZ as usize;
    let mut best: Option<(i64, f32)> = None;
    for lag in -max_lag..=max_lag {
        // a[i] pairs with b[i + lag]
        let i0 = (-lag).max(0) as usize;
        let i1 = (b.len() as i64 - lag).min(a.len() as i64).max(0) as usize;
        if i1 <= i0 || i1 - i0 < min_overlap {
            continue;
        }
        let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
        for i in i0..i1 {
            let x = a[i] as f64;
            let y = b[(i as i64 + lag) as usize] as f64;
            ab += x * y;
            aa += x * x;
            bb += y * y;
        }
        let corr = (ab / (aa * bb).sqrt().max(1e-9)) as f32;
        if best.is_none_or(|(_, c)| corr > c) {
            best = Some((lag, corr));
        }
    }
    best.map(|(lag, c)| (lag * 1000 / ENV_HZ as i64, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                0.2 + (s % 1000) as f32 / 1000.0
            })
            .collect()
    }

    #[test]
    fn finds_positive_lag_for_video_with_intro() {
        // 60 s of track; the video has a 7.5 s intro before the same audio
        let track = noise(60 * ENV_HZ as usize, 42);
        let mut video = noise((7.5 * ENV_HZ as f32) as usize, 7);
        video.extend_from_slice(&track);
        let (lag_ms, corr) = align(&track, &video, 30.0).unwrap();
        assert_eq!(lag_ms, 7500);
        assert!(corr > 0.9, "corr {corr}");
    }

    #[test]
    fn finds_negative_lag_for_trimmed_video() {
        // the video starts 3 s into the track
        let track = noise(60 * ENV_HZ as usize, 42);
        let video = track[3 * ENV_HZ as usize..].to_vec();
        let (lag_ms, corr) = align(&track, &video, 30.0).unwrap();
        assert_eq!(lag_ms, -3000);
        assert!(corr > 0.9, "corr {corr}");
    }

    #[test]
    fn unrelated_audio_scores_low() {
        let track = noise(60 * ENV_HZ as usize, 42);
        let video = noise(60 * ENV_HZ as usize, 99);
        let (_, corr) = align(&track, &video, 30.0).unwrap();
        assert!(corr < MIN_CORR, "corr {corr}");
    }
}
