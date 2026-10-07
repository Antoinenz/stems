// The iOS admin app: pair with the Mac once (scan its QR code), then run
// the party from the phone. Tabs for the main areas, a mini player above
// the tab bar, and a quick reconnect whenever the app comes back.

import StemsKit
import StemsUI
import SwiftUI

@MainActor
@Observable
final class RemoteModel {
    private(set) var client: StemsClient?

    init() {
        #if DEBUG
        // `-pairURL stems://pair?…` (launch argument) for screenshots
        if let link = UserDefaults.standard.string(forKey: "pairURL"), let url = URL(string: link),
            let e = ServerEndpoint(pairingURL: url)
        {
            connect(e, save: false)
            return
        }
        #endif
        if let saved = PairedServer.load() { connect(saved, save: false) }
    }

    func connect(_ endpoint: ServerEndpoint, save: Bool = true) {
        if save { PairedServer.save(endpoint) }
        if let client {
            client.switchTo(endpoint)
        } else {
            let c = StemsClient(endpoint: endpoint, app: "ios")
            c.start()
            client = c
        }
    }

    func forget() {
        PairedServer.clear()
        client?.stop()
        client = nil
    }
}

@main
struct StemsRemoteApp: App {
    @State private var model = RemoteModel()
    @Environment(\.scenePhase) private var phase

    var body: some Scene {
        WindowGroup {
            Group {
                if let client = model.client {
                    RootTabs()
                        .environment(client)
                } else {
                    PairingScreen()
                }
            }
            .environment(model)
            .onOpenURL { url in
                if let e = ServerEndpoint(pairingURL: url) { model.connect(e) }
            }
            .onChange(of: phase) { _, now in
                if now == .active { model.client?.reconnectNow() }
            }
        }
    }
}

struct RootTabs: View {
    @Environment(StemsClient.self) private var client
    @SceneStorage("tab") private var tab = Screen.nowPlaying.rawValue

    var body: some View {
        TabView(selection: $tab) {
            Tab(Screen.nowPlaying.tabTitle, systemImage: Screen.nowPlaying.symbol, value: Screen.nowPlaying.rawValue) {
                screen { NowPlayingView() }
            }
            Tab(Screen.queue.tabTitle, systemImage: Screen.queue.symbol, value: Screen.queue.rawValue) {
                screen { QueueView() }
            }
            .badge(client.state.queue.count)
            Tab(Screen.library.tabTitle, systemImage: Screen.library.symbol, value: Screen.library.rawValue) {
                screen { LibraryView() }
            }
            Tab(Screen.machines.tabTitle, systemImage: Screen.machines.symbol, value: Screen.machines.rawValue) {
                screen { MachinesView() }
            }
            Tab("Settings", systemImage: "gearshape", value: "settings") {
                screen { SettingsTab() }
            }
        }
        .tabBarMinimizeBehavior(.onScrollDown)
        #if DEBUG
        // `-initialTab queue` (launch argument) for screenshots
        .onAppear {
            if let t = UserDefaults.standard.string(forKey: "initialTab") { tab = t }
        }
        #endif
        .tabViewBottomAccessory {
            MiniPlayer()
        }
    }

    private func screen<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        NavigationStack {
            content()
        }
        .safeAreaInset(edge: .top, spacing: 0) { ConnectionBanner() }
        .stemsErrorToast()
    }
}

struct SettingsTab: View {
    @Environment(StemsClient.self) private var client
    @Environment(RemoteModel.self) private var model
    @State private var confirmForget = false
    @State private var rescanning = false

    var body: some View {
        Form {
            Section("Server") {
                LabeledContent("Name", value: client.endpoint.name)
                LabeledContent("Address") {
                    Text(client.endpoint.hosts.map { "\($0):\(client.endpoint.port)" }.joined(separator: "\n"))
                        .multilineTextAlignment(.trailing)
                }
                LabeledContent("Connection") {
                    switch client.connection {
                    case .connected(let host): Text("Connected via \(host)")
                    case .connecting: Text("Connecting…")
                    case .waiting: Text("Not connected")
                    case .idle: Text("Idle")
                    }
                }
                LabeledContent("Role", value: client.isHost ? "Host" : "Guest")
                Button("Scan a New Code", systemImage: "qrcode.viewfinder") { rescanning = true }
            }
            Section {
                NavigationLink {
                    GuestsView()
                } label: {
                    Label("Guests", systemImage: Screen.guests.symbol)
                }
                NavigationLink {
                    PartySettingsView()
                } label: {
                    Label("Party", systemImage: "music.mic")
                }
            }
            .disabled(!client.isHost)
            Section {
                Button("Forget This Server", role: .destructive) { confirmForget = true }
            }
        }
        .navigationTitle("Settings")
        .confirmationDialog("Forget \(client.endpoint.name)?", isPresented: $confirmForget, titleVisibility: .visible) {
            Button("Forget", role: .destructive) { model.forget() }
        } message: {
            Text("You’ll need to scan the Mac’s pairing code again.")
        }
        .sheet(isPresented: $rescanning) {
            PairingScreen(onDone: { rescanning = false })
        }
    }
}
