//! 16-bit stereo 44.1 kHz WAV for instrumentals: plain, seekable, readable
//! by any audio tool. 16 bits put quantisation noise at −96 dB, far below
//! what vocals = mix − instrumental can resolve.

use std::path::Path;

use anyhow::{Context, Result, bail};
use stems_core::{CHANNELS, SAMPLE_RATE};

/// Write interleaved stereo f32 as 16-bit PCM, atomically.
pub fn write_pcm16(path: &Path, pcm: &[f32]) -> Result<()> {
    let tmp = path.with_extension("wav.part");
    let spec = hound::WavSpec {
        channels: CHANNELS,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&tmp, spec)
        .with_context(|| format!("create {}", tmp.display()))?;
    {
        let mut w16 = w.get_i16_writer(pcm.len() as u32);
        for &s in pcm {
            w16.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16);
        }
        w16.flush()?;
    }
    w.finalize()?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

/// Read a stereo 44.1 kHz WAV (16-bit int or 32-bit float) as f32.
pub fn read_pcm(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
    let spec = r.spec();
    if spec.channels != CHANNELS || spec.sample_rate != SAMPLE_RATE {
        bail!(
            "{}: expected {CHANNELS}ch {SAMPLE_RATE} Hz, got {}ch {} Hz",
            path.display(),
            spec.channels,
            spec.sample_rate
        );
    }
    let pcm = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<std::result::Result<_, _>>()?,
        (hound::SampleFormat::Float, 32) => {
            r.samples::<f32>().collect::<std::result::Result<_, _>>()?
        }
        (f, b) => bail!("{}: unsupported sample format {f:?}/{b}", path.display()),
    };
    Ok(pcm)
}
