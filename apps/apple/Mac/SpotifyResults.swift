// Songs found on Spotify that the library doesn't have (or isn't done
// with), offered for download. Shown under the library's own results.
// Turned off in Settings › General when the internet is slow.

import StemsKit
import StemsUI
import SwiftUI

@MainActor
@Observable
final class SpotifySearchModel {
    private(set) var hits: [SpotifyHit] = []
    private(set) var searching = false
    private(set) var error: String?
    /// Downloads asked for here, until the server reports them.
    private(set) var asked: Set<String> = []

    func search(_ q: String, client: StemsClient, limit: Int) async {
        let q = q.trimmingCharacters(in: .whitespaces)
        guard q.count >= 2 else {
            hits = []
            error = nil
            return
        }
        searching = true
        defer { searching = false }
        do {
            let found = try await client.api.spotifySearch(q, limit: limit)
            guard !Task.isCancelled else { return }
            hits = found
            error = nil
        } catch is CancellationError {
        } catch {
            if !Task.isCancelled { self.error = error.localizedDescription }
        }
    }

    func status(_ h: SpotifyHit) -> SpotifyHit.Status {
        asked.contains(h.id) && h.status != .ready ? .queued : h.status
    }

    func download(_ h: SpotifyHit, client: StemsClient) {
        asked.insert(h.id)
        Task {
            do {
                try await client.api.importSources([h.url])
            } catch {
                asked.remove(h.id)
                self.error = error.localizedDescription
            }
        }
    }
}

/// One Spotify result: art, title, and Download / Downloading / In Library.
struct SpotifyHitRow: View {
    @Environment(StemsClient.self) private var client
    let hit: SpotifyHit
    let model: SpotifySearchModel

    var body: some View {
        HStack(spacing: 10) {
            ArtworkView(url: hit.art.flatMap(URL.init(string:)), size: 32, cornerRadius: 4)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(hit.title).lineLimit(1)
                    if hit.explicit {
                        Image(systemName: "e.square.fill").foregroundStyle(.secondary).help("Explicit")
                    }
                }
                Text([hit.artistLine, hit.year].compactMap { $0 }.joined(separator: " · "))
                    .font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer(minLength: 12)
            switch model.status(hit) {
            case .ready:
                Label("In Library", systemImage: "checkmark.circle.fill")
                    .font(.caption).foregroundStyle(.secondary)
            case .queued:
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Downloading").font(.caption).foregroundStyle(.secondary)
                }
                .help("Being downloaded and prepared (stems, lyrics, video). This takes a few minutes per song.")
            case .incomplete, .new:
                Button("Download", systemImage: "arrow.down.circle") { model.download(hit, client: client) }
                    .help("Download and prepare this song for karaoke")
            }
        }
        .accessibilityElement(children: .combine)
    }
}

/// The "On Spotify" panel under the library table.
struct SpotifyPanel: View {
    @Environment(StemsClient.self) private var client
    @State private var model = SpotifySearchModel()
    let search: String

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Label("On Spotify", systemImage: "globe").font(.headline)
                if model.searching { ProgressView().controlSize(.small) }
                Spacer()
                Text("Songs you don’t have yet").font(.caption).foregroundStyle(.secondary)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            Divider()
            if let e = model.error {
                Label(e, systemImage: "exclamationmark.triangle").foregroundStyle(.secondary).padding(12)
            } else if model.hits.isEmpty && !model.searching {
                Text("No matches on Spotify.").foregroundStyle(.secondary).padding(12)
            }
            List(model.hits) { h in
                SpotifyHitRow(hit: h, model: model)
            }
            .listStyle(.plain)
        }
        .task(id: search) {
            try? await Task.sleep(for: .milliseconds(500))  // Spotify is slower than the index
            guard !Task.isCancelled else { return }
            await model.search(search, client: client, limit: 15)
        }
    }
}
