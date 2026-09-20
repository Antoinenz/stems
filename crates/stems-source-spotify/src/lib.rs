//! Spotify Connect source built on librespot.
//!
//! Runs in *discovery* mode: the device shows up in the Spotify app on the
//! LAN, the phone hands over credentials via zeroconf, and from then on the
//! app is the remote control. No username/password or OAuth flow needed.
//!
//! Audio arrives through a custom librespot [`Sink`] that pushes f32 stereo
//! 44.1 kHz straight into the stems ring. Transport events are translated
//! from [`PlayerEvent`] to [`SourceEvent`].

mod canvas;
mod events;
pub mod fetch;
mod mixer;
mod sink;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use librespot_connect::{ConnectConfig, Spirc};
use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_core::config::DeviceType;
use librespot_core::{Session, SessionConfig};
use librespot_discovery::Discovery;
use librespot_playback::config::PlayerConfig;
use librespot_playback::mixer::Mixer as _;
use librespot_playback::player::Player;
use stems_audio::{FlushSignal, Producer};
use stems_core::{Source, SourceEvent};
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;

pub use librespot_core::config::DeviceType as SpotifyDeviceType;
pub use librespot_playback::config::Bitrate;

/// Reconnect no more than this many times per window before giving up.
const RECONNECT_RATE_LIMIT: usize = 5;
const RECONNECT_RATE_LIMIT_WINDOW: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct SpotifyConfig {
    /// Name shown in the Spotify Connect device picker.
    pub name: String,
    pub device_type: DeviceType,
    /// Where to persist credentials (so the phone only pairs once) and
    /// cached audio. `None` disables persistence.
    pub cache_dir: Option<PathBuf>,
    /// Audio cache size limit in bytes.
    pub audio_cache_limit: u64,
    pub bitrate: Bitrate,
    /// 0..=100 shown to the phone on connect.
    pub initial_volume_percent: u16,
    /// Zeroconf HTTP port; 0 = pick a free one.
    pub zeroconf_port: u16,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            name: "stems".into(),
            device_type: DeviceType::Tv,
            cache_dir: None,
            audio_cache_limit: 2 * 1024 * 1024 * 1024,
            bitrate: Bitrate::Bitrate320,
            initial_volume_percent: 70,
            zeroconf_port: 0,
        }
    }
}

pub struct SpotifySource {
    events: broadcast::Sender<SourceEvent>,
    frames_pushed: Arc<AtomicU64>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    fetcher: fetch::TrackFetcher,
}

impl SpotifySource {
    /// Advertise on the LAN and start serving audio into `producer`.
    /// Must be called from within a tokio runtime.
    pub async fn start(
        config: SpotifyConfig,
        producer: Producer,
        flush: FlushSignal,
    ) -> Result<Self> {
        let (events, _) = broadcast::channel(256);
        let frames_pushed = Arc::new(AtomicU64::new(0));
        let (stop_tx, stop_rx) = oneshot::channel();
        let (session_tx, session_rx) = watch::channel(None);

        let session_config = SessionConfig::default();
        let cache = match &config.cache_dir {
            Some(dir) => {
                let creds = dir.join("credentials.json");
                let volume = dir.join("volume");
                let audio = dir.join("audio");
                Some(
                    Cache::new(
                        Some(creds),
                        Some(volume),
                        Some(audio),
                        Some(config.audio_cache_limit),
                    )
                    .context("open librespot cache")?,
                )
            }
            None => None,
        };

        let discovery = Discovery::builder(
            session_config.device_id.clone(),
            session_config.client_id.clone(),
        )
        .name(config.name.clone())
        .device_type(config.device_type)
        .port(config.zeroconf_port)
        .launch()
        .context("launch zeroconf discovery")?;

        tracing::info!(name = %config.name, "spotify connect device advertised");

        let player_config = PlayerConfig {
            bitrate: config.bitrate,
            gapless: true,
            normalisation: false,
            ..PlayerConfig::default()
        };
        let mixer = Arc::new(mixer::VolumeStore::new(config.initial_volume_percent));
        let connect_config = ConnectConfig {
            name: config.name.clone(),
            device_type: config.device_type,
            is_group: false,
            initial_volume: mixer.volume(),
            ..ConnectConfig::default()
        };

        let task = tokio::spawn(run(
            Runtime {
                session_config,
                cache,
                player_config,
                connect_config,
                mixer,
                producer: Some(producer),
                flush,
                events: events.clone(),
                frames_pushed: frames_pushed.clone(),
                session_tx,
            },
            discovery,
            stop_rx,
        ));

        Ok(Self {
            events,
            frames_pushed,
            stop: Some(stop_tx),
            task: Some(task),
            fetcher: fetch::TrackFetcher::new(session_rx, config.bitrate),
        })
    }

    /// Out-of-band track download + decode (pre-buffered separation).
    pub fn fetcher(&self) -> fetch::TrackFetcher {
        self.fetcher.clone()
    }

    /// Wait for the background task to finish (after `shutdown`).
    pub async fn join(&mut self) {
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Source for SpotifySource {
    fn name(&self) -> &'static str {
        "spotify"
    }

    fn events(&self) -> broadcast::Receiver<SourceEvent> {
        self.events.subscribe()
    }

    fn frames_pushed(&self) -> u64 {
        self.frames_pushed.load(Ordering::Relaxed)
    }

    fn shutdown(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

struct Runtime {
    session_config: SessionConfig,
    cache: Option<Cache>,
    player_config: PlayerConfig,
    connect_config: ConnectConfig,
    mixer: Arc<mixer::VolumeStore>,
    producer: Option<Producer>,
    flush: FlushSignal,
    events: broadcast::Sender<SourceEvent>,
    frames_pushed: Arc<AtomicU64>,
    /// The live session, for out-of-band track fetches.
    session_tx: watch::Sender<Option<Session>>,
}

/// Mirrors librespot's own main loop: wait for credentials from discovery,
/// build a session + Spirc, reconnect on drop, tear down on stop.
async fn run(mut rt: Runtime, mut discovery: Discovery, mut stop: oneshot::Receiver<()>) {
    let mut session = Session::new(rt.session_config.clone(), rt.cache.clone());
    let _ = rt.session_tx.send(Some(session.clone()));
    let mut credentials: Option<Credentials> = rt.cache.as_ref().and_then(|c| c.credentials());
    let mut connecting = credentials.is_some();
    let mut spirc: Option<Spirc> = None;
    let mut spirc_task: Option<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>> = None;
    let mut reconnects: Vec<Instant> = Vec::new();

    if connecting {
        tracing::info!("using cached spotify credentials");
    }

    // The player owns the audio thread for the whole lifetime; the sink is
    // created once from the ring producer.
    let producer = rt.producer.take().expect("producer present at start");
    let flush = rt.flush.clone();
    let frames = rt.frames_pushed.clone();
    let player = Player::new(
        rt.player_config.clone(),
        session.clone(),
        mixer::NoOpVolume::boxed(),
        move || Box::new(sink::RingSink::new(producer, flush, frames)),
    );
    let mut event_task = tokio::spawn(events::pump(
        player.get_player_event_channel(),
        session.clone(),
        rt.events.clone(),
        rt.frames_pushed.clone(),
    ));

    loop {
        tokio::select! {
            _ = &mut stop => break,

            creds = discovery.next() => match creds {
                Some(c) => {
                    tracing::info!("spotify credentials received from a client");
                    credentials = Some(c);
                    reconnects.clear();
                    if let Some(s) = spirc.take() {
                        let _ = s.shutdown();
                    }
                    if let Some(t) = spirc_task.take() {
                        tokio::spawn(t);
                    }
                    if !session.is_invalid() {
                        session.shutdown();
                    }
                    connecting = true;
                }
                None => {
                    tracing::error!("zeroconf discovery stopped unexpectedly");
                    break;
                }
            },

            _ = async {}, if connecting && credentials.is_some() => {
                if session.is_invalid() {
                    session = Session::new(rt.session_config.clone(), rt.cache.clone());
                    let _ = rt.session_tx.send(Some(session.clone()));
                    player.set_session(session.clone());
                    event_task.abort();
                    event_task = tokio::spawn(events::pump(
                        player.get_player_event_channel(),
                        session.clone(),
                        rt.events.clone(),
                        rt.frames_pushed.clone(),
                    ));
                }
                let creds = credentials.clone().unwrap_or_default();
                match Spirc::new(
                    rt.connect_config.clone(),
                    session.clone(),
                    creds,
                    player.clone(),
                    rt.mixer.clone(),
                )
                .await
                {
                    Ok((s, t)) => {
                        spirc = Some(s);
                        spirc_task = Some(Box::pin(t));
                        tracing::info!("spotify connect session established");
                    }
                    Err(e) => {
                        tracing::error!("could not start spotify connect: {e}");
                        credentials = None;
                        let _ = rt.events.send(SourceEvent::Disconnected);
                    }
                }
                connecting = false;
            },

            _ = async { if let Some(t) = spirc_task.as_mut() { t.await } },
                if spirc_task.is_some() && !connecting => {
                spirc_task = None;
                spirc = None;
                let _ = rt.events.send(SourceEvent::Disconnected);
                reconnects.retain(|t| t.elapsed() < RECONNECT_RATE_LIMIT_WINDOW);
                if credentials.is_some() && reconnects.len() < RECONNECT_RATE_LIMIT {
                    tracing::warn!("spotify connect session dropped; reconnecting");
                    reconnects.push(Instant::now());
                    if !session.is_invalid() {
                        session.shutdown();
                    }
                    connecting = true;
                } else {
                    tracing::warn!("spotify connect session dropped; waiting for a new client");
                }
            },

            _ = async {}, if player.is_invalid() => {
                tracing::error!("librespot player died");
                break;
            }
        }
    }

    tracing::info!("spotify source shutting down");
    if let Some(s) = spirc.take() {
        let _ = s.shutdown();
    }
    if let Some(t) = spirc_task.take() {
        let _ = tokio::time::timeout(Duration::from_secs(3), t).await;
    }
    // libmdns can panic in `Drop` if its responder task is already gone
    // (seen on Windows at runtime teardown); leaking the discovery handle
    // avoids that path. The OS reclaims the sockets on exit.
    std::mem::forget(discovery);
    event_task.abort();
    let _ = rt.events.send(SourceEvent::Disconnected);
}
