//! `[karaoke] backend = "spool"`: hand whole tracks to an external
//! separator through a folder, for models stems can't run itself (e.g.
//! Roformer on a rented GPU, via python-audio-separator) and for audio
//! analysis (tempo, key, danceability, genres).
//!
//! ```text
//! <spool>/in/<key>.wav     the decoded mix: float32 stereo 44.1 kHz, exactly
//!                          as stems plays it (so the result lines up)
//! <spool>/in/<key>.json    the job; written last, so it means "ready"
//! <spool>/work/<key>.json  the daemon moved the job here: it is on it
//! <spool>/out/<key>.flac   the instrumental, same length as the wav
//! <spool>/out/<key>.json   the result; written last, so it means "done"
//! ```
//!
//! The daemon (`farm/cloud/separator.py`) keeps its models loaded between
//! tracks. stemsd checks the instrumental's format and length before
//! taking it, and cleans up every file of a job when it is finished.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use stems_library::Analysis;

/// How long a job may wait for a daemon to pick it up.
const PICKUP: Duration = Duration::from_secs(120);
/// How long a picked-up job may take.
const RUN: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone)]
pub struct Spool {
    dir: PathBuf,
}

#[derive(Debug, Serialize)]
struct Job<'a> {
    key: &'a str,
    title: &'a str,
    artists: &'a [String],
    /// File name of the mix in `in/`.
    wav: String,
    frames: u64,
    want_instrumental: bool,
    want_analysis: bool,
}

#[derive(Debug, Deserialize)]
struct Outcome {
    #[serde(default)]
    error: Option<String>,
    /// File name of the instrumental in `out/`.
    #[serde(default)]
    instrumental: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    analysis: Option<Analysis>,
    #[serde(default)]
    analysis_error: Option<String>,
}

/// What came back for one track.
#[derive(Debug)]
pub struct SpoolResult {
    /// The instrumental, checked; the caller moves it into the library.
    pub instrumental: Option<PathBuf>,
    pub model: Option<String>,
    pub analysis: Option<Analysis>,
    pub analysis_error: Option<String>,
}

impl Spool {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        for sub in ["in", "work", "out"] {
            std::fs::create_dir_all(dir.join(sub))
                .with_context(|| format!("create {}", dir.join(sub).display()))?;
        }
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, sub: &str, name: &str) -> PathBuf {
        self.dir.join(sub).join(name)
    }

    /// Remove every file of `key`'s job.
    pub fn clean(&self, key: &str) {
        for (sub, ext) in [
            ("in", "wav"),
            ("in", "json"),
            ("work", "json"),
            ("out", "json"),
            ("out", "flac"),
        ] {
            let _ = std::fs::remove_file(self.path(sub, &format!("{key}.{ext}")));
        }
    }

    /// Run one job and wait for it. `pcm` is the decoded mix.
    pub async fn run(
        &self,
        key: &str,
        title: &str,
        artists: &[String],
        pcm: &[f32],
        want_instrumental: bool,
        want_analysis: bool,
    ) -> Result<SpoolResult> {
        self.clean(key);
        let wav = format!("{key}.wav");
        let wav_path = self.path("in", &wav);
        let frames = (pcm.len() / 2) as u64;
        {
            let pcm = pcm.to_vec();
            let path = wav_path.clone();
            tokio::task::spawn_blocking(move || write_f32_wav(&path, &pcm))
                .await
                .context("write task")??;
        }
        let job = Job {
            key,
            title,
            artists,
            wav,
            frames,
            want_instrumental,
            want_analysis,
        };
        write_json(&self.path("in", &format!("{key}.json")), &job)?;

        let queued = Instant::now();
        let mut picked: Option<Instant> = None;
        let out_json = self.path("out", &format!("{key}.json"));
        loop {
            if out_json.is_file() {
                break;
            }
            if picked.is_none() && self.path("work", &format!("{key}.json")).is_file() {
                picked = Some(Instant::now());
            }
            match picked {
                None if queued.elapsed() > PICKUP => {
                    self.clean(key);
                    bail!(
                        "no separator picked up the job in {}s (is the spool daemon running on {}?)",
                        PICKUP.as_secs(),
                        self.dir.display()
                    );
                }
                Some(t) if t.elapsed() > RUN => {
                    self.clean(key);
                    bail!(
                        "the separator took longer than {} minutes",
                        RUN.as_secs() / 60
                    );
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let text = std::fs::read_to_string(&out_json)
            .with_context(|| format!("read {}", out_json.display()))?;
        let outcome: Outcome =
            serde_json::from_str(&text).with_context(|| format!("parse {}", out_json.display()))?;
        if let Some(e) = outcome.error {
            self.clean(key);
            bail!("separator: {e}");
        }
        let instrumental = match (want_instrumental, outcome.instrumental) {
            (false, _) => None,
            (true, None) => {
                self.clean(key);
                bail!("separator returned no instrumental");
            }
            (true, Some(name)) => {
                let path = self.path("out", &name);
                let check = path.clone();
                let got =
                    tokio::task::spawn_blocking(move || stems_library::flac::read_pcm(&check))
                        .await
                        .context("read task")?;
                let got = match got {
                    Ok(p) => p,
                    Err(e) => {
                        self.clean(key);
                        return Err(e.context("instrumental from the separator"));
                    }
                };
                if got.len() != pcm.len() {
                    self.clean(key);
                    bail!(
                        "instrumental is {} frames, the mix {} (they must match exactly)",
                        got.len() / 2,
                        frames
                    );
                }
                Some(path)
            }
        };
        // the mix and job files are no longer needed; the flac is moved by
        // the caller and the rest removed then
        let _ = std::fs::remove_file(&wav_path);
        let _ = std::fs::remove_file(self.path("work", &format!("{key}.json")));
        Ok(SpoolResult {
            instrumental,
            model: outcome.model,
            analysis: outcome.analysis.filter(|_| want_analysis),
            analysis_error: outcome.analysis_error,
        })
    }
}

fn write_f32_wav(path: &Path, pcm: &[f32]) -> Result<()> {
    let tmp = path.with_extension("wav.part");
    let spec = hound::WavSpec {
        channels: stems_core::CHANNELS,
        sample_rate: stems_core::SAMPLE_RATE,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&tmp, spec)
        .with_context(|| format!("create {}", tmp.display()))?;
    for &s in pcm {
        w.write_sample(s)?;
    }
    w.finalize()?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.part");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))
}

/// Default spool location: `<cache>/spool`.
pub fn default_dir() -> Result<PathBuf> {
    Ok(crate::config::paths()?.cache_dir.join("spool"))
}

pub fn from_config(dir: &str) -> Result<Spool> {
    let dir = dir.trim();
    let dir = if dir.is_empty() {
        default_dir()?
    } else {
        crate::config::expand_home(dir)
    };
    Spool::open(dir).map_err(|e| anyhow!("spool: {e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("stems-spool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// A stand-in daemon: claims the job, "separates" by copying a FLAC of
    /// the right length, returns an analysis.
    async fn fake_daemon(dir: PathBuf, key: &str, flac: Option<&'static str>, analysis: bool) {
        let job = dir.join("in").join(format!("{key}.json"));
        while !job.is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::rename(&job, dir.join("work").join(format!("{key}.json"))).unwrap();
        let mut out = serde_json::json!({ "model": "test-model" });
        if let Some(src) = flac {
            let testdata = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../stems-library/testdata")
                .join(src);
            std::fs::copy(testdata, dir.join("out").join(format!("{key}.flac"))).unwrap();
            out["instrumental"] = format!("{key}.flac").into();
        }
        if analysis {
            out["analysis"] = serde_json::json!({ "bpm": 120.0, "danceability": 0.8 });
        }
        std::fs::write(dir.join("out").join(format!("{key}.json")), out.to_string()).unwrap();
    }

    #[tokio::test]
    async fn a_job_round_trips() {
        let dir = tmp("ok");
        let spool = Spool::open(&dir).unwrap();
        // sine.flac is 0.25 s: 11 025 frames
        let pcm = vec![0.1f32; 2 * 11_025];
        let daemon = tokio::spawn(fake_daemon(dir.clone(), "k", Some("sine.flac"), true));
        let r = spool.run("k", "T", &[], &pcm, true, true).await.unwrap();
        daemon.await.unwrap();
        let inst = r.instrumental.unwrap();
        assert!(inst.is_file());
        assert_eq!(r.model.as_deref(), Some("test-model"));
        assert_eq!(r.analysis.unwrap().bpm, Some(120.0));
        assert!(!dir.join("in/k.wav").exists(), "the mix copy is removed");
        spool.clean("k");
        assert!(!inst.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_wrong_length_is_refused() {
        let dir = tmp("len");
        let spool = Spool::open(&dir).unwrap();
        let pcm = vec![0.1f32; 2 * 10_000];
        let daemon = tokio::spawn(fake_daemon(dir.clone(), "k", Some("sine.flac"), false));
        let err = spool
            .run("k", "T", &[], &pcm, true, false)
            .await
            .unwrap_err();
        daemon.await.unwrap();
        assert!(err.to_string().contains("must match"), "{err}");
        assert!(
            !dir.join("out/k.flac").exists(),
            "a bad result is cleaned up"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn daemon_errors_are_reported() {
        let dir = tmp("err");
        let spool = Spool::open(&dir).unwrap();
        let d = dir.clone();
        let daemon = tokio::spawn(async move {
            let job = d.join("in/k.json");
            while !job.is_file() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            std::fs::write(d.join("out/k.json"), r#"{"error":"CUDA out of memory"}"#).unwrap();
        });
        let err = spool
            .run("k", "T", &[], &[0.0; 4], true, true)
            .await
            .unwrap_err();
        daemon.await.unwrap();
        assert!(err.to_string().contains("CUDA out of memory"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
