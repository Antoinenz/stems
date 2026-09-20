//! On-disk cache of finished instrumentals so a track that has been
//! separated once never costs the GPU again. Plain 16-bit stereo WAV
//! files, keyed by source id, pruned oldest-first to a size cap.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use stems_core::{CHANNELS, SAMPLE_RATE, TrackId};

pub struct StemDisk {
    dir: PathBuf,
    cap_bytes: u64,
}

impl StemDisk {
    pub fn new(dir: impl Into<PathBuf>, cap_bytes: u64) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        Ok(Self { dir, cap_bytes })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Cache file for `id` (safe on every filesystem).
    pub fn path(&self, id: &TrackId) -> PathBuf {
        let key = match id {
            TrackId::Spotify(s) => format!("spotify_{s}"),
            TrackId::Local(p) => format!("local_{}", p.display()),
        };
        let key: String = key
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        self.dir.join(format!("{key}.wav"))
    }

    /// Interleaved stereo f32 instrumental, if cached. A file that does
    /// not decode as expected is treated as absent (and removed).
    pub fn load(&self, id: &TrackId) -> Option<Vec<f32>> {
        let path = self.path(id);
        if !path.is_file() {
            return None;
        }
        match read_wav(&path) {
            Ok(pcm) => {
                // touch so the LRU prune keeps recently played tracks
                let _ = fs::File::open(&path).and_then(|f| f.set_modified(SystemTime::now()));
                Some(pcm)
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), "bad stem cache file: {e:#}");
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    /// Write `pcm` (interleaved stereo f32) for `id`, then prune.
    pub fn save(&self, id: &TrackId, pcm: &[f32]) -> Result<()> {
        let path = self.path(id);
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
        fs::rename(&tmp, &path)?;
        self.prune();
        Ok(())
    }

    /// Delete the oldest files until the cache fits `cap_bytes`.
    fn prune(&self) {
        let Ok(rd) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "wav") {
                    let m = e.metadata().ok()?;
                    Some((m.modified().ok()?, m.len(), p))
                } else {
                    None
                }
            })
            .collect();
        files.sort_by_key(|f| std::cmp::Reverse(f.0));
        let mut total = 0u64;
        for (_, len, p) in files {
            total += len;
            if total > self.cap_bytes && fs::remove_file(&p).is_ok() {
                tracing::info!(path = %p.display(), "stem cache evicted");
            }
        }
    }
}

fn read_wav(path: &Path) -> Result<Vec<f32>> {
    let mut r = hound::WavReader::open(path)?;
    let spec = r.spec();
    if spec.channels != CHANNELS || spec.sample_rate != SAMPLE_RATE {
        bail!(
            "unexpected format {}ch {}Hz",
            spec.channels,
            spec.sample_rate
        );
    }
    let pcm: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => r
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / 32768.0))
            .collect::<std::result::Result<_, _>>()?,
        (hound::SampleFormat::Float, 32) => {
            r.samples::<f32>().collect::<std::result::Result<_, _>>()?
        }
        (f, b) => bail!("unexpected sample format {f:?}/{b}"),
    };
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_prune() {
        let dir = std::env::temp_dir().join(format!("stems-disk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let pcm: Vec<f32> = (0..44_100 * 2)
            .map(|i| ((i % 200) as f32 / 100.0) - 1.0)
            .collect();
        // cap: a bit more than one file (1 s × 2 ch × 2 B + header)
        let disk = StemDisk::new(&dir, 44_100 * 4 + 1024).unwrap();
        let a = TrackId::Spotify("aaa".into());
        let b = TrackId::Spotify("bbb".into());
        disk.save(&a, &pcm).unwrap();
        let back = disk.load(&a).unwrap();
        assert_eq!(back.len(), pcm.len());
        let err = back
            .iter()
            .zip(&pcm)
            .map(|(x, y)| (x - y).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 1e-3, "max err {err}");
        // second file pushes the first out
        std::thread::sleep(std::time::Duration::from_millis(20));
        disk.save(&b, &pcm).unwrap();
        assert!(disk.load(&b).is_some());
        assert!(disk.load(&a).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
