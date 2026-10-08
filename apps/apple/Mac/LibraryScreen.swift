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
        HSplitView {
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
            .frame(minWidth: 170, idealWidth: 220, maxWidth: 320)

            songs(model.librarySelection ?? .all)
                .frame(minWidth: 480)
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
            SongsView(title: playlists.first { $0.id == id }?.displayName ?? "Playlist", query: SongQuery(playlist: id), search: search)
                .id("playlist-" + id)
        case .genre(let g):
            SongsView(title: g, query: SongQuery(genre: g), search: search).id("genre-" + g)
        }
    }
}
