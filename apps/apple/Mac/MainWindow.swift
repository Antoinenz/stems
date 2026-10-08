// Sidebar navigation (HIG: two levels at most, symbols, no critical
// controls at the bottom) with the transport in the toolbar. Every toolbar
// item is also in the Controls menu.

import StemsKit
import StemsUI
import SwiftUI

/// Where the main window is.
enum Destination: Hashable {
    case show, library, machines, guests
}

/// What the library shows: everything, one playlist, or one genre.
enum LibrarySelection: Hashable {
    case all, playlist(String), genre(String)
}

struct MainWindow: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            List(selection: $model.destination) {
                Section("Party") {
                    // badge before tag: the tag must be on the row itself
                    Label("Now Playing", systemImage: "play.circle")
                        .badge(client.state.queue.count)
                        .tag(Destination.show)
                }
                Section("Library") {
                    Label("Songs", systemImage: "music.note.list")
                        .tag(Destination.library)
                }
                Section("System") {
                    Label("Machines", systemImage: "server.rack")
                        .badge(problems)
                        .tag(Destination.machines)
                    Label("Guests", systemImage: "person.2")
                        .tag(Destination.guests)
                }
            }
            .navigationSplitViewColumnWidth(min: 170, ideal: 190, max: 260)
        } detail: {
            NavigationStack {
                detail(model.destination ?? .show)
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
        case .library: LibraryScreen()
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
