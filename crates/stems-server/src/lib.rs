//! Local HTTP + WebSocket server.
//!
//! * `GET  /api/state`  — current [`PlaybackState`] as JSON
//! * `GET  /api/ws`     — WebSocket: `{"type":"state",...}` on connect and on
//!   every change, `{"type":"clock",...}` at ~10 Hz; accepts [`Command`] JSON
//! * `POST /api/command` — one [`Command`] as JSON
//! * `GET  /api/health`
//! * `/stage`, `/remote`, `/` — static files from the web directory, if present
//!
//! The server owns no state: it forwards `watch` channels from the engine and
//! pushes commands back through an `mpsc`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use stems_core::{ClockTick, Command, PlaybackState};
use tokio::sync::{mpsc, watch};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;

#[derive(Clone)]
pub struct Channels {
    pub state: watch::Receiver<PlaybackState>,
    pub clock: watch::Receiver<ClockTick>,
    pub commands: mpsc::Sender<Command>,
}

/// Host load, for the remote's status line. Sampled at most every 2 s.
#[derive(Serialize, Clone, Default)]
pub struct SystemStats {
    pub cpu_percent: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    /// Temperature sensors the OS exposes (none on some platforms).
    pub temps: Vec<Temp>,
}

#[derive(Serialize, Clone)]
pub struct Temp {
    pub label: String,
    pub celsius: f32,
}

struct SysMon {
    sys: sysinfo::System,
    components: sysinfo::Components,
    last: Option<std::time::Instant>,
    stats: SystemStats,
}

impl SysMon {
    fn new() -> Self {
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Self {
            sys,
            components: sysinfo::Components::new_with_refreshed_list(),
            last: None,
            stats: SystemStats::default(),
        }
    }

    fn sample(&mut self) -> SystemStats {
        let fresh = self
            .last
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(2));
        if fresh {
            return self.stats.clone();
        }
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.components.refresh(false);
        let mut temps: Vec<Temp> = self
            .components
            .iter()
            .filter_map(|c| {
                c.temperature().map(|t| Temp {
                    label: c.label().to_string(),
                    celsius: t,
                })
            })
            .filter(|t| t.celsius > 0.0 && t.celsius < 150.0)
            .collect();
        // keep the interesting ones: CPU/GPU/SoC-ish, hottest first
        let keyword = |l: &str| {
            let l = l.to_ascii_lowercase();
            ["cpu", "gpu", "soc", "core", "package", "thermal", "pmu", "die"]
                .iter()
                .any(|k| l.contains(k))
        };
        if temps.iter().any(|t| keyword(&t.label)) {
            temps.retain(|t| keyword(&t.label));
        }
        temps.sort_by(|a, b| b.celsius.total_cmp(&a.celsius));
        temps.truncate(4);
        self.stats = SystemStats {
            cpu_percent: self.sys.global_cpu_usage(),
            mem_used_mb: self.sys.used_memory() / (1024 * 1024),
            mem_total_mb: self.sys.total_memory() / (1024 * 1024),
            temps,
        };
        self.last = Some(std::time::Instant::now());
        self.stats.clone()
    }
}

/// What a client needs to point other devices at this box.
#[derive(Serialize)]
pub struct Info {
    /// Name this box advertises to Spotify Connect / AirPlay.
    pub device_name: String,
    /// Best guess at the LAN address of the server, `host:port`.
    pub lan_host: Option<String>,
    pub remote_url: Option<String>,
    pub stage_url: Option<String>,
}

/// The LAN interface a default route would use. No packet is sent: a
/// UDP "connect" only picks the source address.
pub fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?; // TEST-NET; never routed anywhere
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Outbound<'a> {
    State(&'a PlaybackState),
    Clock(&'a ClockTick),
    Error { message: String },
}

pub struct Server {
    pub addr: SocketAddr,
    pub device_name: String,
    pub web_dir: Option<PathBuf>,
    /// Downloaded music videos, served at `/media/video/`.
    pub video_dir: Option<PathBuf>,
}

impl Server {
    pub fn router(
        ch: Channels,
        device_name: &str,
        web_dir: Option<&Path>,
        video_dir: Option<&Path>,
    ) -> Router {
        let device_name = std::sync::Arc::new(device_name.to_string());
        let sysmon = std::sync::Arc::new(std::sync::Mutex::new(SysMon::new()));
        let mut app = Router::new()
            .route("/api/health", get(|| async { "ok" }))
            .route("/api/state", get(get_state))
            .route("/api/command", post(post_command))
            .route("/api/ws", get(ws_upgrade))
            .route("/api/info", {
                let name = device_name.clone();
                get(move |headers| async move { Json(info(&headers, &name)) })
            })
            .route("/api/remote-qr.svg", get(get_remote_qr))
            .route("/api/system", {
                let m = sysmon.clone();
                get(move || async move { Json(m.lock().expect("sysmon").sample()) })
            })
            .with_state(ch)
            .layer(CorsLayer::permissive());

        if let Some(dir) = web_dir {
            // Vite hashes asset file names, so only the HTML shell must not
            // be cached — otherwise browsers keep loading the old bundle.
            let no_cache = SetResponseHeaderLayer::if_not_present(
                axum::http::header::CACHE_CONTROL,
                axum::http::HeaderValue::from_static("no-cache"),
            );
            for app_name in ["stage", "remote"] {
                let dist = dir.join(app_name).join("dist");
                if dist.join("index.html").is_file() {
                    tracing::info!(app = app_name, dir = %dist.display(), "serving web app");
                    let index = ServeFile::new(dist.join("index.html"));
                    let svc = tower::ServiceBuilder::new()
                        .layer(no_cache.clone())
                        .service(ServeDir::new(&dist).fallback(index));
                    app = app.nest_service(&format!("/{app_name}"), svc);
                } else {
                    tracing::warn!(app = app_name, dir = %dist.display(), "web app not built; skipping");
                }
            }
            app = app.route("/", get(|| async { Redirect::temporary("/remote/") }));
        }
        if let Some(dir) = video_dir {
            // ServeDir handles Range requests, which <video> seeking needs.
            app = app.nest_service("/media/video", ServeDir::new(dir));
        }
        app
    }

    /// Bind and serve until the task is dropped / the process exits.
    pub async fn serve(self, ch: Channels) -> Result<()> {
        let app = Self::router(
            ch,
            &self.device_name,
            self.web_dir.as_deref(),
            self.video_dir.as_deref(),
        );
        let listener = tokio::net::TcpListener::bind(self.addr)
            .await
            .with_context(|| format!("bind {}", self.addr))?;
        tracing::info!(addr = %self.addr, "http server listening");
        axum::serve(listener, app).await.context("http server")
    }
}

fn info(headers: &axum::http::HeaderMap, device_name: &str) -> Info {
    let port = headers
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.rsplit(':').next())
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(80);
    let lan_host = lan_ip().map(|ip| format!("{ip}:{port}"));
    Info {
        device_name: device_name.to_string(),
        remote_url: lan_host.as_ref().map(|h| format!("http://{h}/remote/")),
        stage_url: lan_host.as_ref().map(|h| format!("http://{h}/stage/")),
        lan_host,
    }
}

/// QR code of the remote's LAN URL, for the stage to show.
async fn get_remote_qr(headers: axum::http::HeaderMap) -> impl IntoResponse {
    let Some(url) = info(&headers, "").remote_url else {
        return (StatusCode::NOT_FOUND, "no LAN address").into_response();
    };
    match qrcode::QrCode::new(url.as_bytes()) {
        Ok(code) => {
            let svg = code
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(160, 160)
                .quiet_zone(true)
                .dark_color(qrcode::render::svg::Color("#000"))
                .light_color(qrcode::render::svg::Color("#fff"))
                .build();
            ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], svg).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn get_state(State(ch): State<Channels>) -> Json<PlaybackState> {
    Json(ch.state.borrow().clone())
}

async fn post_command(
    State(ch): State<Channels>,
    Json(cmd): Json<Command>,
) -> Result<StatusCode, (StatusCode, String)> {
    ch.commands
        .send(cmd)
        .await
        .map(|_| StatusCode::ACCEPTED)
        .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "engine not running".into()))
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(ch): State<Channels>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_session(socket, ch))
}

async fn send(
    tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    msg: Outbound<'_>,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(&msg).expect("serializable");
    tx.send(Message::Text(text.into())).await
}

async fn ws_session(socket: WebSocket, mut ch: Channels) {
    let (mut tx, mut rx) = socket.split();

    // Initial snapshot.
    {
        let snapshot = ch.state.borrow().clone();
        if send(&mut tx, Outbound::State(&snapshot)).await.is_err() {
            return;
        }
    }
    ch.state.mark_unchanged();
    ch.clock.mark_unchanged();

    loop {
        tokio::select! {
            changed = ch.state.changed() => {
                if changed.is_err() { break; }
                let snapshot = ch.state.borrow_and_update().clone();
                if send(&mut tx, Outbound::State(&snapshot)).await.is_err() { break; }
            }
            changed = ch.clock.changed() => {
                if changed.is_err() { break; }
                let tick = *ch.clock.borrow_and_update();
                if send(&mut tx, Outbound::Clock(&tick)).await.is_err() { break; }
            }
            incoming = rx.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str::<Command>(&text) {
                        Ok(cmd) => {
                            if ch.commands.send(cmd).await.is_err() { break; }
                        }
                        Err(e) => {
                            let _ = send(&mut tx, Outbound::Error { message: format!("bad command: {e}") }).await;
                        }
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn channels() -> (
        Channels,
        watch::Sender<PlaybackState>,
        mpsc::Receiver<Command>,
    ) {
        let (state_tx, state_rx) = watch::channel(PlaybackState::default());
        let (_clock_tx, clock_rx) = watch::channel(ClockTick::default());
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        (
            Channels {
                state: state_rx,
                clock: clock_rx,
                commands: cmd_tx,
            },
            state_tx,
            cmd_rx,
        )
    }

    #[tokio::test]
    async fn state_endpoint_returns_json() {
        let (ch, state_tx, _rx) = channels();
        state_tx.send_modify(|s| s.volume = 0.42);
        let app = Server::router(ch, "test", None, None);
        let res = app
            .oneshot(Request::get("/api/state").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["volume"], 0.42);
        assert_eq!(v["status"], "idle");
    }

    #[tokio::test]
    async fn command_endpoint_forwards_to_engine() {
        let (ch, _state_tx, mut rx) = channels();
        let app = Server::router(ch, "test", None, None);
        let res = app
            .oneshot(
                Request::post("/api/command")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"cmd":"set_karaoke","enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert_eq!(rx.recv().await, Some(Command::SetKaraoke { enabled: true }));
    }
}
