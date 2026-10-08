//! Fetch and decode a whole track outside the player, for pre-buffered
//! separation. Mirrors the player's own loading path: metadata → best
//! available file → encrypted stream → decrypt → Symphonia decode.

use std::io::{self, Read, Seek, SeekFrom};

use anyhow::{Context, Result, anyhow};
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::{Session, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioFiles, AudioItem};
use librespot_playback::config::Bitrate;
use librespot_playback::decoder::{AudioDecoder, SymphoniaDecoder};
use symphonia05::core::io::MediaSource;
use symphonia05::core::probe::Hint;
use tokio::sync::watch;

/// Spotify's Ogg files carry a proprietary header before the Ogg stream.
const SPOTIFY_OGG_HEADER_END: u64 = 0xa7;

/// Handle onto the live session; cheap to clone.
#[derive(Clone)]
pub struct TrackFetcher {
    session: watch::Receiver<Option<Session>>,
    bitrate: Bitrate,
}

impl TrackFetcher {
    pub(crate) fn new(session: watch::Receiver<Option<Session>>, bitrate: Bitrate) -> Self {
        Self { session, bitrate }
    }

    /// Download and decode `spotify:track:<base62>` to interleaved stereo
    /// f32 at 44.1 kHz. Runs the blocking decode on a worker thread.
    pub async fn fetch(&self, base62: &str) -> Result<Vec<f32>> {
        let session = self
            .session
            .borrow()
            .clone()
            .ok_or_else(|| anyhow!("no spotify session"))?;
        let opened = open_decrypted(&session, base62, self.bitrate).await?;
        let mut hint = Hint::new();
        if let Some(mime) = AudioFiles::mime_type(opened.format) {
            hint.mime_type(mime);
        }
        let Opened {
            decrypted,
            offset,
            len,
            name,
            ..
        } = opened;
        tokio::task::spawn_blocking(move || -> Result<Vec<f32>> {
            let src = Subfile::new(decrypted, offset, len)?;
            let mut decoder =
                SymphoniaDecoder::new(src, hint).map_err(|e| anyhow!("decoder for {name}: {e}"))?;
            let mut pcm: Vec<f32> = Vec::new();
            loop {
                match decoder.next_packet() {
                    Ok(Some((_, packet))) => {
                        let s = packet
                            .samples()
                            .map_err(|e| anyhow!("decoder packet: {e}"))?;
                        pcm.extend(s.iter().map(|v| *v as f32));
                    }
                    Ok(None) => break,
                    Err(e) => return Err(anyhow!("decode {name}: {e}")),
                }
            }
            Ok(pcm)
        })
        .await
        .context("decode task")?
    }
}

/// A track's audio file, downloaded and wrapped for decryption.
pub(crate) struct Opened {
    pub decrypted: AudioDecrypt<AudioFile>,
    /// Where the codec stream starts (past Spotify's Ogg header).
    pub offset: u64,
    pub len: u64,
    pub format: AudioFileFormat,
    pub name: String,
}

/// Metadata → best file for `bitrate` → whole file downloaded → key.
pub(crate) async fn open_decrypted(
    session: &Session,
    base62: &str,
    bitrate: Bitrate,
) -> Result<Opened> {
    let uri = SpotifyUri::from_uri(&format!("spotify:track:{base62}"))
        .map_err(|e| anyhow!("track uri: {e}"))?;
    let mut track_id: SpotifyId = (&uri).try_into().map_err(|e| anyhow!("track id: {e}"))?;

    let audio_item = AudioItem::get_file(session, uri)
        .await
        .map_err(|e| anyhow!("track metadata: {e}"))?;
    let mut picked = pick_file(&audio_item.files, bitrate);
    if picked.is_none() {
        // Not available in this account's country under this id: Spotify
        // lists the same recording released elsewhere as alternatives
        // (its own player "relinks" to them).
        for alt in audio_item.alternatives.iter().flat_map(|t| t.iter()) {
            let Ok(item) = AudioItem::get_file(session, alt.clone()).await else {
                continue;
            };
            if let Some(p) = pick_file(&item.files, bitrate) {
                tracing::info!(track = %audio_item.name, alternative = %alt, "using an alternative release");
                track_id = alt.try_into().map_err(|e| anyhow!("track id: {e}"))?;
                picked = Some(p);
                break;
            }
        }
    }
    let (format, file_id) = picked.ok_or_else(|| {
        anyhow!(
            "no playable file for {} (formats: {:?}, {} alternatives)",
            audio_item.name,
            audio_item.files.keys().collect::<Vec<_>>(),
            audio_item.alternatives.as_ref().map_or(0, |t| t.len())
        )
    })?;

    // A generous rate makes the loader prefetch far ahead of the reader.
    let bytes_per_second = 8 * 40_000;
    let encrypted = AudioFile::open(session, file_id, bytes_per_second)
        .await
        .map_err(|e| anyhow!("open audio file: {e}"))?;
    let controller = encrypted
        .get_stream_loader_controller()
        .map_err(|e| anyhow!("stream controller: {e}"))?;
    let len = controller.len() as u64;
    // Ask for the whole file up front so the download runs flat out
    // while the decoder works through it, instead of trickling behind
    // the read position.
    controller
        .fetch_next_and_wait(len as usize, 0)
        .map_err(|e| anyhow!("prefetch: {e}"))?;
    // Spotify's files are always encrypted: without the key all we'd have
    // is noise that no decoder reads, so no key is an error (retried), not
    // a reason to carry on.
    let key = session
        .audio_key()
        .request(track_id, file_id)
        .await
        .map_err(|e| anyhow!("audio key: {e}"))?;
    let key = Some(key);
    let offset = if AudioFiles::is_ogg_vorbis(format) {
        SPOTIFY_OGG_HEADER_END
    } else {
        0
    };
    Ok(Opened {
        decrypted: AudioDecrypt::new(key, encrypted),
        offset,
        len,
        format,
        name: audio_item.name,
    })
}

fn pick_file(
    files: &AudioFiles,
    bitrate: Bitrate,
) -> Option<(AudioFileFormat, librespot_core::FileId)> {
    use AudioFileFormat::*;
    let order: &[AudioFileFormat] = match bitrate {
        Bitrate::Bitrate96 => &[
            OGG_VORBIS_96,
            OGG_VORBIS_160,
            OGG_VORBIS_320,
            MP3_96,
            MP3_160,
            MP3_256,
            MP3_320,
        ],
        Bitrate::Bitrate160 => &[
            OGG_VORBIS_160,
            OGG_VORBIS_320,
            OGG_VORBIS_96,
            MP3_160,
            MP3_256,
            MP3_320,
            MP3_96,
        ],
        Bitrate::Bitrate320 => &[
            OGG_VORBIS_320,
            OGG_VORBIS_160,
            MP3_320,
            MP3_256,
            OGG_VORBIS_96,
            MP3_160,
            MP3_96,
        ],
    };
    order
        .iter()
        .find_map(|f| files.0.get(f).map(|id| (*f, *id)))
}

/// A byte-range view so the decoder sees the audio without Spotify's header.
struct Subfile<T: Read + Seek> {
    stream: T,
    offset: u64,
    length: u64,
}

impl<T: Read + Seek> Subfile<T> {
    fn new(mut stream: T, offset: u64, length: u64) -> io::Result<Self> {
        stream.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            stream,
            offset,
            length,
        })
    }
}

impl<T: Read + Seek> Read for Subfile<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl<T: Read + Seek> Seek for Subfile<T> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let pos = match pos {
            SeekFrom::Start(o) => SeekFrom::Start(o + self.offset),
            other => other,
        };
        let new = self.stream.seek(pos)?;
        Ok(new.saturating_sub(self.offset))
    }
}

impl<T: Read + Seek + Send + Sync> MediaSource for Subfile<T> {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}
