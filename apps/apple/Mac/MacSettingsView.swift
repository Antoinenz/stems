// Settings (⌘,): a noncustomizable toolbar of panes (HIG). General is the
// Mac app itself; Party is the server's live settings.

import AppKit
import StemsKit
import StemsUI
import SwiftUI

struct MacSettingsView: View {
    @AppStorage("settingsPane") private var pane = "general"

    var body: some View {
        TabView(selection: $pane) {
            Tab("General", systemImage: "gearshape", value: "general") {
                GeneralSettings()
            }
            Tab("Party", systemImage: "music.mic", value: "party") {
                PartySettingsView()
            }
        }
        .frame(width: 520)
        .frame(minHeight: 420)
    }
}

struct GeneralSettings: View {
    @Environment(AppModel.self) private var model
    @AppStorage("serverMode") private var mode = ServerMode.library.rawValue
    @AppStorage("showMenuBarExtra") private var showMenuBarExtra = true
    @AppStorage("stemsdPath") private var stemsdPath = ""
    @AppStorage("stemsdConfig") private var configPath = ""
    @AppStorage("previewDeviceUID") private var previewDevice = ""
    @AppStorage("spotifySearch") private var spotifySearch = true
    @State private var ntfy = ""
    @State private var ntfySaved = ""

    var body: some View {
        Form {
            Section {
                Picker("Play From", selection: $mode) {
                    ForEach(ServerMode.allCases) { m in Text(m.title).tag(m.rawValue) }
                }
                .onChange(of: mode) { model.supervisor.restart() }
            } footer: {
                Text("Changing this restarts the server. Library plays imported songs and works without internet.")
            }
            Section {
                Picker("Preview Songs On", selection: $previewDevice) {
                    Text("Built-in Output").tag("")
                    ForEach(OutputDevice.all()) { d in Text(d.name).tag(d.uid) }
                }
            } footer: {
                Text("Song previews play here, never on the party speakers. Choose headphones to listen without anyone hearing.")
            }
            Section {
                Toggle("Search Spotify Too", isOn: $spotifySearch)
            } footer: {
                Text("Searches also show songs on Spotify you can download. Turn off when the internet is slow or offline.")
            }
            Section {
                TextField("Phone Notifications Topic", text: $ntfy, prompt: Text("e.g. stems-antoine-8h2k"))
                    .onSubmit(saveNtfy)
                if ntfy != ntfySaved {
                    Button("Save and Restart Server", action: saveNtfy)
                }
            } footer: {
                Text("Install the free ntfy app on your phone and subscribe to this topic to be told when someone asks to sing. Pick something hard to guess.")
            }
            Section {
                Toggle("Show in Menu Bar", isOn: $showMenuBarExtra)
            } footer: {
                Text("Play, skip and karaoke from the menu bar, even when the window is closed.")
            }
            Section("Server") {
                LabeledContent("stemsd") {
                    Text(model.supervisor.binary?.path ?? "Not found")
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .textSelection(.enabled)
                }
                HStack {
                    Button("Choose…") { choose() }
                    if !stemsdPath.isEmpty {
                        Button("Use Built-In") {
                            stemsdPath = ""
                            model.supervisor.restart()
                        }
                    }
                }
                if let cfg = model.supervisor.config {
                    LabeledContent("Port", value: String(cfg.port))
                    LabeledContent("Config File") {
                        Button(URL(fileURLWithPath: cfg.configFile).lastPathComponent) {
                            NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: cfg.configFile)])
                        }
                        .buttonStyle(.link)
                        .help(cfg.configFile)
                    }
                    HStack {
                        Button("Use Another Config File…") { chooseConfig() }
                        if !configPath.isEmpty {
                            Button("Use Default") {
                                configPath = ""
                                model.supervisor.restart()
                            }
                        }
                    }
                    LabeledContent("Library") {
                        Text(cfg.libraryDir).lineLimit(1).truncationMode(.middle).textSelection(.enabled)
                    }
                }
            }
        }
        .formStyle(.grouped)
        .task(id: model.client.connection.isConnected) {
            ntfy = (try? await model.client.api.settings())?.string("notify.ntfy") ?? ""
            ntfySaved = ntfy
        }
    }

    private func saveNtfy() {
        let topic = ntfy.trimmingCharacters(in: .whitespaces)
        Task {
            _ = try? await model.client.setSetting("notify.ntfy", .string(topic))
            ntfySaved = topic
            ntfy = topic
            model.supervisor.restart()
        }
    }

    private func chooseConfig() {
        let panel = NSOpenPanel()
        panel.message = "Choose a stems.toml (for example one kept on the library drive)"
        panel.allowedContentTypes = [.init(filenameExtension: "toml") ?? .plainText]
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }
        configPath = url.path
        model.supervisor.restart()
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.message = "Choose a stemsd binary"
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }
        stemsdPath = url.path
        model.supervisor.restart()
    }
}
