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
        let uri = SpotifyUri::from_uri(&format!("spotify:track:{base62}"))
            .map_err(|e| anyhow!("track uri: {e}"))?;
        let track_id: SpotifyId = (&uri).try_into().map_err(|e| anyhow!("track id: {e}"))?;

        let audio_item = AudioItem::get_file(&session, uri)
            .await
            .map_err(|e| anyhow!("track metadata: {e}"))?;
        let (format, file_id) = pick_file(&audio_item.files, self.bitrate)
            .ok_or_else(|| anyhow!("no playable file for {}", audio_item.name))?;

        // A generous rate makes the loader prefetch far ahead of the reader.
        let bytes_per_second = 8 * 40_000;
        let encrypted = AudioFile::open(&session, file_id, bytes_per_second)
            .await
            .map_err(|e| anyhow!("open audio file: {e}"))?;
        let len = encrypted
            .get_stream_loader_controller()
            .map_err(|e| anyhow!("stream controller: {e}"))?
            .len() as u64;
        let key = match session.audio_key().request(track_id, file_id).await {
            Ok(k) => Some(k),
            Err(e) => {
                tracing::warn!("audio key unavailable, trying without decryption: {e}");
                None
            }
        };
        let decrypted = AudioDecrypt::new(key, encrypted);
        let offset = if AudioFiles::is_ogg_vorbis(format) {
            SPOTIFY_OGG_HEADER_END
        } else {
            0
        };
        let mut hint = Hint::new();
        if let Some(mime) = AudioFiles::mime_type(format) {
            hint.mime_type(mime);
        }
        let name = audio_item.name.clone();
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
