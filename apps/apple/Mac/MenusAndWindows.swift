// Menu bar commands, the menu bar extra, the server log and iPhone pairing.

import AppKit
import StemsKit
import StemsUI
import SwiftUI

// MARK: Menus

struct StemsCommands: Commands {
    let model: AppModel
    static let places: [(LocalizedStringKey, Destination)] = [
        ("Now Playing", .show), ("Songs", .library), ("Machines", .machines), ("Guests", .guests),
    ]
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(after: .appSettings) {
            Button("Pair iPhone…") { openWindow(id: "pair") }
        }
        // the app has no documents
        CommandGroup(replacing: .newItem) {}

        CommandMenu("Controls") {
            let client = model.client
            // Space is handled by the main window (onKeyPress) rather than
            // here: a bare-Space menu shortcut would eat spaces typed in
            // the search field
            Button(client.state.status == .playing ? "Pause" : "Play") { client.togglePlayPause() }
            Button("Next Song") { client.send(.next) }
                .keyboardShortcut(.rightArrow, modifiers: .command)
            Divider()
            Toggle("Karaoke", isOn: Binding(get: { client.state.karaoke.enabled }, set: { client.send(.setKaraoke($0)) }))
                .keyboardShortcut("k", modifiers: .command)
            Button("Volume Up") { client.sendThrottled(.setVolume(min(1, client.state.volume + 0.05)), key: "volume") }
                .keyboardShortcut(.upArrow, modifiers: .command)
            Button("Volume Down") { client.sendThrottled(.setVolume(max(0, client.state.volume - 0.05)), key: "volume") }
                .keyboardShortcut(.downArrow, modifiers: .command)
            Divider()
            Button("Restart Server") { model.supervisor.restart() }
        }

        CommandGroup(before: .sidebar) {
            ForEach(Array(Self.places.enumerated()), id: \.offset) { i, place in
                Button(place.0) {
                    model.destination = place.1
                    openWindow(id: "main")
                }
                .keyboardShortcut(KeyEquivalent(Character(String(i + 1))), modifiers: .command)
            }
            Divider()
        }

        CommandGroup(before: .windowList) {
            Button("Server Log") { openWindow(id: "log") }
            Divider()
        }
    }
}

/// The menu bar extra: a menu (not a popover), per the HIG.
struct MenuBarMenu: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        let s = client.state
        if let track = s.track {
            Text(track.title)
            Text(s.turn?.text ?? track.artistLine)
        } else {
            Text(client.connection.isConnected ? "Nothing playing" : "Not connected")
        }
        Divider()
        Button(s.status == .playing ? "Pause" : "Play") { client.togglePlayPause() }
            .disabled(!client.connection.isConnected)
        Button("Next Song") { client.send(.next) }
            .disabled(!client.connection.isConnected)
        Toggle("Karaoke", isOn: Binding(get: { s.karaoke.enabled }, set: { client.send(.setKaraoke($0)) }))
            .disabled(!client.connection.isConnected)
        if let next = s.queue.first {
            Divider()
            Text("Up next: \(next.track.title)" + (next.singer.map { " (\($0))" } ?? ""))
        }
        Divider()
        Button("Open Stems") {
            openWindow(id: "main")
            NSApp.activate()
        }
        Button("Server Log") {
            openWindow(id: "log")
            NSApp.activate()
        }
        Button("Restart Server") { model.supervisor.restart() }
        Divider()
        Button("Quit Stems") { NSApp.terminate(nil) }
    }
}

struct MenuBarIcon: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client

    var body: some View {
        if !model.supervisor.state.isUp && model.supervisor.state != .starting {
            Image(systemName: "exclamationmark.triangle")
                .accessibilityLabel("Stems: server not running")
        } else {
            Image(systemName: client.state.status == .playing ? "music.mic.circle.fill" : "music.mic.circle")
                .accessibilityLabel("Stems")
        }
    }
}

// MARK: Server log

struct LogWindow: View {
    @Environment(AppModel.self) private var model
    @State private var filter = ""
    @State private var problemsOnly = false
    @State private var follow = true

    var body: some View {
        let lines = model.supervisor.log.filter {
            (!problemsOnly || $0.isProblem) && (filter.isEmpty || $0.text.localizedCaseInsensitiveContains(filter))
        }
        ScrollViewReader { proxy in
            List(lines) { line in
                Text(line.text)
                    .font(.system(.callout, design: .monospaced))
                    .foregroundStyle(line.isProblem ? AnyShapeStyle(.orange) : AnyShapeStyle(.primary))
                    .textSelection(.enabled)
                    .listRowSeparator(.hidden)
                    .id(line.id)
            }
            .listStyle(.plain)
            .onChange(of: model.supervisor.log.last?.id) { _, last in
                if follow, let last { proxy.scrollTo(last, anchor: .bottom) }
            }
        }
        .overlay {
            if lines.isEmpty {
                ContentUnavailableView(filter.isEmpty ? "No Output Yet" : "No Matches", systemImage: "text.alignleft")
            }
        }
        .searchable(text: $filter, prompt: "Filter")
        .navigationTitle("Server Log")
        .toolbar {
            Toggle(isOn: $problemsOnly) { Label("Warnings Only", systemImage: "exclamationmark.triangle") }
                .help("Show only warnings and errors")
            Toggle(isOn: $follow) { Label("Follow", systemImage: "arrow.down.to.line") }
                .help("Keep the newest line in view")
            Button("Copy All", systemImage: "doc.on.doc") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(lines.map(\.text).joined(separator: "\n"), forType: .string)
            }
            Button("Clear", systemImage: "trash") { model.supervisor.clearLog() }
        }
    }
}

// MARK: Pairing

/// A QR code the iPhone app scans to connect as the host. It carries the
/// host token, so only show it to people who should control the party.
struct PairingView: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client
    @State private var info: ServerInfo?
    @State private var reveal = false

    var body: some View {
        VStack(spacing: 18) {
            if let url = endpoint?.pairingURL {
                QRCodeView(url.absoluteString)
                    .frame(width: 260, height: 260)
                    .blur(radius: reveal ? 0 : 14)
                    .overlay {
                        if !reveal {
                            Button("Show Code") { reveal = true }
                                .buttonStyle(.borderedProminent)
                                .controlSize(.large)
                        }
                    }
                    .accessibilityLabel("Pairing code")
                Text("Open Stems on your iPhone and choose Scan Code, or point the Camera app at it.")
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 300)
                Label("Anyone with this code controls the party.", systemImage: "lock.shield")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                if let e = endpoint {
                    Text(e.hosts.map { "\($0):\(e.port)" }.joined(separator: "  ·  "))
                        .font(.caption.monospaced())
                        .foregroundStyle(.tertiary)
                        .textSelection(.enabled)
                }
            } else {
                ProgressView()
                Text("Waiting for the server…").foregroundStyle(.secondary)
            }
        }
        .padding(28)
        .frame(width: 380)
        .task(id: client.connection.isConnected) { info = try? await client.api.info() }
    }

    /// The LAN IP first (phones on a no-internet wifi may skip mDNS), then
    /// the `.local` name in case the router hands out a new IP.
    private var endpoint: ServerEndpoint? {
        guard let cfg = model.supervisor.config else { return nil }
        var hosts: [String] = []
        if let lan = info?.lanHost, let host = lan.split(separator: ":").first { hosts.append(String(host)) }
        let local = ProcessInfo.processInfo.hostName
        if local.hasSuffix(".local") { hosts.append(local) } else if !local.isEmpty { hosts.append(local + ".local") }
        guard !hosts.isEmpty else { return nil }
        return ServerEndpoint(name: Host.current().localizedName ?? "stems", hosts: hosts, port: cfg.port, token: cfg.hostToken)
    }
}
