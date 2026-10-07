// The song book: search, see what's ready, request a song, import more.

import StemsKit
import SwiftUI

@MainActor
@Observable
final class LibraryModel {
    var tracks: [LibraryTrack] = []
    var error: String?
    var loaded = false

    func load(_ client: StemsClient) async {
        do {
            tracks = try await client.api.library()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
        loaded = true
    }

    /// Every word must match the title, an artist or the album.
    func filtered(_ query: String) -> [LibraryTrack] {
        let words = query.lowercased().split(separator: " ").map(String.init)
        guard !words.isEmpty else { return tracks }
        return tracks.filter { t in
            let hay = ([t.title, t.album ?? ""] + t.artists).joined(separator: " ").lowercased()
            return words.allSatisfy { hay.localizedStandardContains($0) }
        }
    }
}

public struct LibraryView: View {
    @Environment(StemsClient.self) private var client
    @State private var model = LibraryModel()
    @State private var query = ""
    @State private var requesting: LibraryTrack?
    @State private var importing = false
    @State private var showUnready = true

    public init() {}

    public var body: some View {
        let shown = model.filtered(query).filter { showUnready || $0.playable }
        List(shown) { track in
            Button {
                requesting = track
            } label: {
                LibraryRow(track: track, showDetails: client.isHost)
            }
            .buttonStyle(.plain)
            .disabled(!track.playable)
            .swipeActions(edge: .leading, allowsFullSwipe: true) {
                Button("Sing", systemImage: "music.mic") { request(track, .sing) }.tint(.accentColor)
            }
            .swipeActions(edge: .trailing, allowsFullSwipe: true) {
                Button("Just Play", systemImage: "music.note") { request(track, .play) }.tint(.indigo)
            }
            .contextMenu { menu(for: track) }
        }
        .overlay { emptyState(shown) }
        .searchable(text: $query, prompt: "Songs, Artists, Albums")
        .navigationTitle("Library")
        .toolbar {
            if client.isHost {
                ToolbarItem {
                    Toggle(isOn: $showUnready) {
                        Label("Show Unfinished", systemImage: "line.3.horizontal.decrease")
                    }
                    .help("Show songs that are still importing or failed to import")
                }
                ToolbarItem(placement: .primaryAction) {
                    Button("Import", systemImage: "square.and.arrow.down") { importing = true }
                        .help("Add songs from Spotify links or audio files")
                }
            }
        }
        .refreshable { await model.load(client) }
        .task(id: client.connection.isConnected) {
            // imports land while the screen is open
            while !Task.isCancelled {
                await model.load(client)
                try? await Task.sleep(for: .seconds(15))
            }
        }
        .sheet(item: $requesting) { track in
            RequestSheet(track: track)
        }
        .sheet(isPresented: $importing) {
            ImportView()
                #if os(macOS)
                .frame(minWidth: 520, minHeight: 480)
                #endif
        }
    }

    @ViewBuilder
    private func emptyState(_ shown: [LibraryTrack]) -> some View {
        if !model.loaded {
            ProgressView()
        } else if let error = model.error, model.tracks.isEmpty {
            ContentUnavailableView("Library Unavailable", systemImage: "externaldrive.badge.exclamationmark", description: Text(error))
        } else if shown.isEmpty, !query.isEmpty {
            ContentUnavailableView.search(text: query)
        } else if shown.isEmpty {
            ContentUnavailableView(
                "No Songs Yet", systemImage: "music.note.list",
                description: Text(client.isHost ? "Import Spotify links or audio files to fill the song book." : "The host hasn’t added any songs yet."))
        }
    }

    @ViewBuilder
    private func menu(for track: LibraryTrack) -> some View {
        Button("Sing…", systemImage: "music.mic") { requesting = track }
        Button("Add to Queue (Just Play)", systemImage: "text.badge.plus") { request(track, .play) }
        Button("Play Next", systemImage: "text.line.first.and.arrowtriangle.forward") {
            client.send(.enqueue(key: track.key, singer: nil, next: true, kind: .play))
        }
        Divider()
        Button("Play Now", systemImage: "play.fill") {
            client.send(.playTrack(key: track.key, singer: nil, kind: .play))
        }
    }

    private func request(_ track: LibraryTrack, _ kind: QueueKind) {
        guard track.playable else { return }
        client.send(.enqueue(key: track.key, singer: nil, next: false, kind: kind))
    }
}

struct LibraryRow: View {
    @Environment(StemsClient.self) private var client
    var track: LibraryTrack
    var showDetails: Bool

    var body: some View {
        HStack(spacing: 12) {
            ArtworkView(url: client.url(for: track.art), size: 44)
            VStack(alignment: .leading, spacing: 2) {
                Text(track.title).font(.body.weight(.medium)).lineLimit(1)
                HStack(spacing: 6) {
                    Text(track.artistLine).lineLimit(1)
                    if showDetails { badges }
                }
                .font(.subheadline)
                .foregroundStyle(.secondary)
            }
            Spacer(minLength: 8)
            Text(Format.time(ms: track.durationMs))
                .font(.caption.monospacedDigit())
                .foregroundStyle(.tertiary)
        }
        .contentShape(.rect)
        .opacity(track.playable ? 1 : 0.5)
        .accessibilityElement(children: .combine)
        .accessibilityHint(track.playable ? "Request this song" : "Not ready yet")
    }

    @ViewBuilder
    private var badges: some View {
        if !track.playable {
            Badge(text: "Not Ready", warning: true)
        } else if !track.lyrics {
            Badge(text: "No Lyrics", warning: true)
        } else if !track.words {
            Badge(text: "Lines", warning: false)
        }
        if track.video { Image(systemName: "film").accessibilityLabel("Has video") }
    }
}

struct Badge: View {
    var text: LocalizedStringKey
    var warning: Bool

    var body: some View {
        Text(text)
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .foregroundStyle(warning ? AnyShapeStyle(.orange) : AnyShapeStyle(.secondary))
            .background(warning ? AnyShapeStyle(.orange.opacity(0.15)) : AnyShapeStyle(.quaternary), in: .capsule)
    }
}

/// Request one song: sing (with a name) or just play it.
struct RequestSheet: View {
    @Environment(StemsClient.self) private var client
    @Environment(\.dismiss) private var dismiss
    // starts empty: the host requests for a different person each time
    @State private var singer = ""
    @State private var kind: QueueKind = .sing
    var track: LibraryTrack

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    HStack(spacing: 14) {
                        ArtworkView(url: client.url(for: track.art), size: 64, cornerRadius: 8)
                        VStack(alignment: .leading, spacing: 2) {
                            Text(track.title).font(.headline)
                            Text(track.artistLine).foregroundStyle(.secondary)
                            if !track.lyrics {
                                Label("No lyrics for this one", systemImage: "text.badge.xmark")
                                    .font(.caption).foregroundStyle(.orange)
                            }
                        }
                    }
                }
                Section {
                    Picker("Request", selection: $kind) {
                        ForEach(QueueKind.allCases, id: \.self) { k in
                            Label(k.title, systemImage: k.symbol).tag(k)
                        }
                    }
                    .pickerStyle(.segmented)
                    if kind == .sing {
                        TextField("Who’s singing?", text: $singer)
                            .textContentType(.name)
                            .submitLabel(.done)
                            .onSubmit(add)
                    }
                } footer: {
                    Text(kind == .sing ? "Karaoke turns on when this song starts." : "The full song plays, vocals and all.")
                }
                Section {
                    Button("Play Next", systemImage: "text.line.first.and.arrowtriangle.forward") {
                        client.send(.enqueue(key: track.key, singer: who, next: true, kind: kind))
                        dismiss()
                    }
                    Button("Play Now", systemImage: "play.fill") {
                        client.send(.playTrack(key: track.key, singer: who, kind: kind))
                        dismiss()
                    }
                }
            }
            .formStyle(.grouped)
            .navigationTitle("Request")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", role: .cancel) { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add to Queue", action: add)
                        .buttonStyle(.borderedProminent)
                }
            }
        }
        .presentationDetents([.medium, .large])
        #if os(macOS)
        .frame(minWidth: 420, minHeight: 380)
        #endif
    }

    private var who: String? {
        let name = singer.trimmingCharacters(in: .whitespaces)
        return kind == .sing && !name.isEmpty ? name : nil
    }

    private func add() {
        client.send(.enqueue(key: track.key, singer: who, next: false, kind: kind))
        dismiss()
    }
}
