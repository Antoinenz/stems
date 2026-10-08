// Library: a browser column (playlists in the host's folders, or genres)
// next to the songs table, with the song page in an inspector.

import StemsKit
import StemsUI
import SwiftUI

/// Library data the screen's parts share.
@MainActor
@Observable
final class LibraryData {
    var playlists: [PlaylistRow] = []
    var genres: [GenreCount] = []
    var artists: [GenreCount] = []
    var folders = PlaylistFolders()
    var error: String?

    /// Playlists of the host's own (the ones that can be edited).
    var local: [PlaylistRow] { playlists.filter { $0.kind == "local" } }

    func load(_ client: StemsClient) async {
        if let p = try? await client.api.playlists() { playlists = p }
        if let g = try? await client.api.genres() { genres = g }
        if let a = try? await client.api.artists() { artists = a }
        if let f = try? await client.api.folders() { folders = f }
    }

    func saveFolders(_ client: StemsClient) {
        let f = folders
        Task {
            do {
                try await client.api.setFolders(f)
            } catch {
                self.error = error.localizedDescription
            }
        }
    }
}

struct LibraryScreen: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client
    @State private var data = LibraryData()
    @AppStorage("libraryBrowse") private var browse = "playlists"
    @State private var showInspector = true
    @State private var artistFilter = ""
    @AppStorage("spotifySearch") private var spotifySearch = true
    @State private var search = ""
    @State private var selectedSong: String?
    @State private var naming: Naming?
    @State private var nameText = ""

    /// What the name prompt is for.
    enum Naming: Identifiable {
        case newPlaylist(songs: [String])
        case newFolder(playlist: String?)
        case rename(PlaylistRow)

        var id: String {
            switch self {
            case .newPlaylist: "playlist"
            case .newFolder: "folder"
            case .rename(let p): "rename-" + p.id
            }
        }
    }

    var body: some View {
        @Bindable var model = model
        HStack(spacing: 0) {
            browser
                .frame(width: 230)
            Divider()
            VStack(spacing: 0) {
                songs(model.librarySelection ?? .all)
                if spotifySearch && search.trimmingCharacters(in: .whitespaces).count >= 2 {
                    Divider()
                    SpotifyPanel(search: search)
                        .frame(height: 260)
                }
            }
            .frame(maxWidth: .infinity)
        }
        .searchable(text: $search, placement: .toolbar, prompt: "Songs, artists, lyrics")
        .inspector(isPresented: $showInspector) {
            SongInspector(key: selectedSong)
                .inspectorColumnWidth(min: 280, ideal: 320, max: 420)
        }
        .task(id: client.connection.isConnected) {
            // new imports add playlists and genres
            while !Task.isCancelled {
                await data.load(client)
                try? await Task.sleep(for: .seconds(60))
            }
        }
        .alert(title(for: naming), isPresented: Binding(get: { naming != nil }, set: { if !$0 { naming = nil } })) {
            TextField("Name", text: $nameText)
            Button("Cancel", role: .cancel) { naming = nil }
            Button("OK") { commitName() }
        }
    }

    // MARK: Browser column

    private var browser: some View {
        @Bindable var model = model
        return VStack(spacing: 0) {
            HStack {
                Picker("Browse", selection: $browse) {
                    Text("Playlists").tag("playlists")
                    Text("Genres").tag("genres")
                    Text("Artists").tag("artists")
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                Menu {
                    Button("New Playlist…") { ask(.newPlaylist(songs: [])) }
                    Button("New Folder…") { ask(.newFolder(playlist: nil)) }
                } label: {
                    Image(systemName: "plus")
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
                .help("New playlist or folder")
            }
            .padding(10)
            if browse == "artists" {
                TextField("Filter artists", text: $artistFilter)
                    .textFieldStyle(.roundedBorder)
                    .padding(.horizontal, 10)
                    .padding(.bottom, 6)
            }
            List(selection: $model.librarySelection) {
                Label("All Songs", systemImage: "music.note.list").tag(LibrarySelection.all)
                Label("Not Ready Yet", systemImage: "hourglass").tag(LibrarySelection.unfinished)
                if browse == "artists" {
                    Section("Artists") {
                        ForEach(data.artists.filter { artistFilter.isEmpty || $0.name.localizedCaseInsensitiveContains(artistFilter) }) { a in
                            Label(a.name, systemImage: "music.microphone")
                                .lineLimit(1)
                                .badge(a.songs)
                                .tag(LibrarySelection.artist(a.name))
                        }
                    }
                } else if browse == "playlists" {
                    ForEach(data.folders.folders) { folder in
                        Section {
                            ForEach(folder.playlists.compactMap { id in data.playlists.first { $0.id == id } }) { p in
                                playlistRow(p)
                            }
                        } header: {
                            Label(folder.name, systemImage: "folder")
                                .contextMenu {
                                    Button("Delete Folder") {
                                        data.folders.folders.removeAll { $0.name == folder.name }
                                        data.saveFolders(client)
                                    }
                                }
                        }
                    }
                    let unfiled = data.playlists.filter { data.folders.folder(of: $0.id) == nil }
                    if !unfiled.isEmpty {
                        Section(data.folders.folders.isEmpty ? "Playlists" : "Other Playlists") {
                            ForEach(unfiled) { p in playlistRow(p) }
                        }
                    }
                } else {
                    Section("Genres") {
                        ForEach(data.genres) { g in
                            Label(g.name, systemImage: "guitars")
                                .badge(g.songs)
                                .tag(LibrarySelection.genre(g.name))
                        }
                    }
                }
            }
            .listStyle(.sidebar)
        }
    }

    private func playlistRow(_ p: PlaylistRow) -> some View {
        Label(p.displayName, systemImage: p.kind == "local" ? "music.note.list" : p.kind == "album" ? "square.stack" : "music.note.list")
            .lineLimit(1)
            .help(p.kind == "local" ? "\(p.songs) songs" : "\(p.playable) of \(p.songs) songs ready")
            .tag(LibrarySelection.playlist(p.id))
            .contextMenu {
                Menu("Move to Folder") {
                    ForEach(data.folders.folders) { f in
                        Button(f.name) {
                            data.folders.move(p.id, to: f.name)
                            data.saveFolders(client)
                        }
                    }
                    if !data.folders.folders.isEmpty { Divider() }
                    Button("New Folder…") { ask(.newFolder(playlist: p.id)) }
                }
                if data.folders.folder(of: p.id) != nil {
                    Button("Remove from Folder") {
                        data.folders.move(p.id, to: nil)
                        data.saveFolders(client)
                    }
                }
                if p.kind == "local" {
                    Divider()
                    Button("Rename…") { ask(.rename(p)) }
                    Button("Delete Playlist", role: .destructive) {
                        Task {
                            try? await client.api.deletePlaylist(p.id)
                            data.folders.move(p.id, to: nil)
                            data.saveFolders(client)
                            await data.load(client)
                        }
                    }
                }
            }
    }

    // MARK: Songs

    @ViewBuilder
    private func songs(_ sel: LibrarySelection) -> some View {
        let actions = PlaylistActions(
            local: data.local,
            add: { id, keys in
                Task {
                    try? await client.api.addToPlaylist(id, songs: keys)
                    await data.load(client)
                }
            },
            newPlaylist: { keys in ask(.newPlaylist(songs: keys)) },
            removeFromCurrent: removeAction(sel))
        switch sel {
        case .all:
            SongsView(title: "Songs", query: SongQuery(), search: search, selected: $selectedSong, playlistActions: actions)
                .id("all")
        case .playlist(let id):
            let p = data.playlists.first { $0.id == id }
            SongsView(
                title: p?.displayName ?? "Playlist", query: SongQuery(playlist: id), search: search,
                emptyHint: p.map { $0.kind == "local" ? "Right-click songs and choose Add to Playlist." : "None of this playlist’s \($0.songs) songs are in this library yet." },
                selected: $selectedSong, playlistActions: actions)
                .id("playlist-" + id)
        case .genre(let g):
            SongsView(title: g, query: SongQuery(genre: g), search: search, selected: $selectedSong, playlistActions: actions)
                .id("genre-" + g)
        case .artist(let a):
            SongsView(title: a, query: SongQuery(artist: a), search: search, selected: $selectedSong, playlistActions: actions)
                .id("artist-" + a)
        case .unfinished:
            SongsView(
                title: "Not Ready Yet", query: SongQuery(unfinished: true), search: search,
                emptyHint: "Every song is ready for karaoke.", selected: $selectedSong, playlistActions: actions)
                .id("unfinished")
        }
    }

    /// Taking songs out works for the host's own playlists only.
    private func removeAction(_ sel: LibrarySelection) -> (([String]) async -> Void)? {
        guard case .playlist(let id) = sel, data.local.contains(where: { $0.id == id }) else { return nil }
        return { keys in
            try? await client.api.removeFromPlaylist(id, songs: keys)
            await data.load(client)
        }
    }

    // MARK: Naming

    private func ask(_ n: Naming) {
        if case .rename(let p) = n { nameText = p.displayName } else { nameText = "" }
        naming = n
    }

    private func title(for n: Naming?) -> LocalizedStringKey {
        switch n {
        case .newPlaylist: "New Playlist"
        case .newFolder: "New Folder"
        case .rename: "Rename Playlist"
        case nil: ""
        }
    }

    private func commitName() {
        let name = nameText.trimmingCharacters(in: .whitespaces)
        defer { naming = nil }
        guard !name.isEmpty, let n = naming else { return }
        switch n {
        case .newPlaylist(let songs):
            Task {
                try? await client.api.createPlaylist(name: name, songs: songs)
                await data.load(client)
            }
        case .newFolder(let playlist):
            if !data.folders.folders.contains(where: { $0.name == name }) {
                data.folders.folders.append(.init(name: name))
            }
            if let playlist { data.folders.move(playlist, to: name) }
            data.saveFolders(client)
        case .rename(let p):
            Task {
                try? await client.api.renamePlaylist(p.id, to: name)
                await data.load(client)
            }
        }
    }
}

/// What the songs table can do with the host's playlists.
struct PlaylistActions {
    var local: [PlaylistRow]
    var add: (String, [String]) -> Void
    var newPlaylist: ([String]) -> Void
    /// When showing one of the host's playlists: take songs out of it.
    var removeFromCurrent: (([String]) async -> Void)?
}
