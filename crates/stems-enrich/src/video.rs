//! Music videos via `yt-dlp`: search YouTube for the track, download once
//! into the cache, serve the file from stemsd. The video's timeline
//! rarely matches the streamed track exactly (intros, different edits);
//! [`crate::valign`] measures the offset from the audio and
//! `video_offset_ms` in the state applies it (the remote can nudge it).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use stems_core::{TrackId, TrackRef};
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct VideoFetcher {
    dir: PathBuf,
    ytdlp: String,
    max_height: u32,
}

/// Cache file name for a track (safe on every filesystem). Keyed by the
/// source id rather than ISRC: the ISRC arrives after the track starts.
pub fn video_file_name(track: &TrackRef) -> String {
    let key = match &track.id {
        TrackId::Spotify(id) => format!("spotify_{id}"),
        TrackId::Local(p) => format!("local_{}", p.display()),
    };
    let key: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{key}.mp4")
}

impl VideoFetcher {
    pub fn new(dir: impl Into<PathBuf>, ytdlp: impl Into<String>, max_height: u32) -> Self {
        Self {
            dir: dir.into(),
            ytdlp: ytdlp.into(),
            max_height: max_height.max(240),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Cached file for the track, if already downloaded.
    pub fn cached(&self, track: &TrackRef) -> Option<PathBuf> {
        let p = self.dir.join(video_file_name(track));
        p.is_file().then_some(p)
    }

    /// Search query: artist and title, minus the usual noise.
    pub fn query(track: &TrackRef) -> String {
        let title = strip_noise(&track.title);
        let artist = track.artists.first().cloned().unwrap_or_default();
        format!("{artist} - {title} official video")
    }

    /// Download (or reuse) the video. Returns `Ok(None)` when yt-dlp found
    /// nothing; errors when yt-dlp is missing or fails.
    pub async fn fetch(&self, track: &TrackRef) -> Result<Option<PathBuf>> {
        if let Some(p) = self.cached(track) {
            return Ok(Some(p));
        }
        tokio::fs::create_dir_all(&self.dir)
            .await
            .with_context(|| format!("create {}", self.dir.display()))?;
        let name = video_file_name(track);
        let target = self.dir.join(&name);
        let template = self
            .dir
            .join(format!("{}.%(ext)s", name.trim_end_matches(".mp4")));
        let query = Self::query(track);
        let format = format!(
            "bv*[ext=mp4][height<={h}]+ba[ext=m4a]/b[ext=mp4][height<={h}]/b",
            h = self.max_height
        );
        tracing::info!(query, "fetching music video");
        let run = Command::new(&self.ytdlp)
            .args([
                "--no-playlist",
                "--socket-timeout",
                "30",
                "--no-progress",
                "--quiet",
                "--merge-output-format",
                "mp4",
                "-f",
                &format,
                "-o",
            ])
            .arg(&template)
            .arg(format!("ytsearch1:{query}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();
        // a stuck download (YouTube challenge, dead connection) must not
        // hold up a whole import run: give up and record the stage failed
        let output = tokio::time::timeout(DOWNLOAD_LIMIT, run)
            .await
            .map_err(|_| {
                anyhow!(
                    "yt-dlp took longer than {} minutes; gave up",
                    DOWNLOAD_LIMIT.as_secs() / 60
                )
            })?
            .with_context(|| format!("run `{}` (is yt-dlp installed?)", self.ytdlp))?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "yt-dlp exited with {}: {}",
                output.status,
                err.trim().lines().last().unwrap_or("")
            ));
        }
        if target.is_file() {
            Ok(Some(target))
        } else {
            Ok(None)
        }
    }
}

/// Longest a single music video download may take.
const DOWNLOAD_LIMIT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Drop `(feat. …)`, `- Remastered 2011`, `[Live]` etc. from a title.
pub fn strip_noise(title: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.split(" - ").next().unwrap_or(&out).to_string();
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_title_noise() {
        assert_eq!(strip_noise("90210 (feat. Kacy Hill)"), "90210");
        assert_eq!(strip_noise("Wannabe - Radio Edit"), "Wannabe");
        assert_eq!(strip_noise("Alibi [Live] (with X)  "), "Alibi");
        assert_eq!(strip_noise("Plain"), "Plain");
    }

    #[test]
    fn file_name_is_safe() {
        let t = TrackRef {
            id: TrackId::Spotify("abc".into()),
            isrc: Some("usSM1-1".into()),
            title: "x".into(),
            artists: vec![],
            album: None,
            duration_ms: 0,
            art: None,
        };
        assert_eq!(video_file_name(&t), "spotify_abc.mp4");
    }
}
