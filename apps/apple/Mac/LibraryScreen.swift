// Library: a browser column (playlists or genres, as in a mockup from the
// host) next to the songs table for what's selected.

import StemsKit
import StemsUI
import SwiftUI

struct LibraryScreen: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client
    @State private var playlists: [PlaylistRow] = []
    @State private var genres: [GenreCount] = []
    @AppStorage("libraryBrowse") private var browse = "playlists"
    @State private var search = ""

    var body: some View {
        @Bindable var model = model
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                Picker("Browse", selection: $browse) {
                    Text("Playlists").tag("playlists")
                    Text("Genres").tag("genres")
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .padding(10)
                List(selection: $model.librarySelection) {
                    Label("All Songs", systemImage: "music.note.list").tag(LibrarySelection.all)
                    if browse == "playlists" {
                        Section("Playlists") {
                            ForEach(playlists) { p in
                                Label(p.displayName, systemImage: p.kind == "album" ? "square.stack" : "music.note.list")
                                    .lineLimit(1)
                                    .help("\(p.playable) of \(p.songs) songs ready")
                                    .tag(LibrarySelection.playlist(p.id))
                            }
                        }
                    } else {
                        Section("Genres") {
                            ForEach(genres) { g in
                                Label(g.name, systemImage: "guitars")
                                    .badge(g.songs)
                                    .tag(LibrarySelection.genre(g.name))
                            }
                        }
                    }
                }
                .listStyle(.sidebar)
            }
            .frame(width: 230)
            Divider()
            songs(model.librarySelection ?? .all)
                .frame(maxWidth: .infinity)
        }
        .searchable(text: $search, placement: .toolbar, prompt: "Songs, artists, lyrics")
        .task(id: client.connection.isConnected) {
            // new imports add playlists and genres
            while !Task.isCancelled {
                if let p = try? await client.api.playlists() { playlists = p }
                if let g = try? await client.api.genres() { genres = g }
                try? await Task.sleep(for: .seconds(60))
            }
        }
    }

    @ViewBuilder
    private func songs(_ sel: LibrarySelection) -> some View {
        switch sel {
        case .all:
            SongsView(title: "Songs", query: SongQuery(), search: search).id("all")
        case .playlist(let id):
            let p = playlists.first { $0.id == id }
            SongsView(
                title: p?.displayName ?? "Playlist", query: SongQuery(playlist: id), search: search,
                emptyHint: p.map { "None of this playlist’s \($0.songs) songs are in this library yet." })
                .id("playlist-" + id)
        case .genre(let g):
            SongsView(title: g, query: SongQuery(genre: g), search: search).id("genre-" + g)
        }
    }
}
