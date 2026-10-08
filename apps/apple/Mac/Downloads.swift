// The download manager: what the importer is doing (downloading from
// Spotify, separating, lyrics, video), what's waiting, what finished or
// failed, and a box for pasting Spotify links.

import StemsKit
import StemsUI
import SwiftUI

@MainActor
@Observable
final class DownloadsModel {
    private(set) var status = ImportStatus()
    var error: String?

    var active: Int { status.waiting.count + (status.current == nil ? 0 : 1) }

    func refresh(_ client: StemsClient) async {
        if let s = try? await client.api.importStatus() { status = s }
    }

    func add(_ text: String, client: StemsClient) async -> Bool {
        let links = text.split(whereSeparator: \.isWhitespace).map(String.init).filter { !$0.isEmpty }
        guard !links.isEmpty else { return false }
        do {
            try await client.api.importSources(links)
            error = nil
            await refresh(client)
            return true
        } catch {
            self.error = error.localizedDescription
            return false
        }
    }
}

struct DownloadsButton: View {
    @Environment(StemsClient.self) private var client
    @State private var model = DownloadsModel()
    @State private var open = false

    var body: some View {
        Button {
            open.toggle()
        } label: {
            Label("Downloads", systemImage: model.active > 0 ? "arrow.down.circle.dotted" : "arrow.down.circle")
        }
        .badge(model.active)
        .help(model.active > 0 ? "\(model.active) downloading or waiting" : "Downloads")
        .popover(isPresented: $open, arrowEdge: .bottom) {
            DownloadsPanel(model: model).frame(width: 420, height: 480)
        }
        .task(id: client.connection.isConnected) {
            while !Task.isCancelled {
                await model.refresh(client)
                try? await Task.sleep(for: .seconds(open || model.active > 0 ? 2 : 15))
            }
        }
    }
}

struct DownloadsPanel: View {
    @Environment(StemsClient.self) private var client
    let model: DownloadsModel
    @State private var link = ""

    var body: some View {
        let s = model.status
        VStack(spacing: 0) {
            HStack {
                TextField("Paste Spotify links (songs, albums, playlists)", text: $link)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(add)
                Button("Download", action: add).disabled(link.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(12)
            if let e = model.error {
                Label(e, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange).font(.caption)
                    .padding(.horizontal, 12).padding(.bottom, 8)
            }
            Divider()
            List {
                if let cur = s.current {
                    Section("Now") {
                        HStack {
                            ProgressView().controlSize(.small)
                            Text(cur).lineLimit(1)
                        }
                    }
                }
                if !s.waiting.isEmpty {
                    Section("Waiting (\(s.waiting.count))") {
                        ForEach(Array(s.waiting.prefix(100).enumerated()), id: \.offset) { _, t in
                            Label(t, systemImage: "clock").lineLimit(1).foregroundStyle(.secondary)
                        }
                        if s.waiting.count > 100 {
                            Text("and \(s.waiting.count - 100) more").foregroundStyle(.secondary)
                        }
                    }
                }
                if !s.errors.isEmpty {
                    Section("Problems") {
                        ForEach(Array(s.errors.enumerated()), id: \.offset) { _, e in
                            Label(e, systemImage: "xmark.octagon").foregroundStyle(.red).lineLimit(3)
                        }
                    }
                }
                if !s.finished.isEmpty {
                    Section("Finished") {
                        ForEach(s.finished) { f in FinishedRow(item: f) }
                    }
                }
                if s.current == nil && s.waiting.isEmpty && s.finished.isEmpty && s.errors.isEmpty {
                    ContentUnavailableView(
                        "No Downloads", systemImage: "arrow.down.circle",
                        description: Text("Search for a song and choose Download, or paste a Spotify link above."))
                }
            }
            .listStyle(.inset)
        }
    }

    private func add() {
        let text = link
        Task { if await model.add(text, client: client) { link = "" } }
    }
}

private struct FinishedRow: View {
    @Environment(StemsClient.self) private var client
    let item: ImportStatus.Finished

    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            Image(systemName: item.playable ? (item.problems.isEmpty ? "checkmark.circle.fill" : "checkmark.circle") : "exclamationmark.triangle.fill")
                .foregroundStyle(item.playable ? (item.problems.isEmpty ? .green : .secondary) : .orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(item.title).lineLimit(1)
                if !item.problems.isEmpty {
                    Text(item.problems.joined(separator: "\n")).font(.caption).foregroundStyle(.secondary).lineLimit(3)
                }
            }
            Spacer()
            if !item.problems.isEmpty, item.key.hasPrefix("spotify_") {
                Button("Retry") {
                    Task { try? await client.api.importSources(["https://open.spotify.com/track/\(item.key.dropFirst("spotify_".count))"]) }
                }
                .controlSize(.small)
                .help("Redo only the steps that failed")
            }
        }
    }
}
