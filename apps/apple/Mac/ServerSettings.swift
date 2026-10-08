// Every server setting (stems.toml), grouped and explained, edited in
// place. Changes save together; the ones that only apply after a restart
// say so and offer it.

import StemsKit
import StemsUI
import SwiftUI

/// How one setting is shown.
struct SettingSpec {
    enum Kind {
        case toggle
        case text
        case secret
        case path
        case int(unit: String?)
        case fraction  // 0…1 as a percentage
        case decimal(unit: String?)
        case choice([(String, String)])  // value, title
    }

    let key: String
    let title: String
    var help: String?
    let kind: Kind
}

enum SettingsCatalog {
    static let sections: [(String, [SettingSpec])] = [
        ("General", [
            .init(key: "device_name", title: "Name", help: "What the Spotify app and phones call this Mac.", kind: .text),
            .init(key: "port", title: "Web port", help: "Phones, the lyrics screen and the apps connect here.", kind: .int(unit: nil)),
            .init(key: "sink", title: "Audio output", kind: .choice([("cpal", "This Mac’s output"), ("null", "None (testing)")])),
            .init(key: "lyrics_offset_ms", title: "Lyrics timing", help: "Positive shows lyrics later.", kind: .int(unit: "ms")),
        ]),
        ("Library", [
            .init(key: "library.dir", title: "Folder", help: "Where downloaded songs live. An external drive works.", kind: .path),
            .init(key: "library.auto_karaoke", title: "Karaoke on when a song has stems", kind: .toggle),
            .init(key: "library.intermission", title: "Music between requests", kind: .toggle),
            .init(key: "library.intermission_volume", title: "Between-requests volume", kind: .fraction),
            .init(key: "library.target_lufs", title: "Loudness target", help: "Songs are evened out to this level. −14 is streaming loudness.", kind: .decimal(unit: "LUFS")),
        ]),
        ("Spotify", [
            .init(key: "spotify.enabled", title: "Spotify", help: "Spotify Connect playback and downloads.", kind: .toggle),
            .init(key: "spotify.bitrate", title: "Quality", kind: .choice([("96", "Low (96 kbps)"), ("160", "Normal (160 kbps)"), ("320", "High (320 kbps)")])),
            .init(key: "spotify.device_type", title: "Shows up as", kind: .choice([("tv", "TV"), ("speaker", "Speaker"), ("computer", "Computer"), ("audio_dongle", "Audio dongle")])),
            .init(key: "spotify.initial_volume_percent", title: "Starting volume", kind: .int(unit: "%")),
            .init(key: "spotify.audio_cache_mb", title: "Audio cache", kind: .int(unit: "MB")),
        ]),
        ("Vocal Removal", [
            .init(key: "karaoke.backend", title: "Separate vocals with", kind: .choice([("mdx", "AI on this Mac"), ("remote", "Worker machines"), ("dsp", "Simple filter (no AI)"), ("spool", "Folder spool (cloud farm)")])),
            .init(key: "karaoke.mode", title: "When", kind: .choice([("streaming", "While playing"), ("prebuffer", "Before playing")])),
            .init(key: "karaoke.model", title: "Model", kind: .text),
            .init(key: "karaoke.device", title: "Run on", help: "auto picks the GPU / Neural Engine when there is one.", kind: .text),
            .init(key: "karaoke.workers", title: "Parallel jobs", kind: .int(unit: nil)),
            .init(key: "karaoke.threads", title: "Threads", help: "0 = automatic.", kind: .int(unit: nil)),
            .init(key: "karaoke.gpu_limit", title: "GPU share", help: "Leave headroom for other apps.", kind: .fraction),
            .init(key: "karaoke.fade_ms", title: "Karaoke on/off fade", kind: .int(unit: "ms")),
            .init(key: "karaoke.slack_ms", title: "Head start", kind: .int(unit: "ms")),
            .init(key: "karaoke.lookahead_ms", title: "Look-ahead", help: "0 = automatic.", kind: .int(unit: "ms")),
            .init(key: "karaoke.hop_ms", title: "Hop", help: "0 = automatic.", kind: .int(unit: "ms")),
            .init(key: "karaoke.stem_cache_mb", title: "Stem cache", kind: .int(unit: "MB")),
            .init(key: "karaoke.denoise", title: "Clean up the karaoke track", kind: .toggle),
            .init(key: "karaoke.remote", title: "Workers", help: "Set in Manage Machines.", kind: .text),
            .init(key: "karaoke.local_fallback", title: "If no worker answers", kind: .choice([("never", "Wait for one"), ("when_no_worker", "Use this Mac"), ("always", "Use this Mac as well")])),
            .init(key: "karaoke.spool_dir", title: "Spool folder", kind: .path),
        ]),
        ("Lyrics and Video", [
            .init(key: "lyrics.align", title: "Word-by-word timing", help: "Times each word against the vocals when importing.", kind: .toggle),
            .init(key: "video.enabled", title: "Music videos", kind: .toggle),
            .init(key: "video.align", title: "Line videos up with the song", kind: .toggle),
            .init(key: "video.max_height", title: "Video quality", kind: .choice([("360", "360p"), ("480", "480p"), ("720", "720p"), ("1080", "1080p")])),
            .init(key: "video.ytdlp", title: "yt-dlp command", kind: .text),
        ]),
        ("Guests", [
            .init(key: "guests.queue", title: "Can request songs", kind: .toggle),
            .init(key: "guests.karaoke", title: "Can turn karaoke on and off", kind: .toggle),
            .init(key: "guests.vocals", title: "Can change guide vocals", kind: .toggle),
            .init(key: "guests.volume", title: "Can change the volume", kind: .toggle),
            .init(key: "guests.stage", title: "Can change the lyrics screen", kind: .toggle),
            .init(key: "guests.timing", title: "Can fix lyrics timing", kind: .toggle),
        ]),
        ("Notifications", [
            .init(key: "notify.ntfy", title: "ntfy topic", help: "Your phone is told when someone asks to sing.", kind: .text),
        ]),
        ("Security", [
            .init(key: "host_token", title: "Host key", help: "Lets the apps control everything. Changing it signs out paired phones.", kind: .secret),
        ]),
    ]

    static let known = Set(sections.flatMap { $0.1.map(\.key) })
}

struct ServerSettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client
    @Environment(\.dismiss) private var dismiss
    @State private var doc: SettingsDocument?
    @State private var draft: [String: JSONValue] = [:]
    @State private var error: String?
    @State private var restartKeys: [String] = []
    @State private var saving = false

    var body: some View {
        NavigationStack {
            Form {
                if let doc {
                    ForEach(SettingsCatalog.sections, id: \.0) { section in
                        Section(section.0) {
                            ForEach(section.1, id: \.key) { spec in
                                if doc.config.value(at: spec.key) != nil {
                                    SettingRow(spec: spec, value: binding(spec.key), live: doc.isLive(spec.key))
                                }
                            }
                        }
                    }
                    let others = leaves(doc.config).filter { !SettingsCatalog.known.contains($0.0) }
                    if !others.isEmpty {
                        Section("Other") {
                            ForEach(others, id: \.0) { k, v in
                                SettingRow(spec: .init(key: k, title: k, kind: kind(for: v)), value: binding(k), live: doc.isLive(k))
                            }
                        }
                    }
                } else if let error {
                    Label(error, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                } else {
                    ProgressView()
                }
            }
            .formStyle(.grouped)
            .navigationTitle("Server Settings")
            .safeAreaInset(edge: .bottom) { footer }
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Close") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save", action: save).buttonStyle(.borderedProminent).disabled(draft.isEmpty || saving)
                }
            }
            .task(id: client.connection.isConnected) { if client.connection.isConnected { await load() } }
        }
    }

    @ViewBuilder
    private var footer: some View {
        if !restartKeys.isEmpty || error != nil {
            HStack {
                if let error {
                    Label(error, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                } else {
                    Label("Saved. Some changes apply after a restart.", systemImage: "arrow.clockwise")
                }
                Spacer()
                if !restartKeys.isEmpty {
                    Button("Restart Server") {
                        model.supervisor.restart()
                        restartKeys = []
                    }
                    .buttonStyle(.borderedProminent)
                }
            }
            .padding(12)
            .background(.bar)
        }
    }

    private func binding(_ key: String) -> Binding<JSONValue> {
        Binding(
            get: { draft[key] ?? doc?.config.value(at: key) ?? .null },
            set: { v in
                if v == doc?.config.value(at: key) { draft[key] = nil } else { draft[key] = v }
            })
    }

    private func load() async {
        do {
            doc = try await client.api.settings()
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func save() {
        saving = true
        var patch = JSONValue.object([:])
        for (k, v) in draft { patch = merge(patch, .patch(k, v)) }
        // the app signs in with the host key: pick up the new one
        let newKey = draft["host_token"] != nil
        Task {
            defer { saving = false }
            do {
                let r = try await client.api.patchSettings(patch)
                restartKeys = r.restartRequired
                draft = [:]
                error = nil
                if newKey {
                    model.supervisor.restart()
                    restartKeys = []
                    dismiss()
                    return
                }
                await load()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }

    private func merge(_ a: JSONValue, _ b: JSONValue) -> JSONValue {
        guard case .object(var ao) = a, case .object(let bo) = b else { return b }
        for (k, v) in bo { ao[k] = ao[k].map { merge($0, v) } ?? v }
        return .object(ao)
    }

    private func leaves(_ v: JSONValue, prefix: String = "") -> [(String, JSONValue)] {
        guard case .object(let o) = v else { return [] }
        return o.sorted { $0.key < $1.key }.flatMap { k, v -> [(String, JSONValue)] in
            if case .object = v { return leaves(v, prefix: prefix + k + ".") }
            return [(prefix + k, v)]
        }
    }

    private func kind(for v: JSONValue) -> SettingSpec.Kind {
        switch v {
        case .bool: .toggle
        case .number: .decimal(unit: nil)
        default: .text
        }
    }
}

private struct SettingRow: View {
    let spec: SettingSpec
    @Binding var value: JSONValue
    let live: Bool
    @State private var reveal = false

    var body: some View {
        LabeledContent {
            control
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 4) {
                    Text(spec.title)
                    if !live {
                        Image(systemName: "arrow.clockwise").font(.caption2).foregroundStyle(.tertiary)
                            .help("Applies after a server restart")
                    }
                }
                if let help = spec.help { Text(help).font(.caption).foregroundStyle(.secondary) }
            }
        }
    }

    @ViewBuilder
    private var control: some View {
        switch spec.kind {
        case .toggle:
            Toggle(spec.title, isOn: Binding(get: { value.bool ?? false }, set: { value = .bool($0) }))
                .labelsHidden()
        case .text:
            TextField(spec.title, text: text).labelsHidden().frame(maxWidth: 260)
        case .secret:
            HStack {
                if reveal {
                    TextField(spec.title, text: text).labelsHidden().monospaced()
                } else {
                    SecureField(spec.title, text: text).labelsHidden()
                }
                Button(reveal ? "Hide" : "Show") { reveal.toggle() }
            }
            .frame(maxWidth: 320)
        case .path:
            HStack {
                TextField(spec.title, text: text).labelsHidden().truncationMode(.middle)
                Button("Choose…") {
                    let panel = NSOpenPanel()
                    panel.canChooseDirectories = true
                    panel.canChooseFiles = false
                    panel.canCreateDirectories = true
                    if panel.runModal() == .OK, let url = panel.url { value = .string(url.path) }
                }
            }
            .frame(maxWidth: 360)
        case .int(let unit):
            HStack(spacing: 4) {
                TextField(spec.title, value: Binding(get: { Int(value.number ?? 0) }, set: { value = .number(Double($0)) }), format: .number.grouping(.never))
                    .labelsHidden().multilineTextAlignment(.trailing).frame(width: 90)
                if let unit { Text(unit).foregroundStyle(.secondary) }
            }
        case .decimal(let unit):
            HStack(spacing: 4) {
                TextField(spec.title, value: Binding(get: { value.number ?? 0 }, set: { value = .number($0) }), format: .number)
                    .labelsHidden().multilineTextAlignment(.trailing).frame(width: 90)
                if let unit { Text(unit).foregroundStyle(.secondary) }
            }
        case .fraction:
            HStack {
                Slider(value: Binding(get: { value.number ?? 0 }, set: { value = .number(($0 * 100).rounded() / 100) }), in: 0...1)
                    .frame(width: 160)
                Text("\(Int(((value.number ?? 0) * 100).rounded())) %").monospacedDigit().frame(width: 44, alignment: .trailing)
            }
        case .choice(let options):
            Picker(spec.title, selection: choice) {
                ForEach(options, id: \.0) { v, title in Text(title).tag(v) }
                if !options.contains(where: { $0.0 == choice.wrappedValue }) {
                    Text(choice.wrappedValue).tag(choice.wrappedValue)
                }
            }
            .labelsHidden()
            .fixedSize()
        }
    }

    private var text: Binding<String> {
        Binding(get: { value.string ?? "" }, set: { value = .string($0) })
    }

    /// Choices are strings in the picker; numbers stay numbers in the file.
    private var choice: Binding<String> {
        Binding(
            get: {
                switch value {
                case .number(let n): String(Int(n))
                case .string(let s): s
                default: ""
                }
            },
            set: { s in
                if case .number = value, let n = Double(s) { value = .number(n) } else { value = .string(s) }
            })
    }
}
