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
    @Environment(AppModel.self) private var app
    @Environment(\.openWindow) private var openWindow
    @State private var trashing: [SongRow] = []
    @State private var model: SongsModel
    @State private var selection = Set<String>()
    @State private var sortOrder: [KeyPathComparator<SongRow>]
    @State private var requesting: RequestTarget?
    let title: String
    /// From the screen's one search field (a toolbar search per table
    /// crashes AppKit when tables are swapped).
    let search: String
    /// Shown when the list is empty without a search.
    let emptyHint: String?
    /// The song the inspector shows.
    @Binding var selected: String?
    let playlistActions: PlaylistActions?

    init(
        title: String, query: SongQuery, search: String = "", emptyHint: String? = nil,
        selected: Binding<String?> = .constant(nil), playlistActions: PlaylistActions? = nil
    ) {
        self.title = title
        self.search = search
        self.emptyHint = emptyHint
        _selected = selected
        self.playlistActions = playlistActions
        _model = State(initialValue: SongsModel(query: query))
        _sortOrder = State(initialValue: query.playlist != nil ? [KeyPathComparator(\SongRow.sPos)] : [KeyPathComparator(\SongRow.artists)])
    }

    var body: some View {
        Table(model.rows, selection: $selection, sortOrder: $sortOrder) {
            nameColumns
            detailColumns
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
        .confirmationDialog(
            trashing.count == 1 ? "Move “\(trashing.first?.title ?? "")” to the library trash?" : "Move \(trashing.count) songs to the library trash?",
            isPresented: Binding(get: { !trashing.isEmpty }, set: { if !$0 { trashing = [] } })
        ) {
            Button("Move to Trash", role: .destructive) {
                let rows = trashing
                Task {
                    for r in rows { try? await client.api.deleteSong(r.key) }
                    await model.load(client)
                }
            }
        } message: {
            Text("They leave the library and its playlists. The files go to the library’s trash folder, so you can put them back.")
        }
        .task(id: model.query) { await model.load(client) }
        .task(id: search) {
            try? await Task.sleep(for: .milliseconds(250))  // debounce typing
            model.query.q = search
        }
        .onChange(of: selection) { _, sel in
            if let first = sel.first { selected = first }
        }
        .onChange(of: sortOrder) { _, order in
            guard let first = order.first else { return }
            model.query.sort = Self.sort(for: first.keyPath)
            model.query.desc = first.order == .reverse
        }
    }

    @TableColumnBuilder<SongRow, KeyPathComparator<SongRow>>
    private var nameColumns: some TableColumnContent<SongRow, KeyPathComparator<SongRow>> {
        TableColumn("#", value: \.sPos) { s in
            Text(s.position.map { String($0 + 1) } ?? "").foregroundStyle(.secondary).monospacedDigit()
        }
        .width(min: 24, ideal: 30, max: 44)
        .defaultVisibility(model.query.playlist == nil ? .hidden : .visible)
        TableColumn("Title", value: \.title) { s in
            Text(s.title).lineLimit(1).opacity(s.playable ? 1 : 0.45)
        }
        .width(min: 140, ideal: 240)
        TableColumn("Karaoke") { s in
            Image(systemName: s.playable ? "music.mic" : "hourglass")
                .foregroundStyle(s.playable ? AnyShapeStyle(.tint) : AnyShapeStyle(.tertiary))
                .help(s.playable ? "Ready for karaoke" : "Not ready yet (still importing, or an import step failed)")
                .frame(maxWidth: .infinity)
        }
        .width(min: 28, ideal: 52, max: 60)
        TableColumn("Lyrics") { s in
            Group {
                if s.hasWords {
                    Image(systemName: "text.bubble.fill").foregroundStyle(.secondary).help("Word-by-word timing")
                } else if s.hasLyrics {
                    Image(systemName: "text.alignleft").foregroundStyle(.secondary).help("Line timing")
                }
            }
            .frame(maxWidth: .infinity)
        }
        .width(min: 28, ideal: 44, max: 56)
        TableColumn("Video") { s in
            Group {
                if s.hasVideo { Image(systemName: "film").foregroundStyle(.secondary).help("Music video") }
            }
            .frame(maxWidth: .infinity)
        }
        .width(min: 28, ideal: 42, max: 56)
        TableColumn("Explicit") { s in
            Group {
                if s.explicit == true { Image(systemName: "e.square.fill").foregroundStyle(.secondary).help("Explicit") }
            }
            .frame(maxWidth: .infinity)
        }
        .width(min: 28, ideal: 50, max: 60)
    }

    @TableColumnBuilder<SongRow, KeyPathComparator<SongRow>>
    private var detailColumns: some TableColumnContent<SongRow, KeyPathComparator<SongRow>> {
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
                ContentUnavailableView(
                    "No Songs", systemImage: "music.note.list", description: emptyHint.map { Text($0) })
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
        let all = model.rows.filter { keys.contains($0.key) }
        if all.count == 1, let r = all.first {
            Divider()
            Button("Preview…") { openWindow(id: "preview", value: r.key) }
            Button("Show Info") { selected = r.key }
        }
        if let actions = playlistActions {
            let keys = all.map(\.key)
            Divider()
            Menu("Add to Playlist") {
                ForEach(actions.local) { p in
                    Button(p.displayName) { actions.add(p.id, keys) }
                }
                if !actions.local.isEmpty { Divider() }
                Button("New Playlist…") { actions.newPlaylist(keys) }
            }
            .disabled(keys.isEmpty)
            if let remove = actions.removeFromCurrent {
                Button(keys.count > 1 ? "Remove \(keys.count) Songs from Playlist" : "Remove from Playlist") {
                    Task {
                        await remove(keys)
                        await model.load(client)
                    }
                }
            }
        }
        let spotify = all.compactMap(\.spotifyURL)
        if !spotify.isEmpty {
            Divider()
            if all.count == 1, let url = spotify.first {
                Button("Open in Spotify") { NSWorkspace.shared.open(url) }
            }
            Button(spotify.count > 1 ? "Copy Spotify Links" : "Copy Spotify Link") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(spotify.map(\.absoluteString).joined(separator: "\n"), forType: .string)
            }
            let unfinished = all.filter { !$0.playable || !$0.hasLyrics || !$0.hasVideo }.compactMap(\.spotifyURL)
            Button("Retry Missing Parts") {
                Task { try? await client.api.importSources(unfinished.map(\.absoluteString)) }
            }
            .disabled(unfinished.isEmpty)
            .help("Import again: only the steps that are missing or failed are redone")
        }
        if let dir = app.supervisor.config?.libraryDir {
            Button("Show in Finder") {
                NSWorkspace.shared.activateFileViewerSelecting(
                    all.map { URL(fileURLWithPath: dir).appending(path: "tracks/\($0.key)") })
            }
            .disabled(all.isEmpty)
        }
        Divider()
        Button(all.count > 1 ? "Move \(all.count) Songs to Trash…" : "Move to Trash…", role: .destructive) { trashing = all }
            .disabled(all.isEmpty)
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

extension SongRow {
    /// The song on Spotify, for songs imported from there.
    var spotifyURL: URL? {
        guard key.hasPrefix("spotify_") else { return nil }
        return URL(string: "https://open.spotify.com/track/\(key.dropFirst("spotify_".count))")
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
