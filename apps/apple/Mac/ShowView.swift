// The main view for running the night: what's playing on the left, the
// queue on the right, and a search field to add songs without leaving.

import StemsKit
import StemsUI
import SwiftUI

struct ShowView: View {
    var body: some View {
        HSplitView {
            NowPlayingView()
                .frame(minWidth: 380, idealWidth: 520)
            VStack(spacing: 0) {
                AddSongsField()
                    .padding(12)
                Divider()
                QueueView()
            }
            .frame(minWidth: 320, idealWidth: 420)
        }
        .navigationTitle("Now Playing")
    }
}

/// Type to find a song; results drop down; queue it from there.
struct AddSongsField: View {
    @Environment(StemsClient.self) private var client
    @State private var text = ""
    @State private var results: [SongRow] = []
    @State private var showing = false
    @State private var requesting: RequestTarget?
    @FocusState private var focused: Bool

    var body: some View {
        HStack {
            Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
            TextField("Add a song…", text: $text)
                .textFieldStyle(.plain)
                .focused($focused)
                .onSubmit { if let first = results.first { requesting = RequestTarget(first) } }
            if !text.isEmpty {
                Button("Clear", systemImage: "xmark.circle.fill") { text = "" }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.plain)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(8)
        .background(.quaternary.opacity(0.6), in: .rect(cornerRadius: 8))
        .popover(isPresented: $showing, arrowEdge: .bottom) {
            results(list: results)
        }
        .task(id: text) {
            let q = text.trimmingCharacters(in: .whitespaces)
            guard !q.isEmpty else {
                results = []
                showing = false
                return
            }
            try? await Task.sleep(for: .milliseconds(200))
            var query = SongQuery()
            query.q = q
            query.playable = true
            results = (try? await client.api.songs(query, limit: 8).songs) ?? []
            showing = !results.isEmpty && focused
        }
        .sheet(item: $requesting) { RequestSheet(track: $0).frame(minWidth: 420, minHeight: 380) }
    }

    private func results(list: [SongRow]) -> some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(list) { s in
                HStack(spacing: 10) {
                    ArtworkView(url: client.url(for: "/media/library/\(s.key)/cover.jpg"), size: 32, cornerRadius: 4)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(s.title).lineLimit(1)
                        Text(s.artists).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                    Spacer(minLength: 12)
                    Button("Sing…") {
                        showing = false
                        requesting = RequestTarget(s)
                    }
                    .disabled(!s.hasLyrics)
                    .help(s.hasLyrics ? "Request with a singer" : "No lyrics for this one")
                    Button("Add", systemImage: "plus") {
                        client.send(.enqueue(key: s.key, singer: nil, next: false, kind: .play))
                    }
                    .help("Add to the queue (just play)")
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
            }
        }
        .padding(.vertical, 6)
        .frame(width: 460)
    }
}
