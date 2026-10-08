// Sidebar navigation (HIG: two levels at most, symbols, no critical
// controls at the bottom) with the transport in the toolbar. Every toolbar
// item is also in the Controls menu.

import StemsKit
import StemsUI
import SwiftUI

/// Where the main window is.
enum Destination: Hashable {
    case show, songs, playlist(String), genre(String), machines, guests
}

struct MainWindow: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client
    @State private var playlists: [PlaylistRow] = []
    @State private var genres: [GenreCount] = []

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            List(selection: Binding($model.destination)) {
                Section("Party") {
                    Label("Now Playing", systemImage: "play.circle").tag(Destination.show)
                        .badge(client.state.queue.count)
                }
                Section("Library") {
                    Label("Songs", systemImage: "music.note.list").tag(Destination.songs)
                }
                if !playlists.isEmpty {
                    Section("Playlists") {
                        ForEach(playlists) { p in
                            Label(p.displayName, systemImage: p.kind == "album" ? "square.stack" : "music.note.list")
                                .lineLimit(1)
                                .help("\(p.playable) of \(p.songs) songs ready")
                                .tag(Destination.playlist(p.id))
                        }
                    }
                }
                if !genres.isEmpty {
                    Section("Genres") {
                        ForEach(genres) { g in
                            Label(g.name, systemImage: "guitars").tag(Destination.genre(g.name))
                                .badge(g.songs)
                        }
                    }
                }
                Section("System") {
                    Label("Machines", systemImage: "server.rack").tag(Destination.machines)
                        .badge(problems)
                    Label("Guests", systemImage: "person.2").tag(Destination.guests)
                }
            }
            .navigationSplitViewColumnWidth(min: 190, ideal: 220, max: 300)
            .task(id: client.connection.isConnected) {
                // new imports add playlists and genres
                while !Task.isCancelled {
                    if let p = try? await client.api.playlists() { playlists = p }
                    if let g = try? await client.api.genres() { genres = g }
                    try? await Task.sleep(for: .seconds(60))
                }
            }
        } detail: {
            NavigationStack {
                detail(model.destination)
            }
            .safeAreaInset(edge: .top, spacing: 0) {
                VStack(spacing: 0) {
                    ServerBanner()
                    ConnectionBanner()
                }
            }
            .stemsErrorToast()
        }
        // Space plays/pauses unless a text field has focus (it gets the key first)
        .onKeyPress(.space) {
            guard client.connection.isConnected else { return .ignored }
            client.togglePlayPause()
            return .handled
        }
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Button {
                    client.togglePlayPause()
                } label: {
                    Label(
                        client.state.status == .playing ? "Pause" : "Play",
                        systemImage: client.state.status == .playing ? "pause.fill" : "play.fill")
                }
                .help("Play or pause (Space)")
                .disabled(!client.connection.isConnected)

                Button("Next Song", systemImage: "forward.fill") { client.send(.next) }
                    .help("Skip to the next request (⌘→)")
                    .disabled(!client.connection.isConnected)
            }
            ToolbarItem(placement: .primaryAction) {
                Toggle(
                    isOn: Binding(get: { client.state.karaoke.enabled }, set: { client.send(.setKaraoke($0)) })
                ) {
                    Label("Karaoke", systemImage: "music.mic")
                }
                .help("Vocal removal on or off (⌘K)")
                .disabled(!client.connection.isConnected || client.state.karaoke.backend == nil)
            }
        }
    }

    @ViewBuilder
    private func detail(_ d: Destination) -> some View {
        switch d {
        case .show: ShowView()
        case .songs: SongsView(title: "Songs", query: SongQuery()).id("songs")
        case .playlist(let id):
            SongsView(title: playlists.first { $0.id == id }?.displayName ?? "Playlist", query: SongQuery(playlist: id))
                .id(id)
        case .genre(let g): SongsView(title: g, query: SongQuery(genre: g)).id("genre-" + g)
        case .machines: MachinesView { ThisMacSection() }
        case .guests: GuestsView()
        }
    }

    /// Things in the machines view that need a look.
    private var problems: Int {
        var n = model.supervisor.state.isUp ? 0 : 1
        if let s = client.status {
            if s.library?.available == false { n += 1 }
            if let sep = s.separation, sep.backend == "remote", !sep.workers.contains(where: \.isConnected) { n += 1 }
        }
        return n
    }
}

/// The server process isn't running: say why and offer the fix, without
/// an alert.
struct ServerBanner: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        switch model.supervisor.state {
        case .running, .external, .stopped, .starting:
            EmptyView()
        case .restarting(let at, let attempt):
            bar(symbol: "arrow.clockwise", tint: .orange) {
                Text("The server stopped. Restarting \(at, style: .relative) (attempt \(attempt))…")
            }
        case .failed(let reason):
            bar(symbol: "xmark.octagon.fill", tint: .red) {
                Text(reason)
            }
        }
    }

    private func bar<Content: View>(symbol: String, tint: Color, @ViewBuilder text: () -> Content) -> some View {
        HStack(spacing: 10) {
            Image(systemName: symbol).foregroundStyle(tint)
            text().font(.callout).lineLimit(2)
            Spacer(minLength: 0)
            Button("Show Log") { openWindow(id: "log") }
                .controlSize(.small)
            Button("Restart Server") { model.supervisor.restart() }
                .controlSize(.small)
                .buttonStyle(.borderedProminent)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .background(tint.opacity(0.12))
    }
}

/// Mac-only rows at the top of Machines: the server process and thermals.
struct ThisMacSection: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let sup = model.supervisor
        Section {
            LabeledContent("Server") {
                switch sup.state {
                case .running(let pid): StatusLabelMac(text: "Running (\(sup.mode == .library ? "library" : "Spotify"), pid \(pid))", ok: true)
                case .external: StatusLabelMac(text: "Started outside the app", ok: true)
                case .starting: Text("Starting…")
                case .stopped: StatusLabelMac(text: "Stopped", ok: false)
                case .restarting: StatusLabelMac(text: "Restarting…", ok: false)
                case .failed: StatusLabelMac(text: "Not running", ok: false)
                }
            }
            LabeledContent("Temperature") {
                StatusLabelMac(text: sup.thermal.title, ok: sup.thermal == .nominal || sup.thermal == .fair)
            }
            LabeledContent("Sleep") {
                Text(sup.preventsSleep ? "Prevented while the server runs" : "Allowed")
            }
            HStack {
                Button("Restart Server") { sup.restart() }
                    .disabled(sup.state == .external)
                Button("Show Log") { openWindow(id: "log") }
            }
        } header: {
            Text("This Mac")
        }
    }
}

struct StatusLabelMac: View {
    var text: String
    var ok: Bool

    var body: some View {
        Label {
            Text(text)
        } icon: {
            Image(systemName: ok ? "checkmark.circle.fill" : "exclamationmark.triangle.fill")
                .foregroundStyle(ok ? .green : .orange)
        }
    }
}
