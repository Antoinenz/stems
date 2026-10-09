// Up next: reorder by dragging, swipe to remove, play something now.

import StemsKit
import SwiftUI

public struct QueueView: View {
    @Environment(StemsClient.self) private var client
    @State private var confirmClear = false

    public init() {}

    public var body: some View {
        let s = client.state
        List {
            nowSection
            upNextSection
        }
        .overlay { emptyState }
        .navigationTitle("Queue")
        .toolbar { toolbar }
        .confirmationDialog(
            "Remove all \(s.queue.count) requests?", isPresented: $confirmClear, titleVisibility: .visible
        ) {
            Button("Clear Queue", role: .destructive) { client.send(.clearQueue) }
        } message: {
            Text("Singers will have to request their songs again.")
        }
    }

    @ViewBuilder
    private var nowSection: some View {
        let s = client.state
        if s.intermission, let track = s.track {
            Section("Background Music") {
                TrackRow(track: track, artURL: client.url(for: track.art))
            }
        } else if let current = s.current {
            Section("Now Playing") {
                QueueRow(item: current)
            }
        }
    }

    private var upNextSection: some View {
        let canEdit = client.allowed(.queue)
        return Section {
            ForEach(client.state.queue) { item in
                upNextRow(item)
            }
            .onMove { from, to in if canEdit { move(from: from, to: to) } }
            .onDelete { offsets in if canEdit { delete(at: offsets) } }
            .moveDisabled(!canEdit)
            .deleteDisabled(!canEdit)
        } header: {
            if !client.state.queue.isEmpty { Text("Up Next") }
        }
    }

    private func upNextRow(_ item: QueueItem) -> some View {
        QueueRow(item: item)
            .contextMenu { menu(for: item) }
            .swipeActions(edge: .leading) {
                Button("Play Now", systemImage: "play.fill") { playNow(item) }
                    .tint(.accentColor)
            }
    }

    @ViewBuilder
    private var emptyState: some View {
        let s = client.state
        if s.queue.isEmpty {
            let text: String =
                s.isLibrary || s.source == nil
                ? "Add songs from the library. Background music plays while the queue is empty."
                : "The queue is for library playback. This server is playing from \(s.source ?? "another source")."
            ContentUnavailableView {
                Label("No Requests", systemImage: "music.note.list")
            } description: {
                Text(text)
            }
        }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        #if os(iOS)
        if !client.state.queue.isEmpty {
            ToolbarItem(placement: .topBarLeading) { EditButton() }
        }
        #endif
        ToolbarItem(placement: .destructiveAction) {
            Button("Clear Queue", systemImage: "trash") { confirmClear = true }
                .disabled(client.state.queue.isEmpty || !client.allowed(.queue))
        }
    }

    @ViewBuilder
    private func menu(for item: QueueItem) -> some View {
        Button("Play Now", systemImage: "play.fill") { playNow(item) }
        Button("Move to Top", systemImage: "arrow.up.to.line") { client.send(.moveInQueue(id: item.id, to: 0)) }
            .disabled(client.state.queue.first?.id == item.id)
        Divider()
        Button("Remove", systemImage: "minus.circle", role: .destructive) { client.send(.dequeue(id: item.id)) }
    }

    private func playNow(_ item: QueueItem) {
        client.send(.dequeue(id: item.id))
        client.send(.playTrack(key: item.key, singer: item.singer, kind: item.kind))
    }

    private func move(from source: IndexSet, to destination: Int) {
        guard let from = source.first else { return }
        let item = client.state.queue[from]
        // List gives the index before removal; the server wants the final one
        let to = destination > from ? destination - 1 : destination
        client.send(.moveInQueue(id: item.id, to: to))
    }

    private func delete(at offsets: IndexSet) {
        for i in offsets { client.send(.dequeue(id: client.state.queue[i].id)) }
    }
}

/// A queue entry: song, who sings it, sing or just play.
public struct QueueRow: View {
    @Environment(StemsClient.self) private var client
    var item: QueueItem

    public init(item: QueueItem) { self.item = item }

    public var body: some View {
        HStack(spacing: 12) {
            ArtworkView(url: client.url(for: item.track.art), size: 44)
            VStack(alignment: .leading, spacing: 2) {
                Text(item.track.title).font(.body.weight(.medium)).lineLimit(1)
                Text(item.track.artistLine).font(.subheadline).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 8)
            VStack(alignment: .trailing, spacing: 2) {
                Image(systemName: item.kind.symbol)
                    .foregroundStyle(item.kind == .sing ? AnyShapeStyle(.tint) : AnyShapeStyle(.secondary))
                if item.kind == .sing, let singer = item.singer, !singer.isEmpty {
                    Text(singer).font(.caption.weight(.semibold)).lineLimit(1)
                }
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(Self.spoken(item))
    }

    static func spoken(_ item: QueueItem) -> String {
        let song = "\(item.track.title) by \(item.track.artistLine)"
        switch item.kind {
        case .sing: return item.singer.map { "\($0) sings \(song)" } ?? "Karaoke: \(song)"
        case .play: return "Just play: \(song)"
        }
    }
}

struct TrackRow: View {
    var track: TrackRef
    var artURL: URL?

    var body: some View {
        HStack(spacing: 12) {
            ArtworkView(url: artURL, size: 44)
            VStack(alignment: .leading, spacing: 2) {
                Text(track.title).font(.body.weight(.medium)).lineLimit(1)
                Text(track.artistLine).font(.subheadline).foregroundStyle(.secondary).lineLimit(1)
            }
        }
        .accessibilityElement(children: .combine)
    }
}
