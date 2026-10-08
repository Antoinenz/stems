// The songs table: every song (or one playlist / genre), searched on the
// server's index, sortable by any column, double-click to request.

import StemsKit
import StemsUI
import SwiftUI

@MainActor
@Observable
final class SongsModel {
    var query: SongQuery
    private(set) var rows: [SongRow] = []
    private(set) var total = 0
    private(set) var loading = false
    var error: String?
    private var generation = 0

    init(query: SongQuery) { self.query = query }

    /// Every page for the current query (the library is a few thousand
    /// songs at most: two or three requests).
    func load(_ client: StemsClient) async {
        generation += 1
        let mine = generation
        loading = true
        defer { if mine == generation { loading = false } }
        do {
            var all: [SongRow] = []
            var total = 0
            repeat {
                let page = try await client.api.songs(query, limit: 1000, offset: all.count)
                guard mine == generation else { return }  // a newer query took over
                total = page.total
                all += page.songs
                if page.songs.isEmpty { break }
            } while all.count < total
            rows = all
            self.total = total
            error = nil
        } catch {
            if mine == generation { self.error = error.localizedDescription }
        }
    }
}

/// Sortable stand-ins for optional columns (unknowns sort last on the
/// server either way; these only drive the header arrows).
extension SongRow {
    var sYear: Int { year ?? 0 }
    var sBpm: Double { bpm ?? 0 }
    var sKey: String { musicalKey ?? "" }
    var sDance: Double { danceability ?? 0 }
    var sStyle: String { style ?? "" }
    var sLast: Int { lastPlayed ?? 0 }
    var sPos: Int { position ?? 0 }
}

struct SongsView: View {
    @Environment(StemsClient.self) private var client
    @State private var model: SongsModel
    @State private var selection = Set<String>()
    @State private var sortOrder: [KeyPathComparator<SongRow>]
    @State private var requesting: RequestTarget?
    let title: String
    /// From the screen's one search field (a toolbar search per table
    /// crashes AppKit when tables are swapped).
    let search: String

    init(title: String, query: SongQuery, search: String = "") {
        self.title = title
        self.search = search
        _model = State(initialValue: SongsModel(query: query))
        _sortOrder = State(initialValue: query.playlist != nil ? [KeyPathComparator(\SongRow.sPos)] : [KeyPathComparator(\SongRow.artists)])
    }

    var body: some View {
        Table(model.rows, selection: $selection, sortOrder: $sortOrder) {
            TableColumn("#", value: \.sPos) { s in
                Text(s.position.map { String($0 + 1) } ?? "").foregroundStyle(.secondary).monospacedDigit()
            }
            .width(min: 24, ideal: 30, max: 44)
            .defaultVisibility(model.query.playlist == nil ? .hidden : .visible)
            TableColumn("Title", value: \.title) { s in
                HStack(spacing: 6) {
                    Text(s.title).lineLimit(1)
                    if s.hasWords {
                        Image(systemName: "text.bubble").foregroundStyle(.secondary).help("Word-timed lyrics")
                    } else if s.hasLyrics {
                        Image(systemName: "text.alignleft").foregroundStyle(.tertiary).help("Line-timed lyrics")
                    }
                    if s.hasVideo { Image(systemName: "film").foregroundStyle(.secondary).help("Music video") }
                }
                .opacity(s.playable ? 1 : 0.45)
            }
            .width(min: 140, ideal: 240)
            TableColumn("Artist", value: \.artists) { s in Text(s.artists).lineLimit(1) }
                .width(min: 100, ideal: 170)
            TableColumn("Year", value: \.sYear) { s in Text(s.year.map(String.init) ?? "").monospacedDigit() }
                .width(min: 36, ideal: 44, max: 60)
            TableColumn("Time", value: \.durationMs) { s in Text(Fmt.time(ms: s.durationMs)).monospacedDigit() }
                .width(min: 40, ideal: 48, max: 60)
            TableColumn("BPM", value: \.sBpm) { s in Text(s.bpm.map { String(Int($0.rounded())) } ?? "").monospacedDigit() }
                .width(min: 34, ideal: 42, max: 56)
            TableColumn("Key", value: \.sKey) { s in Text(s.musicalKey ?? "").lineLimit(1) }
                .width(min: 50, ideal: 70, max: 90)
            TableColumn("Dance", value: \.sDance) { s in DanceBar(value: s.danceability) }
                .width(min: 50, ideal: 70, max: 100)
            TableColumn("Style", value: \.sStyle) { s in
                Text(s.styleName ?? "").lineLimit(1).help(s.style ?? "")
            }
            .width(min: 80, ideal: 140)
            TableColumn("Plays", value: \.plays) { s in
                Text(s.plays > 0 ? String(s.plays) : "")
                    .monospacedDigit()
                    .help(s.lastPlayed.map { "Last played " + Fmt.ago(unix: $0) } ?? "Never played")
            }
            .width(min: 34, ideal: 42, max: 60)
        }
        .contextMenu(forSelectionType: String.self) { keys in
            menu(for: keys)
        } primaryAction: { keys in
            if let row = row(keys.first) { requesting = RequestTarget(row) }
        }
        .overlay { emptyState }
        .navigationTitle(title)
        .navigationSubtitle(subtitle)
        .sheet(item: $requesting) { RequestSheet(track: $0).frame(minWidth: 420, minHeight: 380) }
        .task(id: model.query) { await model.load(client) }
        .task(id: search) {
            try? await Task.sleep(for: .milliseconds(250))  // debounce typing
            model.query.q = search
        }
        .onChange(of: sortOrder) { _, order in
            guard let first = order.first else { return }
            model.query.sort = Self.sort(for: first.keyPath)
            model.query.desc = first.order == .reverse
        }
    }

    private var subtitle: String {
        if model.loading && model.rows.isEmpty { return "Loading…" }
        let ready = model.rows.lazy.filter(\.playable).count
        return model.total == ready ? "\(model.total) songs" : "\(model.total) songs, \(ready) ready"
    }

    @ViewBuilder
    private var emptyState: some View {
        if let e = model.error, model.rows.isEmpty {
            ContentUnavailableView("Library Unavailable", systemImage: "externaldrive.badge.exclamationmark", description: Text(e))
        } else if model.rows.isEmpty && !model.loading {
            if search.isEmpty {
                ContentUnavailableView("No Songs", systemImage: "music.note.list")
            } else {
                ContentUnavailableView.search(text: search)
            }
        }
    }

    private func row(_ key: String?) -> SongRow? {
        guard let key else { return nil }
        return model.rows.first { $0.key == key }
    }

    @ViewBuilder
    private func menu(for keys: Set<String>) -> some View {
        let rows = model.rows.filter { keys.contains($0.key) && $0.playable }
        if rows.count == 1, let r = rows.first {
            Button("Sing…") { requesting = RequestTarget(r) }
        }
        Button(rows.count > 1 ? "Add \(rows.count) Songs to Queue" : "Add to Queue") {
            for r in rows { client.send(.enqueue(key: r.key, singer: nil, next: false, kind: .play)) }
        }
        .disabled(rows.isEmpty)
        Button("Play Next") {
            for r in rows.reversed() { client.send(.enqueue(key: r.key, singer: nil, next: true, kind: .play)) }
        }
        .disabled(rows.isEmpty)
        if rows.count == 1, let r = rows.first {
            Divider()
            Button("Play Now") { client.send(.playTrack(key: r.key, singer: nil, kind: .play)) }
        }
    }

    private static func sort(for keyPath: PartialKeyPath<SongRow>) -> SongSort {
        switch keyPath {
        case \SongRow.title: .title
        case \SongRow.artists: .artist
        case \SongRow.sYear: .year
        case \SongRow.durationMs: .duration
        case \SongRow.sBpm: .bpm
        case \SongRow.sKey: .key
        case \SongRow.sDance: .danceability
        case \SongRow.sStyle: .genre
        case \SongRow.plays: .plays
        case \SongRow.sLast: .lastPlayed
        case \SongRow.sPos: .position
        default: .default
        }
    }
}

/// Danceability as a small bar (with the number on hover).
struct DanceBar: View {
    var value: Double?

    var body: some View {
        if let v = value {
            Gauge(value: v) { EmptyView() }
                .gaugeStyle(.accessoryLinearCapacity)
                .tint(.accentColor)
                .help("\(Int((v * 100).rounded()))% danceable")
                .accessibilityLabel("Danceability")
                .accessibilityValue("\(Int((v * 100).rounded())) percent")
        }
    }
}

enum Fmt {
    static func time(ms: Int) -> String {
        let s = max(0, ms / 1000)
        return String(format: "%d:%02d", s / 60, s % 60)
    }

    static func ago(unix: Int) -> String {
        RelativeDateTimeFormatter().localizedString(for: Date(timeIntervalSince1970: TimeInterval(unix)), relativeTo: .now)
    }
}
