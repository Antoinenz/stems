// Guest permissions and party settings, both live on the server
// (`/api/settings`).

import StemsKit
import SwiftUI

@MainActor
@Observable
final class SettingsModel {
    var doc: SettingsDocument?
    var info: ServerInfo?
    var error: String?
    var restartNeeded: [String] = []

    func load(_ client: StemsClient) async {
        do {
            doc = try await client.api.settings()
            info = try? await client.api.info()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    func set(_ key: String, _ value: JSONValue, client: StemsClient) {
        doc = doc?.setting(key, to: value)
        Task {
            do {
                let restart = try await client.setSetting(key, value)
                restartNeeded = Array(Set(restartNeeded + restart)).sorted()
            } catch {
                self.error = error.localizedDescription
                await load(client)
            }
        }
    }

    func bool(_ key: String, client: StemsClient) -> Binding<Bool> {
        Binding(get: { self.doc?.bool(key) ?? false }, set: { self.set(key, .bool($0), client: client) })
    }

    func number(_ key: String, client: StemsClient, default d: Double) -> Binding<Double> {
        Binding(get: { self.doc?.number(key) ?? d }, set: { self.set(key, .number($0), client: client) })
    }
}

extension Permission {
    var title: LocalizedStringKey {
        switch self {
        case .queue: "Play, Skip & Queue"
        case .karaoke: "Karaoke On/Off"
        case .vocals: "Guide Vocals"
        case .volume: "Volume"
        case .stage: "Lyrics Screen Style"
        case .timing: "Lyrics & Video Timing"
        }
    }

    var detail: LocalizedStringKey {
        switch self {
        case .queue: "Request songs, reorder the queue, skip."
        case .karaoke: "Switch vocal removal on and off."
        case .vocals: "Change how much of the singer stays in."
        case .volume: "Change the speaker volume."
        case .stage: "Switch between lyrics, karaoke and video views."
        case .timing: "Nudge lyrics and video when they drift."
        }
    }

    var symbol: String {
        switch self {
        case .queue: "list.bullet"
        case .karaoke: "music.mic"
        case .vocals: "person.wave.2"
        case .volume: "speaker.wave.2"
        case .stage: "tv"
        case .timing: "timer"
        }
    }
}

/// What guests on the party wifi may do from their phones.
public struct GuestsView: View {
    @Environment(StemsClient.self) private var client
    @State private var model = SettingsModel()
    @State private var confirmLock = false

    public init() {}

    public var body: some View {
        Form {
            if let doc = model.doc {
                Section {
                    ForEach(Permission.allCases) { p in
                        Toggle(isOn: model.bool("guests.\(p.rawValue)", client: client)) {
                            Label {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text(p.title)
                                    Text(p.detail).font(.caption).foregroundStyle(.secondary)
                                }
                            } icon: {
                                Image(systemName: p.symbol)
                            }
                        }
                    }
                } header: {
                    Text("Guests Can")
                } footer: {
                    Text("Changes apply right away. The host can always do everything.")
                }
                Section {
                    Button("Lock Down", systemImage: "lock.fill", role: .destructive) { confirmLock = true }
                        .disabled(!Permission.allCases.contains { doc.guestAllowed($0) })
                } footer: {
                    Text("Turns off every guest control at once. Guests can still see what’s playing.")
                }
            }
            if let url = model.info?.remoteUrl {
                Section {
                    HStack {
                        Spacer()
                        QRCodeView(url).frame(width: 180, height: 180)
                        Spacer()
                    }
                    LabeledContent("Address") { Text(url).textSelection(.enabled) }
                } header: {
                    Text("Guest Remote")
                } footer: {
                    Text("Guests on the party wifi open this to see the song book and request songs.")
                }
            }
            if let e = model.error {
                Section { StatusLabel(text: e, level: .problem) }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Guests")
        .task(id: client.connection.isConnected) { await model.load(client) }
        .confirmationDialog("Turn off all guest controls?", isPresented: $confirmLock, titleVisibility: .visible) {
            Button("Lock Down", role: .destructive) {
                let off = JSONValue.object(Dictionary(uniqueKeysWithValues: Permission.allCases.map { ($0.rawValue, JSONValue.bool(false)) }))
                model.doc = model.doc.map { SettingsDocument(config: $0.config.merging(.object(["guests": off])), liveKeys: $0.liveKeys) }
                Task {
                    do {
                        try await client.api.patchSettings(.object(["guests": off]))
                    } catch {
                        model.error = error.localizedDescription
                    }
                }
            }
        }
    }
}

/// Party behavior: background music, karaoke for sung requests, levels.
public struct PartySettingsView: View {
    @Environment(StemsClient.self) private var client
    @State private var model = SettingsModel()

    public init() {}

    public var body: some View {
        Form {
            if model.doc != nil {
                Section {
                    Toggle("Karaoke for Sung Requests", isOn: model.bool("library.auto_karaoke", client: client))
                    Toggle("Background Music Between Requests", isOn: model.bool("library.intermission", client: client))
                    LabeledContent("Background Music Level") {
                        Slider(value: model.number("library.intermission_volume", client: client, default: 0.5), in: 0.1...1)
                            .frame(maxWidth: 220)
                    }
                    .disabled(!(model.doc?.bool("library.intermission") ?? false))
                } header: {
                    Text("Requests")
                } footer: {
                    Text("Background music picks random library songs, quieter and with karaoke off, until the next request.")
                }
                Section {
                    LabeledContent("Loudness Target") {
                        Stepper(
                            value: model.number("library.target_lufs", client: client, default: -14), in: -30 ... -5, step: 1
                        ) {
                            Text("\(Int(model.doc?.number("library.target_lufs") ?? -14)) LUFS").monospacedDigit()
                        }
                    }
                } header: {
                    Text("Levels")
                } footer: {
                    Text("Songs are evened out to this loudness so nobody rides the volume between tracks.")
                }
                Section("Lyrics Screen") {
                    Toggle("Music Videos", isOn: model.bool("video.enabled", client: client))
                    Toggle("Sync Videos to the Song", isOn: model.bool("video.align", client: client))
                        .disabled(!(model.doc?.bool("video.enabled") ?? false))
                }
            }
            if !model.restartNeeded.isEmpty {
                Section {
                    StatusLabel(text: "Restart the server to apply: \(model.restartNeeded.joined(separator: ", "))", level: .warning)
                }
            }
            if let e = model.error {
                Section { StatusLabel(text: e, level: .problem) }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Party")
        .task(id: client.connection.isConnected) { await model.load(client) }
    }
}
