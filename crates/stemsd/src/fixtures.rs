//! JSON fixtures of everything the API sends, written into the Apple
//! app's test bundle. The Swift tests decode them, so a change on either
//! side that breaks the other fails a test instead of the app at a party.
//!
//! `cargo test -p stemsd fixtures` checks the files are current;
//! `STEMS_UPDATE_FIXTURES=1 cargo test -p stemsd fixtures` rewrites them.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use stems_core::{
    ClockTick, KaraokeState, LyricLine, LyricWord, Lyrics, Media, PlaybackState, PlaybackStatus,
    QueueItem, QueueKind, StemsProgress, TrackId, TrackRef, ViewMode,
};
use stems_library::{Entry, Library, Stage};
use stems_server::{Access, LibraryApi, Outbound, SystemStats, Temp};

use crate::config::Config;
use crate::engine::AudioStatus;
use crate::library::LibraryService;
use crate::settings::Settings;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/apple/StemsKit/Tests/StemsKitTests/Fixtures")
}

/// Compare with (or, when asked, rewrite) one fixture. Returns a message
/// for a stale file.
fn check(name: &str, value: &Value) -> Option<String> {
    let path = dir().join(format!("{name}.json"));
    let text = serde_json::to_string_pretty(value).unwrap() + "\n";
    if std::env::var_os("STEMS_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(dir()).unwrap();
        std::fs::write(&path, text).unwrap();
        return None;
    }
    let current: Option<Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    (current.as_ref() != Some(value)).then(|| format!("{name}.json"))
}

fn track(id: &str, title: &str, artists: &[&str], ms: u64) -> TrackRef {
    TrackRef {
        id: TrackId::Spotify(id.into()),
        isrc: Some("USUM71111621".into()),
        title: title.into(),
        artists: artists.iter().map(|a| a.to_string()).collect(),
        album: Some("Album".into()),
        duration_ms: ms,
        art: Some(format!("/media/library/spotify_{id}/cover.jpg")),
    }
}

fn item(id: u64, t: &TrackRef, singer: Option<&str>, kind: QueueKind) -> QueueItem {
    let TrackId::Spotify(sid) = &t.id else {
        unreachable!()
    };
    QueueItem {
        id,
        key: format!("spotify_{sid}"),
        track: t.clone(),
        singer: singer.map(Into::into),
        kind,
    }
}

fn playing_state() -> PlaybackState {
    let now = track(
        "1auxYwYrFRqZP7t3s7w4um",
        "Ni**as In Paris",
        &["JAY-Z", "Kanye West"],
        219_333,
    );
    let next = track(
        "0nbXyq5TXYPCO7pr3N8S4I",
        "The Box",
        &["Roddy Ricch"],
        196_652,
    );
    let other = track(
        "4uLU6hMCjMI75M1A2tKUQC",
        "Never Gonna Give You Up",
        &["Rick Astley"],
        213_573,
    );
    PlaybackState {
        source: Some("library".into()),
        status: PlaybackStatus::Playing,
        track: Some(now.clone()),
        next_track: Some(next.clone()),
        queue: vec![
            item(3, &next, None, QueueKind::Play),
            item(4, &other, Some("Sam & Alex"), QueueKind::Sing),
        ],
        library_key: Some("spotify_1auxYwYrFRqZP7t3s7w4um".into()),
        current: Some(item(1, &now, Some("Mia"), QueueKind::Sing)),
        intermission: false,
        lyrics: Some(Lyrics {
            provider: "lrclib".into(),
            lines: vec![
                LyricLine {
                    time_ms: 15_790,
                    text: "So I ball so hard".into(),
                    words: vec![
                        LyricWord {
                            time_ms: 15_810,
                            text: "So".into(),
                        },
                        LyricWord {
                            time_ms: 16_090,
                            text: "I".into(),
                        },
                        LyricWord {
                            time_ms: 16_210,
                            text: "ball".into(),
                        },
                        LyricWord {
                            time_ms: 16_500,
                            text: "so".into(),
                        },
                        LyricWord {
                            time_ms: 16_800,
                            text: "hard".into(),
                        },
                    ],
                },
                LyricLine {
                    time_ms: 19_000,
                    text: "A line without word timings".into(),
                    words: vec![],
                },
            ],
            plain: None,
            offset_ms: -120,
        }),
        karaoke: KaraokeState {
            enabled: true,
            vocal_gain: 0.15,
            backend: Some("prebuffer".into()),
            stems: Some(StemsProgress {
                ready_ms: 219_333,
                total_ms: 219_333,
                done: true,
                speed: 4.1,
            }),
        },
        view_mode: ViewMode::Video,
        media: Media {
            canvas_url: None,
            video_url: Some("/media/library/spotify_1auxYwYrFRqZP7t3s7w4um/video.mp4".into()),
            video_offset_ms: 350,
        },
        volume: 0.8,
        revision: 42,
    }
}

fn audio() -> AudioStatus {
    AudioStatus {
        source: "library".into(),
        sink: "cpal".into(),
        sink_latency_ms: 12,
        processor: "prebuffer".into(),
        processor_delay_ms: 0,
        buffered_ms: 160,
        dropout_ms: 23,
        dropout_seconds: 1,
        uptime_s: 3_725,
    }
}

/// A library with one finished track and one whose lyrics failed.
fn library(name: &str) -> Library {
    let dir = std::env::temp_dir().join(format!("stems-fixtures-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let lib = Library::open(&dir).unwrap();
    let mut done = Entry::new(
        "spotify_1auxYwYrFRqZP7t3s7w4um".into(),
        track(
            "1auxYwYrFRqZP7t3s7w4um",
            "Ni**as In Paris",
            &["JAY-Z", "Kanye West"],
            219_333,
        ),
        1_791_332_963,
    );
    done.mix_file = Some("mix.ogg".into());
    done.frames = 9_672_585;
    done.loudness_lufs = Some(-8.9);
    done.video_offset_ms = Some(350);
    done.video_corr = Some(0.82);
    done.stems_model = Some("model_bs_roformer_ep_317_sdr_12.9755".into());
    done.info = stems_core::TrackInfo {
        release_date: Some("2011-08-08".into()),
        label: Some("Roc-A-Fella".into()),
        popularity: Some(78),
        explicit: Some(true),
        ..Default::default()
    };
    done.analysis = Some(stems_library::Analysis {
        bpm: Some(140.1),
        key: Some("D minor".into()),
        danceability: Some(0.86),
        moods: [("party".to_string(), 0.71f32), ("happy".to_string(), 0.32)].into(),
        genres: vec![stems_library::Genre {
            name: "Hip Hop / Trap".into(),
            score: 0.64,
        }],
        analyzer: Some("essentia".into()),
        ..Default::default()
    });
    for s in [
        &mut done.stages.audio,
        &mut done.stages.stems,
        &mut done.stages.lyrics,
        &mut done.stages.words,
        &mut done.stages.video,
        &mut done.stages.video_align,
        &mut done.stages.cover,
        &mut done.stages.analysis,
    ] {
        *s = Stage::Done;
    }
    lib.save(&done).unwrap();
    let mut partial = Entry::new(
        "local_demo_00ff00ff00ff00ff".into(),
        TrackRef {
            id: TrackId::Local("/music/demo.flac".into()),
            isrc: None,
            title: "Demo".into(),
            artists: vec![],
            album: None,
            duration_ms: 61_000,
            art: None,
        },
        1_791_333_000,
    );
    partial.mix_file = Some("mix.flac".into());
    partial.stages.audio = Stage::Done;
    partial.stages.stems = Stage::Done;
    partial.stages.lyrics = Stage::Failed {
        error: "no lyrics found on LRCLIB".into(),
    };
    partial.stages.video = Stage::Skipped;
    lib.save(&partial).unwrap();
    lib
}

#[tokio::test]
async fn api_fixtures_are_current() {
    let mut stale = Vec::new();
    let mut put = |name: &str, v: Value| stale.extend(check(name, &v));

    // WebSocket messages
    let state = playing_state();
    put("ws_state", json!(Outbound::State(&state)));
    put(
        "ws_state_idle",
        json!(Outbound::State(&PlaybackState::default())),
    );
    let intermission = PlaybackState {
        current: None,
        intermission: true,
        queue: vec![],
        karaoke: KaraokeState::default(),
        lyrics: Some(Lyrics {
            provider: "local".into(),
            lines: vec![],
            plain: Some("Unsynced words\nSecond line".into()),
            offset_ms: 0,
        }),
        ..playing_state()
    };
    put(
        "ws_state_intermission",
        json!(Outbound::State(&intermission)),
    );
    let tick = ClockTick {
        revision: 42,
        status: PlaybackStatus::Playing,
        position_ms: 16_250,
        server_ms: 98_765_432,
    };
    put("ws_clock", json!(Outbound::Clock(&tick)));
    put(
        "ws_error",
        json!(Outbound::Error {
            message: "guests may not change queue here".into()
        }),
    );

    // status (also pushed over the WebSocket to hosts)
    let mut cfg = Config::default();
    cfg.karaoke.backend = "remote".into();
    cfg.karaoke.remote = "listen:7375".into();
    let mut worker = stems_sep::remote::WorkerStat::new(
        "192.168.8.20",
        "UVR-MDX-NET-Inst_HQ_3",
        "directml",
        true,
    );
    worker.connections = 2;
    worker.requests = 121;
    worker.failures = 1;
    worker.window_ms = 1441.5;
    worker.last_error = Some("connection reset by peer".into());
    worker.idle_s = Some(0.4);
    let lib = library("status");
    let service = LibraryService::start(&cfg, lib.clone());
    let mut lib_status = service.status();
    // machine-specific bits
    lib_status["dir"] = json!("/Volumes/Party/stems-library");
    lib_status["free_bytes"] = json!(156_814_767_167u64);
    lib_status["total_bytes"] = json!(494_384_795_648u64);
    let mut status = crate::status::document(
        "library",
        &audio(),
        &crate::status::separation(&cfg),
        &[worker],
        lib_status,
    );
    status["system"] = json!(SystemStats {
        cpu_percent: 43.9,
        mem_used_mb: 6472,
        mem_total_mb: 8192,
        temps: vec![Temp {
            label: "PMU tdie7".into(),
            celsius: 71.5
        }],
    });
    let apps: BTreeMap<String, usize> =
        [("stage".into(), 1), ("remote".into(), 3), ("ios".into(), 1)].into();
    status["clients"] = stems_server::clients_json(&apps, 2, 3);
    put("status", status.clone());
    put("ws_status", json!(Outbound::Status(&status)));
    // the same with no library and nothing connected (spotify mode)
    let mut bare = crate::status::document(
        "spotify",
        &audio(),
        &crate::status::separation(&Config::default()),
        &[],
        Value::Null,
    );
    bare["system"] = json!(SystemStats::default());
    bare["clients"] = stems_server::clients_json(&BTreeMap::new(), 0, 0);
    put("status_spotify", bare);

    // library
    put("library_host", service.list("", true).unwrap());
    // the indexed views (library.sqlite)
    lib.append_play(&stems_library::Play {
        key: "spotify_1auxYwYrFRqZP7t3s7w4um".into(),
        at: 1_791_400_000,
        played_ms: 200_000,
        completed: true,
        kind: "sing".into(),
        singer: Some("Mia".into()),
        background: false,
    })
    .unwrap();
    lib.save_playlist(&stems_library::Playlist {
        id: "spotify_playlist_19PgP2QSGPcm6Ve8VhbtpG".into(),
        kind: "playlist".into(),
        name: "80s HITS".into(),
        url: Some("https://open.spotify.com/playlist/19PgP2QSGPcm6Ve8VhbtpG".into()),
        description: "The best of the 1980s".into(),
        tracks: vec![
            "spotify_1auxYwYrFRqZP7t3s7w4um".into(),
            "spotify_missing".into(),
        ],
        updated_at: 1_791_332_000,
    })
    .unwrap();
    stems_library::index::update(&lib).unwrap();
    let params: std::collections::HashMap<String, String> = [
        ("sort".to_string(), "danceability".to_string()),
        ("desc".to_string(), "1".to_string()),
    ]
    .into();
    let mut page = service.songs(&params, false).unwrap();
    for s in page["songs"].as_array_mut().unwrap() {
        s["added_at"] = json!(1_791_332_963u64); // machine-independent
    }
    put("library_songs", page);
    put("library_playlists", service.playlists().unwrap());
    put("library_genres", service.genres().unwrap());
    put(
        "library_song",
        service
            .song("spotify_1auxYwYrFRqZP7t3s7w4um")
            .unwrap()
            .unwrap(),
    );
    put("library_guest", service.list("", false).unwrap());
    put("import_status_idle", service.import_status());
    let mut busy = crate::library::ImportStatus {
        running: true,
        current: Some("JAY-Z, Kanye West — Ni**as In Paris".into()),
        waiting: vec!["Roddy Ricch — The Box".into()],
        ..Default::default()
    };
    busy.finished.push_front(crate::library::Finished {
        key: "local_demo_00ff00ff00ff00ff".into(),
        title: "Demo".into(),
        playable: true,
        complete: false,
        problems: vec!["lyrics: no lyrics found on LRCLIB".into()],
    });
    busy.errors
        .push_front("not a Spotify link or audio file: htps://x".into());
    put("import_status_busy", json!(busy));

    // who am I, settings, info
    let tmp = std::env::temp_dir().join(format!("stems-fixtures-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let scfg = Config {
        host_token: "XH7ZT8".into(),
        ..Default::default()
    };
    let settings: Arc<dyn Access> = Settings::new(scfg, tmp.join("stems.toml"), tmp.join("video"));
    let mut s = settings.settings();
    s["config"]["library"]["dir"] = json!("/Volumes/Party/stems-library");
    put("settings", s);
    put(
        "me_host",
        json!(stems_server::access::me(
            &settings,
            stems_server::Role::Host
        )),
    );
    put(
        "me_guest",
        json!(stems_server::access::me(
            &settings,
            stems_server::Role::Guest
        )),
    );
    put(
        "info",
        json!(stems_server::Info {
            device_name: "stems".into(),
            lan_host: Some("192.168.8.10:7373".into()),
            remote_url: Some("http://192.168.8.10:7373/remote/".into()),
            stage_url: Some("http://192.168.8.10:7373/stage/".into()),
        }),
    );

    // what `stemsd config --json` prints (the Mac app reads it)
    let mut cfg = Config {
        host_token: "XH7ZT8".into(),
        ..Default::default()
    };
    cfg.library.dir = "/Volumes/Party/stems-library".into();
    let home = std::path::Path::new("/Users/me/stems");
    let paths = crate::config::Paths {
        config_file: home.join("stems.toml"),
        cache_dir: home.join("cache"),
        models_dir: home.join("cache/models"),
        video_dir: home.join("cache/video"),
        stems_dir: home.join("cache/stems"),
        library_dir: home.join("library"),
    };
    let mut described = cfg.describe(&paths.config_file, &paths).unwrap();
    described["portable"] = json!(false); // depends on the environment
    put("config_json", described);

    let _ = std::fs::remove_dir_all(lib.dir());
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        stale.is_empty(),
        "API fixtures changed: {}. If intended, run `STEMS_UPDATE_FIXTURES=1 cargo test -p stemsd fixtures` \
         and update the Swift models in apps/apple/StemsKit to match.",
        stale.join(", ")
    );
}
