// The Mac hub: runs stemsd, shows the party, and stays in the menu bar.
//
// One main window (closing it keeps the party going; the menu bar extra and
// the Dock icon bring it back), a server log window, and Settings (⌘,).

import AppKit
import StemsKit
import StemsUI
import SwiftUI

@MainActor
@Observable
final class AppModel {
    static let shared = AppModel()

    let supervisor = ServerSupervisor()
    let client: StemsClient
    var destination: Destination = .show

    private init() {
        let port = UserDefaults.standard.integer(forKey: "lastPort")
        client = StemsClient(endpoint: .local(port: port > 0 ? port : 7373, token: nil), app: "mac")
        supervisor.onEndpoint = { [weak self] endpoint in
            UserDefaults.standard.set(endpoint.port, forKey: "lastPort")
            self?.client.switchTo(endpoint)
        }
    }

    func launch() {
        supervisor.start()
        client.start()
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        MainActor.assumeIsolated {
            let model = AppModel.shared
            guard case .running = model.supervisor.state else { return .terminateNow }
            // stopping the server is uncommon and ends the party: confirm
            let alert = NSAlert()
            alert.messageText = "Quit Stems and stop the music?"
            alert.informativeText = "The server stops: playback ends and phones and the lyrics screen disconnect."
            alert.addButton(withTitle: "Quit")
            alert.addButton(withTitle: "Cancel")
            alert.alertStyle = .warning
            guard alert.runModal() == .alertFirstButtonReturn else { return .terminateCancel }
            Task {
                await model.supervisor.stop()
                NSApp.reply(toApplicationShouldTerminate: true)
            }
            return .terminateLater
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

@main
struct StemsMacApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @AppStorage("showMenuBarExtra") private var showMenuBarExtra = true
    private let model = AppModel.shared

    init() {
        AppModel.shared.launch()
    }

    var body: some Scene {
        Window("Stems", id: "main") {
            MainWindow()
                .environment(model)
                .environment(model.client)
                .frame(minWidth: 760, minHeight: 520)
        }
        .defaultSize(width: 1080, height: 720)
        .commands { StemsCommands(model: model) }

        Window("Server Log", id: "log") {
            LogWindow()
                .environment(model)
        }
        .defaultSize(width: 860, height: 520)
        .keyboardShortcut("l", modifiers: [.command, .option])

        Window("Pair iPhone", id: "pair") {
            PairingView()
                .environment(model)
                .environment(model.client)
        }
        .windowResizability(.contentSize)

        Settings {
            MacSettingsView()
                .environment(model)
                .environment(model.client)
        }

        MenuBarExtra(isInserted: $showMenuBarExtra) {
            MenuBarMenu()
                .environment(model)
                .environment(model.client)
        } label: {
            MenuBarIcon()
                .environment(model)
                .environment(model.client)
        }
        .menuBarExtraStyle(.menu)
    }
}
