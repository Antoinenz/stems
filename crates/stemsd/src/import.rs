//! Filling the offline library: every track gets its original audio, an
//! instrumental, lyrics (word-timed when possible), a music video with
//! its measured offset and cover art, so it plays later with no internet.
//!
//! Each stage is recorded in the track's `track.json` as it finishes, so
//! an interrupted import resumes where it stopped and a failed stage is
//! retried on the next run without redoing the rest.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use stems_core::{Lyrics, TrackId, TrackRef};
use stems_enrich::{LocalLyrics, LrcLib, VideoFetcher};
use stems_library::{Entry, Library, Stage};
use stems_sep::{OfflineSeparator, StemBuffer};
use stems_source_spotify::Bitrate;
use stems_source_spotify::import::{ImportSession, Link};

use crate::config::Config;

/// Audio file types accepted for local import.
const AUDIO_EXTS: &[&str] = &["mp3", "flac", "ogg", "m4a", "aac", "wav", "opus"];
/// A separation job that makes no progress for this long has failed
/// (e.g. a remote worker that hung).
const STALL: Duration = Duration::from_secs(120);

/// Something to import: a Spotify link or a local audio file.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Spotify(Link),
    File(PathBuf),
}

/// Spotify links, audio files and folders (searched recursively) → sources.
pub fn parse_sources(args: &[String]) -> Result<Vec<Source>> {
    let mut out = Vec::new();
    for a in args {
        if let Some(link) = Link::parse(a) {
            out.push(Source::Spotify(link));
            continue;
        }
        let p = PathBuf::from(a);
        if p.is_dir() {
            let mut files = Vec::new();
            collect_audio(&p, &mut files);
            files.sort();
            out.extend(files.into_iter().map(Source::File));
        } else if p.is_file() {
            out.push(Source::File(p));
        } else {
            bail!("`{a}` is neither a Spotify track/album/playlist link nor an audio file");
        }
    }
    Ok(out)
}

fn collect_audio(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_audio(&p, out);
        } else if p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| AUDIO_EXTS.contains(&x.to_ascii_lowercase().as_str()))
        {
            out.push(p);
        }
    }
}

/// `Artist - Title.ext` → (artist, title); otherwise the whole stem is the title.
fn local_track(path: &Path) -> Result<TrackRef> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    let mut t = TrackRef::local(&path, 0);
    if let Some((artist, title)) = t.title.clone().split_once(" - ") {
        t.artists = vec![artist.trim().to_string()];
        t.title = title.trim().to_string();
    }
    Ok(t)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub struct Importer {
    lib: Library,
    spotify_dir: PathBuf,
    bitrate: Bitrate,
    spotify: Option<ImportSession>,
    separator: Option<OfflineSeparator>,
    #[cfg(feature = "onnx")]
    aligner: Option<stems_sep::align::Aligner>,
    want_words: bool,
    lrclib: LrcLib,
    local_lyrics: LocalLyrics,
    video: Option<(String, u32)>,
    http: reqwest::Client,
}

impl Importer {
    /// Load the separation (and alignment) models per `cfg`. Without an
    /// ML backend tracks still import, with the stems stage failed.
    pub async fn new(cfg: &Config, lib: Library) -> Result<Self> {
        let paths = crate::config::paths()?;
        // no disk cache: instrumentals live in the library, and a cached
        // file from a live-stream decode may not be sample-aligned with
        // the stored original
        let separator =
            match crate::separator::build_offline(&cfg.karaoke, paths.models_dir.clone(), None)
                .await
            {
                Ok((sep, _limit)) => Some(sep),
                Err(e) => {
                    tracing::warn!("no separation for imports: {e:#}");
                    None
                }
            };
        #[cfg(feature = "onnx")]
        let aligner = if cfg.lyrics.align {
            match crate::separator::build_aligner(paths.models_dir.clone(), cfg.karaoke.threads)
                .await
            {
                Ok(a) => Some(a),
                Err(e) => {
                    tracing::warn!("word alignment unavailable: {e:#}");
                    None
                }
            }
        } else {
            None
        };
        let config_dir = paths
            .config_file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Ok(Self {
            lib,
            spotify_dir: paths.cache_dir.join("spotify"),
            bitrate: match cfg.spotify.bitrate {
                96 => Bitrate::Bitrate96,
                160 => Bitrate::Bitrate160,
                _ => Bitrate::Bitrate320,
            },
            spotify: None,
            separator,
            #[cfg(feature = "onnx")]
            aligner,
            want_words: cfg.lyrics.align,
            lrclib: LrcLib::new(),
            local_lyrics: LocalLyrics::new(config_dir.join("lyrics")),
            video: cfg
                .video
                .enabled
                .then(|| (cfg.video.ytdlp.clone(), cfg.video.max_height)),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    async fn spotify(&mut self) -> Result<&ImportSession> {
        if self.spotify.is_none() {
            self.spotify = Some(ImportSession::connect(&self.spotify_dir, self.bitrate).await?);
        }
        Ok(self.spotify.as_ref().expect("just connected"))
    }

    /// Expand sources into tracks (albums and playlists become their
    /// tracks), dropping duplicates.
    pub async fn resolve(&mut self, sources: &[Source]) -> Result<Vec<TrackRef>> {
        let mut out: Vec<TrackRef> = Vec::new();
        for s in sources {
            let tracks = match s {
                Source::Spotify(link) => {
                    let t = self.spotify().await?.resolve(link).await?;
                    tracing::info!(?link, tracks = t.len(), "resolved");
                    t
                }
                Source::File(p) => vec![local_track(p)?],
            };
            for t in tracks {
                if !out.iter().any(|o| o.id == t.id) {
                    out.push(t);
                }
            }
        }
        Ok(out)
    }

    /// Run every stage of `track` that still needs work. Errors are
    /// recorded per stage; only library I/O failures are returned.
    pub async fn import_track(&mut self, track: TrackRef) -> Result<Entry> {
        let key = Library::key_for(&track.id);
        let mut e = match self.lib.get(&key)? {
            Some(mut e) => {
                // keep hand edits, fill in what the source knows better now
                if e.track.isrc.is_none() {
                    e.track.isrc = track.isrc.clone();
                }
                if e.track.art.is_none() {
                    e.track.art = track.art.clone();
                }
                e
            }
            None => Entry::new(key, track, now()),
        };
        self.lib.save(&e)?;
        let started = Instant::now();
        let title = format!("{} — {}", e.track.artists_joined(), e.track.title);

        // audio: everything else depends on it
        let mix_missing = self.lib.files(&e).mix.is_none_or(|p| !p.is_file());
        let mut pcm: Option<Arc<Vec<f32>>> = None;
        if e.stages.audio.needs_work() || mix_missing {
            tracing::info!(track = %title, "fetching audio");
            match self.fetch_audio(&mut e).await {
                Ok(p) => {
                    e.stages.audio = Stage::Done;
                    pcm = Some(p);
                }
                Err(err) => {
                    tracing::warn!(track = %title, "audio failed: {err:#}");
                    e.stages.audio = Stage::failed(&err);
                    self.lib.save(&e)?;
                    return Ok(e);
                }
            }
            self.lib.save(&e)?;
        }

        let need_pcm = e.stages.stems.needs_work()
            || (self.want_words && e.stages.words.needs_work())
            || e.stages.video_align.needs_work();
        if need_pcm && pcm.is_none() {
            match self.decode_mix(&e).await {
                Ok(p) => pcm = Some(p),
                Err(err) => {
                    // the stored file is broken: refetch next time
                    tracing::warn!(track = %title, "stored audio does not decode: {err:#}");
                    e.stages.audio = Stage::failed(&err);
                    self.lib.save(&e)?;
                    return Ok(e);
                }
            }
        }

        // stems
        let mut buf: Option<Arc<StemBuffer>> = None;
        if e.stages.stems.needs_work() {
            let pcm = pcm.clone().expect("decoded above");
            tracing::info!(track = %title, "separating");
            match self.separate(&e, pcm).await {
                Ok(b) => {
                    e.stages.stems = Stage::Done;
                    buf = Some(b);
                }
                Err(err) => {
                    tracing::warn!(track = %title, "separation failed: {err:#}");
                    e.stages.stems = Stage::failed(&err);
                }
            }
            self.lib.save(&e)?;
        }
        let need_buf = e.stages.stems.is_done()
            && ((self.want_words && e.stages.words.needs_work())
                || e.stages.video_align.needs_work());
        if need_buf && buf.is_none() {
            let pcm = pcm.clone().expect("decoded above");
            match self.lib.read_instrumental(&e) {
                Ok(inst) => buf = StemBuffer::from_parts(&pcm, &inst).map(Arc::new),
                Err(err) => tracing::warn!(track = %title, "reading instrumental: {err:#}"),
            }
            if buf.is_none() {
                tracing::warn!(track = %title, "instrumental missing or mismatched; will separate again");
                e.stages.stems = Stage::Pending;
                self.lib.save(&e)?;
            }
        }

        // lyrics
        let mut lyrics = self.lib.load_lyrics(&e).ok().flatten();
        if e.stages.lyrics.needs_work() {
            match self.find_lyrics(&e).await {
                Ok(Some(l)) => {
                    self.lib.save_lyrics(&e, &l)?;
                    tracing::info!(track = %title, lines = l.lines.len(), synced = l.is_synced(), "lyrics found");
                    e.stages.lyrics = Stage::Done;
                    lyrics = Some(l);
                }
                Ok(None) => {
                    tracing::info!(track = %title, "no lyrics found");
                    e.stages.lyrics = Stage::Missing;
                }
                Err(err) => e.stages.lyrics = Stage::failed(err),
            }
            self.lib.save(&e)?;
        }

        // word timings
        if e.stages.words.needs_work() {
            e.stages.words = match (&lyrics, &buf) {
                _ if !self.want_words => Stage::Skipped,
                (Some(l), _) if !l.is_synced() => Stage::Skipped,
                (None, _) if !e.stages.lyrics.needs_work() => Stage::Skipped,
                (Some(l), _) if l.has_word_timings() => Stage::Done,
                (Some(l), Some(b)) => {
                    let mut l = l.clone();
                    match self.align_words(&e, b.clone(), &mut l).await {
                        Ok(n) => {
                            tracing::info!(track = %title, lines = l.lines.len(), aligned = n, "words aligned");
                            self.lib.save_lyrics(&e, &l)?;
                            Stage::Done
                        }
                        Err(err) => Stage::failed(err),
                    }
                }
                _ => Stage::Pending,
            };
            self.lib.save(&e)?;
        }

        // music video
        if e.stages.video.needs_work() {
            e.stages.video = match self.fetch_video(&e).await {
                Ok(Some(())) => Stage::Done,
                Ok(None) if self.video.is_none() => Stage::Skipped,
                Ok(None) => Stage::Missing,
                Err(err) => {
                    tracing::warn!(track = %title, "video failed: {err:#}");
                    Stage::failed(err)
                }
            };
            self.lib.save(&e)?;
        }
        if e.stages.video_align.needs_work() {
            e.stages.video_align = if e.video_offset_ms.is_some() {
                // set by hand or measured before: keep it
                Stage::Done
            } else if !e.stages.video.is_done() {
                if e.stages.video.needs_work() {
                    Stage::Pending
                } else {
                    Stage::Skipped
                }
            } else if let Some(b) = &buf {
                match self.align_video(&mut e, b.clone()).await {
                    Ok(()) => Stage::Done,
                    Err(err) => Stage::failed(err),
                }
            } else {
                Stage::Pending
            };
            self.lib.save(&e)?;
        }

        // cover art
        if e.stages.cover.needs_work() {
            e.stages.cover = match self.fetch_cover(&e).await {
                Ok(true) => Stage::Done,
                Ok(false) => Stage::Missing,
                Err(err) => Stage::failed(err),
            };
            self.lib.save(&e)?;
        }

        tracing::info!(
            track = %title,
            took = format!("{:.0}s", started.elapsed().as_secs_f32()),
            playable = e.is_playable(),
            complete = e.is_complete(),
            "imported"
        );
        Ok(e)
    }

    /// Store the original audio in the track folder, decode it, measure
    /// loudness and length.
    async fn fetch_audio(&mut self, e: &mut Entry) -> Result<Arc<Vec<f32>>> {
        let dir = self.lib.track_dir(&e.key);
        tokio::fs::create_dir_all(&dir).await?;
        let file = match &e.track.id {
            TrackId::Spotify(id) => {
                let id = id.clone();
                let dl = self.spotify().await?.download(&id).await?;
                let name = format!("mix.{}", dl.ext);
                write_atomic(&dir.join(&name), &dl.bytes).await?;
                name
            }
            TrackId::Local(src) => {
                let ext = src
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.to_ascii_lowercase())
                    .unwrap_or_else(|| "audio".into());
                let name = format!("mix.{ext}");
                let bytes = tokio::fs::read(src)
                    .await
                    .with_context(|| format!("read {}", src.display()))?;
                write_atomic(&dir.join(&name), &bytes).await?;
                name
            }
        };
        e.mix_file = Some(file);
        let pcm = self.decode_mix(e).await?;
        e.frames = (pcm.len() / 2) as u64;
        if e.track.duration_ms == 0 {
            e.track.duration_ms = stems_core::frames_to_ms(e.frames);
        }
        let p = Arc::clone(&pcm);
        e.loudness_lufs = tokio::task::spawn_blocking(move || loudness(&p))
            .await
            .ok()
            .flatten();
        Ok(pcm)
    }

    async fn decode_mix(&self, e: &Entry) -> Result<Arc<Vec<f32>>> {
        let path = self
            .lib
            .files(e)
            .mix
            .ok_or_else(|| anyhow!("no audio file recorded"))?;
        let pcm = tokio::task::spawn_blocking(move || stems_source_file::decode_file(&path))
            .await
            .context("decode task")??;
        if pcm.len() < 2 * 44_100 {
            bail!("audio is shorter than a second");
        }
        Ok(Arc::new(pcm))
    }

    /// Whole-track separation; blocks (asynchronously) until done.
    async fn separate(&self, e: &Entry, pcm: Arc<Vec<f32>>) -> Result<Arc<StemBuffer>> {
        let sep = self
            .separator
            .as_ref()
            .ok_or_else(|| anyhow!("no ML separation backend configured ([karaoke] backend)"))?;
        let buf = sep.submit(e.track.id.clone(), pcm, 0, true);
        let started = Instant::now();
        let mut last_ready = 0usize;
        let mut last_progress = Instant::now();
        let mut last_log = Instant::now();
        while !buf.is_done() {
            if buf.is_cancelled() {
                bail!("separation was cancelled");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            let ready = buf.ready_frames();
            if ready != last_ready {
                last_ready = ready;
                last_progress = Instant::now();
            } else if last_progress.elapsed() > STALL {
                buf.cancel();
                bail!("separation stalled for {}s", STALL.as_secs());
            }
            if last_log.elapsed() > Duration::from_secs(10) {
                last_log = Instant::now();
                tracing::info!(
                    title = %e.track.title,
                    percent = 100 * ready / buf.total_frames().max(1),
                    speed = format!("{:.1}x", buf.speed()),
                    "separating"
                );
            }
        }
        let inst = buf.instrumental();
        let lib = self.lib.clone();
        let entry = e.clone();
        tokio::task::spawn_blocking(move || lib.write_instrumental(&entry, &inst))
            .await
            .context("write task")??;
        tracing::info!(
            title = %e.track.title,
            secs = format!("{:.0}", started.elapsed().as_secs_f32()),
            "instrumental saved"
        );
        Ok(buf)
    }

    /// Hand-made `lyrics.lrc` in the track folder → `<config>/lyrics`
    /// overrides → LRCLIB.
    async fn find_lyrics(&self, e: &Entry) -> Result<Option<Lyrics>> {
        if let Some(text) = self.lib.lyrics_override(e) {
            let lines = stems_enrich::parse_lrc(&text);
            if !lines.is_empty() {
                return Ok(Some(Lyrics {
                    provider: "local".into(),
                    lines,
                    plain: None,
                    offset_ms: 0,
                }));
            }
        }
        if let Some(l) = self.local_lyrics.lookup(&e.track)? {
            return Ok(Some(l));
        }
        self.lrclib.lookup(&e.track).await
    }

    #[cfg(feature = "onnx")]
    async fn align_words(
        &mut self,
        e: &Entry,
        buf: Arc<StemBuffer>,
        lyrics: &mut Lyrics,
    ) -> Result<usize> {
        let Some(mut aligner) = self.aligner.take() else {
            bail!("alignment model not loaded");
        };
        let id = e.track.id.clone();
        let lines = lyrics.lines.clone();
        let (aligner, result) = tokio::task::spawn_blocking(move || {
            let r = stems_sep::align::worker::align_blocking(&mut aligner, id, buf, lines);
            (aligner, r)
        })
        .await
        .context("alignment task")?;
        self.aligner = Some(aligner);
        let aligned = result?;
        let n = aligned.len();
        for a in aligned {
            if let Some(line) = lyrics.lines.get_mut(a.line) {
                line.words = a.words;
            }
        }
        Ok(n)
    }

    #[cfg(not(feature = "onnx"))]
    async fn align_words(
        &mut self,
        _e: &Entry,
        _buf: Arc<StemBuffer>,
        _lyrics: &mut Lyrics,
    ) -> Result<usize> {
        bail!("this build has no ONNX support for word alignment")
    }

    /// `Ok(None)` when videos are off or none was found.
    async fn fetch_video(&self, e: &Entry) -> Result<Option<()>> {
        let Some((ytdlp, max_height)) = &self.video else {
            return Ok(None);
        };
        let files = self.lib.files(e);
        let fetcher = VideoFetcher::new(&files.dir, ytdlp.clone(), *max_height);
        match fetcher.fetch(&e.track).await? {
            Some(path) => {
                tokio::fs::rename(&path, &files.video).await?;
                Ok(Some(()))
            }
            None => Ok(None),
        }
    }

    /// Measure the video's offset from its soundtrack. A poor match is
    /// recorded (corr) but leaves the offset unset.
    async fn align_video(&self, e: &mut Entry, buf: Arc<StemBuffer>) -> Result<()> {
        use stems_enrich::valign;
        let path = self.lib.files(e).video;
        let result = tokio::task::spawn_blocking(move || -> Result<Option<(i64, f32)>> {
            let pcm = valign::decode_audio(&path, 8_000)?;
            let video_env = valign::envelope(&pcm, 8_000);
            let track_env = valign::envelope(buf.mix_mono(), stems_core::SAMPLE_RATE);
            Ok(valign::align(&track_env, &video_env, 150.0))
        })
        .await
        .context("video alignment task")??;
        match result {
            Some((offset, corr)) => {
                e.video_corr = Some(corr);
                if corr >= valign::MIN_CORR {
                    tracing::info!(title = %e.track.title, offset_ms = offset, corr = format!("{corr:.2}"), "video aligned");
                    e.video_offset_ms = Some(offset);
                } else {
                    tracing::warn!(title = %e.track.title, corr = format!("{corr:.2}"), "video does not match the track audio");
                }
            }
            None => tracing::warn!(title = %e.track.title, "video alignment found nothing"),
        }
        Ok(())
    }

    /// `Ok(false)` when the track has no art.
    async fn fetch_cover(&self, e: &Entry) -> Result<bool> {
        let Some(url) = e.track.art.as_deref().filter(|u| u.starts_with("http")) else {
            return Ok(false);
        };
        let resp = self.http.get(url).send().await?.error_for_status()?;
        let bytes = resp.bytes().await?;
        write_atomic(&self.lib.files(e).cover, &bytes).await?;
        Ok(true)
    }
}

async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("part");
    tokio::fs::write(&tmp, bytes)
        .await
        .with_context(|| format!("write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, path)
        .await
        .with_context(|| format!("rename to {}", path.display()))
}

/// Integrated loudness (EBU R128) of interleaved stereo 44.1 kHz.
fn loudness(pcm: &[f32]) -> Option<f32> {
    let mut m = ebur128::EbuR128::new(2, stems_core::SAMPLE_RATE, ebur128::Mode::I).ok()?;
    m.add_frames_f32(pcm).ok()?;
    m.loudness_global()
        .ok()
        .filter(|l| l.is_finite())
        .map(|l| l as f32)
}

/// One line per track that is not fully ready, for the end of a run.
pub fn report(entries: &[Entry]) -> String {
    let mut out = String::new();
    let playable = entries.iter().filter(|e| e.is_playable()).count();
    let complete = entries.iter().filter(|e| e.is_complete()).count();
    out.push_str(&format!(
        "{} tracks: {playable} playable offline, {complete} complete\n",
        entries.len()
    ));
    for e in entries {
        let mut notes: Vec<String> = e
            .failures()
            .into_iter()
            .map(|(stage, err)| format!("{stage} failed: {err}"))
            .collect();
        if e.stages.lyrics == Stage::Missing {
            notes.push("no lyrics".into());
        }
        if e.stages.lyrics.is_done() && e.stages.words == Stage::Skipped {
            notes.push("lyrics not synced".into());
        }
        if e.stages.video == Stage::Missing {
            notes.push("no video".into());
        }
        if e.stages.video.is_done() && e.video_offset_ms.is_none() {
            notes.push(format!(
                "video may not match (corr {:.2})",
                e.video_corr.unwrap_or(0.0)
            ));
        }
        if !notes.is_empty() {
            out.push_str(&format!(
                "  {} — {}: {}\n",
                e.track.artists_joined(),
                e.track.title,
                notes.join("; ")
            ));
        }
    }
    out
}

/// `stemsd import`: resolve, import one track at a time, print a report.
pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    let sources = parse_sources(args)?;
    let lib = Library::open(cfg.library_dir()?)?;
    tracing::info!(library = %lib.dir().display(), "importing");
    let mut importer = Importer::new(cfg, lib).await?;
    let tracks = importer.resolve(&sources).await?;
    let total = tracks.len();
    let mut done = Vec::with_capacity(total);
    for (i, t) in tracks.into_iter().enumerate() {
        tracing::info!(n = i + 1, of = total, title = %t.title, "importing track");
        done.push(importer.import_track(t).await?);
    }
    println!("\n{}", report(&done));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_names_split_into_artist_and_title() {
        let dir = std::env::temp_dir().join(format!("stems-import-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let a = dir.join("The Killers - Mr. Brightside.mp3");
        let b = dir.join("sub").join("untitled.flac");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        let t = local_track(&a).unwrap();
        assert_eq!(t.artists, vec!["The Killers"]);
        assert_eq!(t.title, "Mr. Brightside");
        assert_eq!(local_track(&b).unwrap().title, "untitled");

        let s = parse_sources(&[dir.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(s.len(), 2, "{s:?}");
        let s = parse_sources(&["spotify:track:4uLU6hMCjMI75M1A2tKUQC".into()]).unwrap();
        assert!(matches!(s[0], Source::Spotify(Link::Track(_))));
        assert!(parse_sources(&["nope-not-a-file".into()]).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn loudness_of_a_full_scale_sine() {
        let pcm: Vec<f32> = (0..44_100 * 3)
            .flat_map(|i| {
                let v = (i as f32 * 1000.0 * std::f32::consts::TAU / 44_100.0).sin();
                [v, v]
            })
            .collect();
        let l = loudness(&pcm).unwrap();
        // a 0 dBFS 1 kHz sine in both channels reads about 0 LUFS
        assert!((-1.5..=1.5).contains(&l), "{l}");
    }
}
