// The indexed library (library.sqlite on the server): pages of songs with
// search/sort/filter, playlists, genres, one song in detail.

import Foundation

public struct SongRow: Codable, Hashable, Identifiable, Sendable {
    public var key: String
    public var title: String
    /// "A, B" for display.
    public var artists: String
    public var album: String?
    public var year: Int?
    public var durationMs: Int
    public var bpm: Double?
    public var musicalKey: String?
    public var danceability: Double?
    /// Top genre, e.g. `Rock`.
    public var genre: String?
    /// Most likely style, e.g. `Rock / Alternative Rock`.
    public var style: String?
    public var plays: Int
    /// Unix seconds.
    public var lastPlayed: Int?
    public var playable: Bool
    public var hasLyrics: Bool
    public var hasWords: Bool
    public var hasVideo: Bool
    public var popularity: Int?
    public var addedAt: Int
    /// Place in the playlist being listed.
    public var position: Int?

    public var id: String { key }
    /// The style without its genre prefix ("Alternative Rock").
    public var styleName: String? { style.map { $0.components(separatedBy: " / ").last ?? $0 } }

    enum CodingKeys: String, CodingKey {
        case key, title, artists, album, year, bpm, danceability, genre, style, plays, playable, popularity, position
        case durationMs = "duration_ms"
        case musicalKey = "musical_key"
        case lastPlayed = "last_played"
        case hasLyrics = "has_lyrics"
        case hasWords = "has_words"
        case hasVideo = "has_video"
        case addedAt = "added_at"
    }
}

public struct SongPage: Codable, Hashable, Sendable {
    public var total: Int
    public var offset: Int
    public var songs: [SongRow]
}

public enum SongSort: String, CaseIterable, Sendable {
    case `default`, title, artist, album, year, duration, bpm, key, danceability, genre, plays
    case lastPlayed = "last_played"
    case added, popularity, position
}

/// What to list: everything, a playlist, a genre…, searched and sorted.
public struct SongQuery: Hashable, Sendable {
    public var q = ""
    public var sort: SongSort = .default
    public var desc = false
    public var playable = false
    public var playlist: String?
    public var genre: String?
    public var artist: String?
    public var yearFrom: Int?
    public var yearTo: Int?
    public var bpmFrom: Double?
    public var bpmTo: Double?

    public init(playlist: String? = nil, genre: String? = nil) {
        self.playlist = playlist
        self.genre = genre
    }

    func items(limit: Int, offset: Int) -> [URLQueryItem] {
        var out = [
            URLQueryItem(name: "limit", value: String(limit)),
            URLQueryItem(name: "offset", value: String(offset)),
        ]
        func add(_ k: String, _ v: String?) { if let v, !v.isEmpty { out.append(URLQueryItem(name: k, value: v)) } }
        add("q", q.trimmingCharacters(in: .whitespaces))
        if sort != .default { add("sort", sort.rawValue) }
        if desc { add("desc", "1") }
        if playable { add("playable", "1") }
        add("playlist", playlist)
        add("genre", genre)
        add("artist", artist)
        add("year_from", yearFrom.map(String.init))
        add("year_to", yearTo.map(String.init))
        add("bpm_from", bpmFrom.map { String($0) })
        add("bpm_to", bpmTo.map { String($0) })
        return out
    }
}

public struct PlaylistRow: Codable, Hashable, Identifiable, Sendable {
    public var id: String
    /// `playlist` or `album`.
    public var kind: String
    public var name: String
    public var url: String?
    public var description: String
    public var songs: Int
    /// Of those, in the library and ready.
    public var playable: Int

    public var displayName: String { name.trimmingCharacters(in: .whitespaces) }
}

public struct GenreCount: Codable, Hashable, Identifiable, Sendable {
    public var name: String
    public var songs: Int
    public var id: String { name }
}

public struct Play: Codable, Hashable, Sendable {
    public var key: String
    /// Unix seconds.
    public var at: Int
    public var playedMs: Int
    public var completed: Bool
    public var kind: String
    public var singer: String?
    public var background: Bool

    enum CodingKeys: String, CodingKey {
        case key, at, completed, kind, singer, background
        case playedMs = "played_ms"
    }
}

/// One song in detail: the library entry plus what links to it.
public struct SongDetail: Decodable, Hashable, Sendable {
    public var track: LibraryTrack
    public var info: TrackInfo
    public var analysis: Analysis?
    public var lyricsText: String?
    public var playlists: [PlaylistRow]
    public var plays: [Play]

    enum CodingKeys: String, CodingKey {
        case info, analysis, playlists, plays
        case lyricsText = "lyrics_text"
    }

    public init(from decoder: Decoder) throws {
        track = try LibraryTrack(from: decoder)
        let c = try decoder.container(keyedBy: CodingKeys.self)
        info = try c.decodeIfPresent(TrackInfo.self, forKey: .info) ?? TrackInfo()
        analysis = try c.decodeIfPresent(Analysis.self, forKey: .analysis)
        lyricsText = try c.decodeIfPresent(String.self, forKey: .lyricsText)
        playlists = try c.decodeIfPresent([PlaylistRow].self, forKey: .playlists) ?? []
        plays = try c.decodeIfPresent([Play].self, forKey: .plays) ?? []
    }
}

public struct TrackInfo: Codable, Hashable, Sendable {
    public var releaseDate: String?
    public var label: String?
    public var popularity: Int?
    public var explicit: Bool?
    public var languages: [String]

    enum CodingKeys: String, CodingKey {
        case label, popularity, explicit, languages
        case releaseDate = "release_date"
    }

    public init() { languages = [] }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        releaseDate = try c.decodeIfPresent(String.self, forKey: .releaseDate)
        label = try c.decodeIfPresent(String.self, forKey: .label)
        popularity = try c.decodeIfPresent(Int.self, forKey: .popularity)
        explicit = try c.decodeIfPresent(Bool.self, forKey: .explicit)
        languages = try c.decodeIfPresent([String].self, forKey: .languages) ?? []
    }
}

public struct Analysis: Codable, Hashable, Sendable {
    public struct Genre: Codable, Hashable, Sendable {
        public var name: String
        public var score: Double
    }

    public var bpm: Double?
    public var key: String?
    public var danceability: Double?
    public var moods: [String: Double]
    public var genres: [Genre]
    public var vocals: Double?

    enum CodingKeys: String, CodingKey { case bpm, key, danceability, moods, genres, vocals }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        bpm = try c.decodeIfPresent(Double.self, forKey: .bpm)
        key = try c.decodeIfPresent(String.self, forKey: .key)
        danceability = try c.decodeIfPresent(Double.self, forKey: .danceability)
        moods = try c.decodeIfPresent([String: Double].self, forKey: .moods) ?? [:]
        genres = try c.decodeIfPresent([Genre].self, forKey: .genres) ?? []
        vocals = try c.decodeIfPresent(Double.self, forKey: .vocals)
    }
}

extension API {
    /// One page (the server caps `limit` at 1000).
    public func songs(_ query: SongQuery, limit: Int = 500, offset: Int = 0) async throws -> SongPage {
        try await getJSON("/api/library/songs", query: query.items(limit: limit, offset: offset))
    }

    public func song(_ key: String) async throws -> SongDetail {
        try await getJSON("/api/library/songs/\(key)")
    }

    public func playlists() async throws -> [PlaylistRow] { try await getJSON("/api/library/playlists") }
    public func genres() async throws -> [GenreCount] { try await getJSON("/api/library/genres") }
}
