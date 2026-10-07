// Mirrors crates/stems-core/src/state.rs and track.rs (serde, snake_case).
// The JSON fixtures in Tests/StemsKitTests/Fixtures are written by the Rust
// side (`cargo test -p stemsd fixtures`); the tests decode every one of them.
//
// Decoding is forgiving on purpose: a missing field takes its default, an
// unknown enum value maps to a fallback, so an older app keeps working
// against a newer server instead of showing nothing.

import Foundation

public enum PlaybackStatus: String, Codable, Sendable {
    case idle, playing, paused

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = PlaybackStatus(rawValue: raw) ?? .idle
    }
}

public struct TrackID: Codable, Hashable, Sendable {
    /// `spotify` or `local`.
    public var kind: String
    /// Base62 Spotify id, or the file path for local tracks.
    public var id: String

    public init(kind: String, id: String) {
        self.kind = kind
        self.id = id
    }
}

public struct TrackRef: Codable, Hashable, Sendable {
    public var id: TrackID
    public var isrc: String?
    public var title: String
    public var artists: [String]
    public var album: String?
    public var durationMs: Int
    /// A URL, or a server path such as `/media/library/<key>/cover.jpg`.
    public var art: String?

    public var artistLine: String { artists.joined(separator: ", ") }

    enum CodingKeys: String, CodingKey {
        case id, isrc, title, artists, album, art
        case durationMs = "duration_ms"
    }

    public init(
        id: TrackID, isrc: String? = nil, title: String, artists: [String],
        album: String? = nil, durationMs: Int, art: String? = nil
    ) {
        self.id = id
        self.isrc = isrc
        self.title = title
        self.artists = artists
        self.album = album
        self.durationMs = durationMs
        self.art = art
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(TrackID.self, forKey: .id)
        isrc = try c.decodeIfPresent(String.self, forKey: .isrc)
        title = try c.decodeIfPresent(String.self, forKey: .title) ?? ""
        artists = try c.decodeIfPresent([String].self, forKey: .artists) ?? []
        album = try c.decodeIfPresent(String.self, forKey: .album)
        durationMs = try c.decodeIfPresent(Int.self, forKey: .durationMs) ?? 0
        art = try c.decodeIfPresent(String.self, forKey: .art)
    }
}

public struct LyricWord: Codable, Hashable, Sendable {
    public var timeMs: Int
    public var text: String

    enum CodingKeys: String, CodingKey {
        case text
        case timeMs = "time_ms"
    }
}

public struct LyricLine: Codable, Hashable, Sendable {
    public var timeMs: Int
    public var text: String
    /// Word timings when known; empty means the line has none.
    public var words: [LyricWord]

    enum CodingKeys: String, CodingKey {
        case text, words
        case timeMs = "time_ms"
    }

    public init(timeMs: Int, text: String, words: [LyricWord] = []) {
        self.timeMs = timeMs
        self.text = text
        self.words = words
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        timeMs = try c.decode(Int.self, forKey: .timeMs)
        text = try c.decodeIfPresent(String.self, forKey: .text) ?? ""
        words = try c.decodeIfPresent([LyricWord].self, forKey: .words) ?? []
    }
}

public struct Lyrics: Codable, Hashable, Sendable {
    public var provider: String
    /// Empty when only plain (unsynced) lyrics exist.
    public var lines: [LyricLine]
    public var plain: String?
    public var offsetMs: Int

    public var isSynced: Bool { !lines.isEmpty }

    enum CodingKeys: String, CodingKey {
        case provider, lines, plain
        case offsetMs = "offset_ms"
    }

    public init(provider: String, lines: [LyricLine], plain: String? = nil, offsetMs: Int = 0) {
        self.provider = provider
        self.lines = lines
        self.plain = plain
        self.offsetMs = offsetMs
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        provider = try c.decodeIfPresent(String.self, forKey: .provider) ?? ""
        lines = try c.decodeIfPresent([LyricLine].self, forKey: .lines) ?? []
        plain = try c.decodeIfPresent(String.self, forKey: .plain)
        offsetMs = try c.decodeIfPresent(Int.self, forKey: .offsetMs) ?? 0
    }

    /// Index of the line being sung at `positionMs` (offset applied).
    public func lineIndex(at positionMs: Int) -> Int? {
        let t = positionMs - offsetMs
        var found: Int?
        for (i, line) in lines.enumerated() {
            if line.timeMs <= t { found = i } else { break }
        }
        return found
    }
}

public struct StemsProgress: Codable, Hashable, Sendable {
    public var readyMs: Int
    public var totalMs: Int
    public var done: Bool
    /// Separation speed as a multiple of real time (0 until measured).
    public var speed: Double

    enum CodingKeys: String, CodingKey {
        case done, speed
        case readyMs = "ready_ms"
        case totalMs = "total_ms"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        readyMs = try c.decodeIfPresent(Int.self, forKey: .readyMs) ?? 0
        totalMs = try c.decodeIfPresent(Int.self, forKey: .totalMs) ?? 0
        done = try c.decodeIfPresent(Bool.self, forKey: .done) ?? false
        speed = try c.decodeIfPresent(Double.self, forKey: .speed) ?? 0
    }
}

public struct KaraokeState: Codable, Hashable, Sendable {
    /// The vocal-reduced signal is playing.
    public var enabled: Bool
    /// 0 = vocals removed … 1 = original level, while enabled.
    public var vocalGain: Double
    public var backend: String?
    public var stems: StemsProgress?

    enum CodingKeys: String, CodingKey {
        case enabled, backend, stems
        case vocalGain = "vocal_gain"
    }

    public init(enabled: Bool = false, vocalGain: Double = 0, backend: String? = nil, stems: StemsProgress? = nil) {
        self.enabled = enabled
        self.vocalGain = vocalGain
        self.backend = backend
        self.stems = stems
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        enabled = try c.decodeIfPresent(Bool.self, forKey: .enabled) ?? false
        vocalGain = try c.decodeIfPresent(Double.self, forKey: .vocalGain) ?? 0
        backend = try c.decodeIfPresent(String.self, forKey: .backend)
        stems = try c.decodeIfPresent(StemsProgress.self, forKey: .stems)
    }
}

public enum ViewMode: String, Codable, CaseIterable, Sendable {
    case lyrics, karaoke, video, canvas

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = ViewMode(rawValue: raw) ?? .lyrics
    }
}

public struct Media: Codable, Hashable, Sendable {
    public var canvasUrl: String?
    public var videoUrl: String?
    public var videoOffsetMs: Int

    enum CodingKeys: String, CodingKey {
        case canvasUrl = "canvas_url"
        case videoUrl = "video_url"
        case videoOffsetMs = "video_offset_ms"
    }

    public init() {
        videoOffsetMs = 0
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        canvasUrl = try c.decodeIfPresent(String.self, forKey: .canvasUrl)
        videoUrl = try c.decodeIfPresent(String.self, forKey: .videoUrl)
        videoOffsetMs = try c.decodeIfPresent(Int.self, forKey: .videoOffsetMs) ?? 0
    }
}

/// `sing`: karaoke turns on when it starts; `play`: the full song.
public enum QueueKind: String, Codable, CaseIterable, Sendable {
    case sing, play

    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = QueueKind(rawValue: raw) ?? .sing
    }
}

public struct QueueItem: Codable, Hashable, Identifiable, Sendable {
    /// Unique per queue entry (the same song can be queued twice).
    public var id: Int
    /// Library key of the track.
    public var key: String
    public var track: TrackRef
    public var singer: String?
    public var kind: QueueKind

    public init(id: Int, key: String, track: TrackRef, singer: String? = nil, kind: QueueKind = .sing) {
        self.id = id
        self.key = key
        self.track = track
        self.singer = singer
        self.kind = kind
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(Int.self, forKey: .id)
        key = try c.decode(String.self, forKey: .key)
        track = try c.decode(TrackRef.self, forKey: .track)
        singer = try c.decodeIfPresent(String.self, forKey: .singer)
        kind = try c.decodeIfPresent(QueueKind.self, forKey: .kind) ?? .sing
    }
}

public struct PlaybackState: Codable, Hashable, Sendable {
    /// Active source (`library`, `spotify`, `file`), if any.
    public var source: String?
    public var status: PlaybackStatus
    public var track: TrackRef?
    public var nextTrack: TrackRef?
    /// Up next (library playback).
    public var queue: [QueueItem]
    public var libraryKey: String?
    /// The queue entry playing now.
    public var current: QueueItem?
    /// Background music between requests.
    public var intermission: Bool
    public var lyrics: Lyrics?
    public var karaoke: KaraokeState
    public var viewMode: ViewMode
    public var media: Media
    /// Output volume 0…1.
    public var volume: Double
    public var revision: Int

    enum CodingKeys: String, CodingKey {
        case source, status, track, queue, current, intermission, lyrics, karaoke, media, volume, revision
        case nextTrack = "next_track"
        case libraryKey = "library_key"
        case viewMode = "view_mode"
    }

    public static let empty = PlaybackState()

    public init() {
        status = .idle
        queue = []
        intermission = false
        karaoke = KaraokeState()
        viewMode = .lyrics
        media = Media()
        volume = 1
        revision = 0
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        source = try c.decodeIfPresent(String.self, forKey: .source)
        status = try c.decodeIfPresent(PlaybackStatus.self, forKey: .status) ?? .idle
        track = try c.decodeIfPresent(TrackRef.self, forKey: .track)
        nextTrack = try c.decodeIfPresent(TrackRef.self, forKey: .nextTrack)
        queue = try c.decodeIfPresent([QueueItem].self, forKey: .queue) ?? []
        libraryKey = try c.decodeIfPresent(String.self, forKey: .libraryKey)
        current = try c.decodeIfPresent(QueueItem.self, forKey: .current)
        intermission = try c.decodeIfPresent(Bool.self, forKey: .intermission) ?? false
        lyrics = try c.decodeIfPresent(Lyrics.self, forKey: .lyrics)
        karaoke = try c.decodeIfPresent(KaraokeState.self, forKey: .karaoke) ?? KaraokeState()
        viewMode = try c.decodeIfPresent(ViewMode.self, forKey: .viewMode) ?? .lyrics
        media = try c.decodeIfPresent(Media.self, forKey: .media) ?? Media()
        volume = try c.decodeIfPresent(Double.self, forKey: .volume) ?? 1
        revision = try c.decodeIfPresent(Int.self, forKey: .revision) ?? 0
    }

    /// Library playback: the queue and request controls apply.
    public var isLibrary: Bool { source == "library" }
}

public struct ClockTick: Codable, Hashable, Sendable {
    public var revision: Int
    public var status: PlaybackStatus
    /// Audible position in the current track.
    public var positionMs: Int
    /// Server monotonic time when `positionMs` was sampled.
    public var serverMs: Int

    enum CodingKeys: String, CodingKey {
        case revision, status
        case positionMs = "position_ms"
        case serverMs = "server_ms"
    }

    public init(revision: Int, status: PlaybackStatus, positionMs: Int, serverMs: Int) {
        self.revision = revision
        self.status = status
        self.positionMs = positionMs
        self.serverMs = serverMs
    }
}

/// Commands to the engine (`POST /api/command` or over the WebSocket).
public enum Command: Encodable, Hashable, Sendable {
    case setKaraoke(Bool)
    case setVocalGain(Double)
    case setVolume(Double)
    case setLyricsOffset(Int)
    case setViewMode(ViewMode)
    case setVideoOffset(Int)
    case play
    case pause
    case next
    case seek(positionMs: Int)
    case playTrack(key: String, singer: String?, kind: QueueKind)
    case enqueue(key: String, singer: String?, next: Bool, kind: QueueKind)
    case dequeue(id: Int)
    case moveInQueue(id: Int, to: Int)
    case clearQueue

    private struct Key: CodingKey {
        var stringValue: String
        var intValue: Int? { nil }
        init(_ s: String) { stringValue = s }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { nil }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Key.self)
        func cmd(_ name: String) throws { try c.encode(name, forKey: Key("cmd")) }
        switch self {
        case .setKaraoke(let on):
            try cmd("set_karaoke")
            try c.encode(on, forKey: Key("enabled"))
        case .setVocalGain(let g):
            try cmd("set_vocal_gain")
            try c.encode(g, forKey: Key("gain"))
        case .setVolume(let v):
            try cmd("set_volume")
            try c.encode(v, forKey: Key("level"))
        case .setLyricsOffset(let ms):
            try cmd("set_lyrics_offset")
            try c.encode(ms, forKey: Key("offset_ms"))
        case .setViewMode(let m):
            try cmd("set_view_mode")
            try c.encode(m, forKey: Key("mode"))
        case .setVideoOffset(let ms):
            try cmd("set_video_offset")
            try c.encode(ms, forKey: Key("offset_ms"))
        case .play: try cmd("play")
        case .pause: try cmd("pause")
        case .next: try cmd("next")
        case .seek(let ms):
            try cmd("seek")
            try c.encode(ms, forKey: Key("position_ms"))
        case .playTrack(let key, let singer, let kind):
            try cmd("play_track")
            try c.encode(key, forKey: Key("key"))
            try c.encodeIfPresent(singer, forKey: Key("singer"))
            try c.encode(kind, forKey: Key("kind"))
        case .enqueue(let key, let singer, let next, let kind):
            try cmd("enqueue")
            try c.encode(key, forKey: Key("key"))
            try c.encodeIfPresent(singer, forKey: Key("singer"))
            try c.encode(next, forKey: Key("next"))
            try c.encode(kind, forKey: Key("kind"))
        case .dequeue(let id):
            try cmd("dequeue")
            try c.encode(id, forKey: Key("id"))
        case .moveInQueue(let id, let to):
            try cmd("move_in_queue")
            try c.encode(id, forKey: Key("id"))
            try c.encode(to, forKey: Key("to"))
        case .clearQueue: try cmd("clear_queue")
        }
    }
}

/// One WebSocket message from the server.
public enum Inbound: Decodable, Sendable {
    case state(PlaybackState)
    case clock(ClockTick)
    /// The machines view (hosts only), about once a second.
    case status(MachineStatus)
    case error(String)
    case unknown(String)

    private enum Keys: String, CodingKey { case type, message }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "state": self = .state(try PlaybackState(from: decoder))
        case "clock": self = .clock(try ClockTick(from: decoder))
        case "status": self = .status(try MachineStatus(from: decoder))
        case "error": self = .error(try c.decodeIfPresent(String.self, forKey: .message) ?? "")
        default: self = .unknown(type)
        }
    }
}

public enum Role: String, Codable, Sendable {
    case host, guest
}

/// Guest-grantable permissions, in the order a UI lists them.
public enum Permission: String, Codable, CaseIterable, Identifiable, Sendable {
    case karaoke, vocals, volume, stage, timing, queue

    public var id: String { rawValue }
}

public struct Me: Codable, Hashable, Sendable {
    public var role: Role
    /// Unknown permission names from a newer server are dropped.
    public var permissions: [Permission]

    enum CodingKeys: String, CodingKey { case role, permissions }

    public init(role: Role, permissions: [Permission]) {
        self.role = role
        self.permissions = permissions
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        role = (try? c.decode(Role.self, forKey: .role)) ?? .guest
        let raw = try c.decodeIfPresent([String].self, forKey: .permissions) ?? []
        permissions = raw.compactMap(Permission.init(rawValue:))
    }
}

public struct ServerInfo: Codable, Hashable, Sendable {
    public var deviceName: String
    /// Best guess at the server's LAN address, `host:port`.
    public var lanHost: String?
    public var remoteUrl: String?
    public var stageUrl: String?

    enum CodingKeys: String, CodingKey {
        case deviceName = "device_name"
        case lanHost = "lan_host"
        case remoteUrl = "remote_url"
        case stageUrl = "stage_url"
    }
}
