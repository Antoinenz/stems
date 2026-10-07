// Health of everything the party depends on: the audio path, the
// separation workers, the library drive, the screens. Hosts only; the
// server pushes a fresh document every second.

import StemsKit
import SwiftUI

public struct MachinesView<Extra: View>: View {
    @Environment(StemsClient.self) private var client
    private let extra: Extra

    /// `extra` goes first: the Mac app adds the server process and thermals.
    public init(@ViewBuilder extra: () -> Extra) {
        self.extra = extra()
    }

    public var body: some View {
        Form {
            extra
            if let s = client.status {
                AudioSection(audio: s.audio, mode: s.mode)
                if let sep = s.separation { SeparationSection(sep: sep) }
                if let lib = s.library { LibrarySection(lib: lib) }
                if let sys = s.system { SystemSection(sys: sys) }
                if let clients = s.clients { ScreensSection(clients: clients) }
            } else if client.me != nil && !client.isHost {
                Section {
                    Text("Only the host can see machine status. Pair this device with the host’s QR code.")
                        .foregroundStyle(.secondary)
                }
            } else {
                Section {
                    HStack {
                        ProgressView().controlSize(.small)
                        Text("Waiting for the server…").foregroundStyle(.secondary)
                    }
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Machines")
    }
}

extension MachinesView where Extra == EmptyView {
    public init() { self.init { EmptyView() } }
}

struct AudioSection: View {
    var audio: AudioStatus?
    var mode: String

    var body: some View {
        Section("Audio") {
            if let a = audio {
                LabeledContent("Playing From", value: a.source.capitalized)
                LabeledContent("Output") {
                    Text("\(a.sink) · \(a.sinkLatencyMs) ms")
                }
                LabeledContent("Buffered") {
                    Text("\(a.bufferedMs) ms").monospacedDigit()
                }
                LabeledContent("Dropouts") {
                    if a.dropoutSeconds == 0 {
                        StatusLabel(text: "None", level: .ok)
                    } else {
                        StatusLabel(
                            text: "\(a.dropoutMs) ms in \(a.dropoutSeconds) \(a.dropoutSeconds == 1 ? "second" : "seconds")",
                            level: .warning)
                    }
                }
                LabeledContent("Up For", value: Format.duration(seconds: a.uptimeS))
            } else {
                LabeledContent("Mode", value: mode.capitalized)
            }
        }
    }
}

struct SeparationSection: View {
    var sep: SeparationStatus

    var body: some View {
        Section {
            LabeledContent("Backend") {
                Text(sep.backend == "remote" ? "Workers" : "\(sep.backend) on \(sep.device)")
            }
            if sep.backend == "remote" {
                LabeledContent("If No Worker") {
                    Text(Self.fallback(sep.localFallback))
                }
                if sep.workers.isEmpty {
                    StatusLabel(text: "No worker has connected yet", level: .warning)
                }
            }
            ForEach(sep.workers) { w in
                WorkerRow(worker: w)
            }
        } header: {
            Text("Vocal Separation")
        } footer: {
            if sep.remote.hasPrefix("listen:") || sep.remote.contains("listen:") {
                Text("Workers connect to this Mac on port \(sep.remote.split(separator: ":").last.map(String.init) ?? "7375").")
            }
        }
    }

    static func fallback(_ s: String) -> String {
        switch s {
        case "never": "Wait for one"
        case "when_no_worker": "Use this Mac"
        case "always": "Use this Mac too"
        default: s
        }
    }
}

struct WorkerRow: View {
    var worker: WorkerStat

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                StatusLabel(
                    text: worker.host,
                    level: !worker.isConnected ? .problem : worker.failures > 0 ? .warning : .ok)
                Spacer()
                Text(worker.isConnected ? "\(worker.connections) connected" : "Gone")
                    .foregroundStyle(.secondary)
            }
            Text(details)
                .font(.caption)
                .foregroundStyle(.secondary)
            if let e = worker.lastError {
                Text(e).font(.caption).foregroundStyle(.orange).lineLimit(2)
            }
        }
        .accessibilityElement(children: .combine)
    }

    private var details: String {
        var parts = [worker.provider, "\(worker.requests) done"]
        if worker.windowMs > 0 { parts.append("\(Int(worker.windowMs)) ms per window") }
        if worker.failures > 0 { parts.append("\(worker.failures) failed") }
        if let idle = worker.idleS, idle > 5 { parts.append("idle \(Format.duration(seconds: Int(idle)))") }
        return parts.joined(separator: " · ")
    }
}

struct LibrarySection: View {
    var lib: LibraryStatus

    var body: some View {
        Section {
            LabeledContent("Drive") {
                StatusLabel(text: lib.available ? "Connected" : "Not Found", level: lib.available ? .ok : .problem)
            }
            LabeledContent("Songs Ready") {
                Text("\(lib.playable) of \(lib.tracks)")
            }
            if lib.complete < lib.playable {
                LabeledContent("Missing Lyrics or Video", value: "\(lib.playable - lib.complete)")
            }
            if lib.totalBytes > 0 {
                let used = Double(lib.totalBytes - lib.freeBytes) / Double(lib.totalBytes)
                Gauge(value: used) {
                    Text("Space")
                } currentValueLabel: {
                    Text("\(Format.bytes(lib.freeBytes)) free")
                }
                .tint(lib.freeBytes < 5_000_000_000 ? .orange : .accentColor)
            }
            if let imp = lib.import {
                if let current = imp.current {
                    LabeledContent("Importing") {
                        Text(imp.waiting > 0 ? "\(current) (+\(imp.waiting))" : current).lineLimit(1)
                    }
                }
                if let e = imp.lastError {
                    StatusLabel(text: e, level: .warning).font(.caption)
                }
            }
        } header: {
            Text("Library")
        } footer: {
            Text(lib.dir).textSelection(.enabled)
        }
    }
}

struct SystemSection: View {
    var sys: SystemStats

    var body: some View {
        Section("Server Load") {
            Gauge(value: min(sys.cpuPercent, 100), in: 0...100) {
                Text("CPU")
            } currentValueLabel: {
                Text("\(Int(sys.cpuPercent)) %")
            }
            .tint(sys.cpuPercent > 85 ? .orange : .accentColor)
            if sys.memTotalMb > 0 {
                let used = Double(sys.memUsedMb) / Double(sys.memTotalMb)
                Gauge(value: used) {
                    Text("Memory")
                } currentValueLabel: {
                    Text("\(Format.percent(used)) of \(sys.memTotalMb / 1024) GB")
                }
                .tint(used > 0.9 ? .orange : .accentColor)
            }
            if let t = sys.hottest {
                LabeledContent("Hottest Sensor") {
                    StatusLabel(
                        text: "\(Int(t.celsius.rounded())) °C",
                        level: t.celsius >= 95 ? .problem : t.celsius >= 85 ? .warning : .ok)
                }
            }
        }
    }
}

struct ScreensSection: View {
    var clients: Clients

    var body: some View {
        Section("Connected") {
            LabeledContent("Lyrics Screen") {
                let n = clients.apps["stage"] ?? 0
                StatusLabel(text: n == 0 ? "None" : n == 1 ? "Connected" : "\(n) connected", level: n == 0 ? .warning : .ok)
            }
            LabeledContent("Phones and Remotes", value: "\(clients.guests + clients.hosts)")
            LabeledContent("Guests", value: "\(clients.guests)")
        }
    }
}
