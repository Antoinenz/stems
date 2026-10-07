// `GET /api/library` and `/api/library/import` (crates/stemsd/src/library.rs).

import Foundation

/// One step of importing a track.
public enum StageState: Hashable, Sendable {
    case pending, done, missing, skipped
    case failed(String)
    case unknown(String)

    public var isDone: Bool { self == .done }
    public var isFailed: Bool { if case .failed = self { true } else { false } }
}

extension StageState: Codable {
    private enum Keys: String, CodingKey { case state, error }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        switch try c.decode(String.self, forKey: .state) {
        case "pending": self = .pending
        case "done": self = .done
        case "missing": self = .missing
        case "skipped": self = .skipped
        case "failed": self = .failed(try c.decodeIfPresent(String.self, forKey: .error) ?? "")
        case let other: self = .unknown(other)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: Keys.self)
        switch self {
        case .pending: try c.encode("pending", forKey: .state)
        case .done: try c.encode("done", forKey: .state)
        case .missing: try c.encode("missing", forKey: .state)
        case .skipped: try c.encode("skipped", forKey: .state)
        case .failed(let e):
            try c.encode("failed", forKey: .state)
            try c.encode(e, forKey: .error)
        case .unknown(let s): try c.encode(s, forKey: .state)
        }
    }
}

/// Import steps, in the order they run.
public struct Stages: Codable, Hashable, Sendable {
    public var audio: StageState
    public var stems: StageState
    public var lyrics: StageState
    public var words: StageState
    public var video: StageState
    public var videoAlign: StageState
    public var cover: StageState
    /// Optional: tempo, key, danceability, genres.
    public var analysis: StageState

    enum CodingKeys: String, CodingKey {
        case audio, stems, lyrics, words, video, cover, analysis
        case videoAlign = "video_align"
    }

    public var all: [(name: String, state: StageState)] {
        [
            ("Audio", audio), ("Stems", stems), ("Lyrics", lyrics), ("Word timing", words),
            ("Video", video), ("Video sync", videoAlign), ("Cover", cover),
        ]
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        func get(_ k: CodingKeys) throws -> StageState {
            try c.decodeIfPresent(StageState.self, forKey: k) ?? .pending
        }
        audio = try get(.audio)
        stems = try get(.stems)
        lyrics = try get(.lyrics)
        words = try get(.words)
        video = try get(.video)
        videoAlign = try get(.videoAlign)
        cover = try get(.cover)
        analysis = try get(.analysis)
    }
}

public struct LibraryTrack: Codable, Hashable, Identifiable, Sendable {
    public var key: String
    public var title: String
    public var artists: [String]
    public var album: String?
    public var durationMs: Int
    public var art: String?
    /// Audio and stems are there: it can be queued.
    public var playable: Bool
    public var lyrics: Bool
    /// Lyrics have word timings.
    public var words: Bool
    public var video: Bool
    /// From the audio analysis, when it ran (for sorting and browsing).
    public var bpm: Double?
    public var danceability: Double?
    /// Top genre, e.g. `Hip Hop`.
    public var genre: String?
    public var year: Int?
    // Host view only:
    public var complete: Bool?
    public var stages: Stages?
    public var loudnessLufs: Double?
    public var videoOffsetMs: Int?
    public var videoCorr: Double?
    public var addedAt: Int?
    /// Which separation model made the instrumental.
    public var stemsModel: String?

    public var id: String { key }
    public var artistLine: String { artists.joined(separator: ", ") }

    enum CodingKeys: String, CodingKey {
        case key, title, artists, album, art, playable, lyrics, words, video, complete, stages
        case bpm, danceability, genre, year
        case stemsModel = "stems_model"
        case durationMs = "duration_ms"
        case loudnessLufs = "loudness_lufs"
        case videoOffsetMs = "video_offset_ms"
        case videoCorr = "video_corr"
        case addedAt = "added_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        key = try c.decode(String.self, forKey: .key)
        title = try c.decodeIfPresent(String.self, forKey: .title) ?? key
        artists = try c.decodeIfPresent([String].self, forKey: .artists) ?? []
        album = try c.decodeIfPresent(String.self, forKey: .album)
        durationMs = try c.decodeIfPresent(Int.self, forKey: .durationMs) ?? 0
        art = try c.decodeIfPresent(String.self, forKey: .art)
        playable = try c.decodeIfPresent(Bool.self, forKey: .playable) ?? false
        lyrics = try c.decodeIfPresent(Bool.self, forKey: .lyrics) ?? false
        words = try c.decodeIfPresent(Bool.self, forKey: .words) ?? false
        video = try c.decodeIfPresent(Bool.self, forKey: .video) ?? false
        complete = try c.decodeIfPresent(Bool.self, forKey: .complete)
        stages = try c.decodeIfPresent(Stages.self, forKey: .stages)
        loudnessLufs = try c.decodeIfPresent(Double.self, forKey: .loudnessLufs)
        videoOffsetMs = try c.decodeIfPresent(Int.self, forKey: .videoOffsetMs)
        videoCorr = try c.decodeIfPresent(Double.self, forKey: .videoCorr)
        addedAt = try c.decodeIfPresent(Int.self, forKey: .addedAt)
        bpm = try c.decodeIfPresent(Double.self, forKey: .bpm)
        danceability = try c.decodeIfPresent(Double.self, forKey: .danceability)
        genre = try c.decodeIfPresent(String.self, forKey: .genre)
        year = try c.decodeIfPresent(Int.self, forKey: .year)
        stemsModel = try c.decodeIfPresent(String.self, forKey: .stemsModel)
    }
}

public struct ImportStatus: Codable, Hashable, Sendable {
    public struct Finished: Codable, Hashable, Identifiable, Sendable {
        public var key: String
        public var title: String
        public var playable: Bool
        public var complete: Bool
        /// `stage: error` for every failed step.
        public var problems: [String]

        public var id: String { key }
    }

    public var running: Bool
    /// The track being imported right now.
    public var current: String?
    public var waiting: [String]
    /// Most recent first.
    public var finished: [Finished]
    /// Problems that stopped a whole batch (bad link, no Spotify login…).
    public var errors: [String]

    public init() {
        running = false
        waiting = []
        finished = []
        errors = []
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        running = try c.decodeIfPresent(Bool.self, forKey: .running) ?? false
        current = try c.decodeIfPresent(String.self, forKey: .current)
        waiting = try c.decodeIfPresent([String].self, forKey: .waiting) ?? []
        finished = try c.decodeIfPresent([Finished].self, forKey: .finished) ?? []
        errors = try c.decodeIfPresent([String].self, forKey: .errors) ?? []
    }
}
