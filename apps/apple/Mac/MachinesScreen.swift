// Machines: one card per machine (this Mac, each separation worker, the
// lyrics screens, phones), and a panel to manage how workers connect.

import StemsKit
import StemsUI
import SwiftUI

struct MachinesScreen: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client
    @State private var managing = false
    @State private var info: ServerInfo?

    var body: some View {
        ScrollView {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 320), spacing: 16, alignment: .top)], spacing: 16) {
                ThisMacCard()
                if let sep = client.status?.separation {
                    if sep.workers.isEmpty && sep.backend == "remote" {
                        Card(title: "Separation Worker", symbol: "cpu", status: ("Not connected", .warning)) {
                            Text("Vocal separation is set to run on other machines, but none has connected. Use Manage Machines to add one.")
                                .font(.callout).foregroundStyle(.secondary)
                        }
                    }
                    ForEach(sep.workers) { w in WorkerCard(worker: w) }
                }
                ScreensCard(info: info)
                PhonesCard()
            }
            .padding(20)
        }
        .navigationTitle("Machines")
        .toolbar {
            ToolbarItem {
                Button("Manage Machines…", systemImage: "slider.horizontal.3") { managing = true }
                    .help("Workers, how they connect, adding machines")
            }
        }
        .sheet(isPresented: $managing) {
            ManageMachines(info: info).frame(minWidth: 560, minHeight: 560)
        }
        .task(id: client.connection.isConnected) { info = try? await client.api.info() }
    }
}

// MARK: Cards

enum Health { case ok, warning, problem, off }

struct Card<Content: View>: View {
    let title: String
    let symbol: String
    var subtitle: String?
    let status: (String, Health)
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline) {
                Label {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(title).font(.headline)
                        if let subtitle { Text(subtitle).font(.caption).foregroundStyle(.secondary) }
                    }
                } icon: {
                    Image(systemName: symbol).foregroundStyle(.tint)
                }
                Spacer()
                HealthBadge(text: status.0, health: status.1)
            }
            content
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.background.secondary, in: .rect(cornerRadius: 14))
    }
}

struct HealthBadge: View {
    let text: String
    let health: Health

    var body: some View {
        HStack(spacing: 5) {
            Image(systemName: symbol).foregroundStyle(color)
            Text(text)
        }
        .font(.caption.weight(.semibold))
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .background(color.opacity(0.14), in: .capsule)
        .accessibilityElement(children: .combine)
    }

    private var symbol: String {
        switch health {
        case .ok: "checkmark.circle.fill"
        case .warning: "exclamationmark.triangle.fill"
        case .problem: "xmark.octagon.fill"
        case .off: "minus.circle"
        }
    }

    private var color: Color {
        switch health {
        case .ok: .green
        case .warning: .orange
        case .problem: .red
        case .off: .secondary
        }
    }
}

struct Row: View {
    let label: String
    let value: String
    var body: some View {
        LabeledContent(label) { Text(value).monospacedDigit().textSelection(.enabled) }
            .font(.callout)
    }
}

struct Meter: View {
    let label: String
    let value: Double
    let text: String
    var warnAbove = 0.85

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack {
                Text(label)
                Spacer()
                Text(text).foregroundStyle(.secondary).monospacedDigit()
            }
            .font(.callout)
            Gauge(value: min(max(value, 0), 1)) { EmptyView() }
                .gaugeStyle(.accessoryLinearCapacity)
                .tint(value > warnAbove ? .orange : .accentColor)
        }
    }
}

struct ThisMacCard: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client

    var body: some View {
        let sup = model.supervisor
        let s = client.status
        Card(title: Host.current().localizedName ?? "This Mac", symbol: "laptopcomputer", subtitle: "Server · audio · library", status: health) {
            if let sys = s?.system {
                Meter(label: "CPU", value: sys.cpuPercent / 100, text: "\(Int(sys.cpuPercent)) %")
                if sys.memTotalMb > 0 {
                    let used = Double(sys.memUsedMb) / Double(sys.memTotalMb)
                    Meter(label: "Memory", value: used, text: "\(Int((used * 100).rounded())) % of \(sys.memTotalMb / 1024) GB", warnAbove: 0.9)
                }
                if let t = sys.hottest { Row(label: "Hottest sensor", value: "\(Int(t.celsius.rounded())) °C") }
            }
            Row(label: "Temperature", value: sup.thermal.title)
            Divider()
            if let a = s?.audio {
                Row(label: "Playing from", value: a.source.capitalized)
                Row(label: "Output", value: "\(a.sink) · \(a.sinkLatencyMs) ms")
                Row(label: "Dropouts", value: a.dropoutSeconds == 0 ? "None" : "\(a.dropoutMs) ms in \(a.dropoutSeconds) s")
                Row(label: "Up for", value: Duration.seconds(a.uptimeS).formatted(.units(allowed: [.hours, .minutes], width: .abbreviated)))
            }
            if let lib = s?.library {
                Divider()
                Row(label: "Library drive", value: lib.available ? "Connected" : "Not found")
                Row(label: "Songs ready", value: "\(lib.playable) of \(lib.tracks)")
                if lib.totalBytes > 0 {
                    Meter(
                        label: "Disk", value: Double(lib.totalBytes - lib.freeBytes) / Double(lib.totalBytes),
                        text: ByteCountFormatter.string(fromByteCount: lib.freeBytes, countStyle: .file) + " free", warnAbove: 0.95)
                }
                if let cur = lib.import?.current { Row(label: "Importing", value: cur) }
            }
            HStack {
                Button("Restart Server") { sup.restart() }.disabled(sup.state == .external)
                Spacer()
                Text(serverText(sup.state)).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private var health: (String, Health) {
        let sup = model.supervisor
        guard sup.state.isUp else { return ("Server stopped", .problem) }
        if client.status?.library?.available == false { return ("Library drive missing", .problem) }
        if (client.status?.audio?.dropoutSeconds ?? 0) > 0 { return ("Dropouts", .warning) }
        if sup.thermal == .serious || sup.thermal == .critical { return ("Hot", .warning) }
        return ("OK", .ok)
    }

    private func serverText(_ s: ServerSupervisor.State) -> String {
        switch s {
        case .running(let pid): "stemsd pid \(pid)"
        case .external: "started outside the app"
        case .starting: "starting…"
        case .stopped: "stopped"
        case .restarting: "restarting…"
        case .failed: "not running"
        }
    }
}

struct WorkerCard: View {
    let worker: WorkerStat

    var body: some View {
        Card(
            title: worker.host, symbol: "cpu", subtitle: "Vocal separation · \(worker.provider)",
            status: worker.isConnected
                ? (worker.failures > 0 ? ("\(worker.failures) failed", .warning) : ("Connected", .ok))
                : ("Gone", .problem)
        ) {
            Row(label: "Connections", value: String(worker.connections))
            Row(label: "Done", value: "\(worker.requests)")
            if worker.windowMs > 0 { Row(label: "Speed", value: "\(Int(worker.windowMs)) ms per window") }
            Row(label: "Model", value: worker.model)
            if worker.align { Row(label: "Word timing", value: "Yes") }
            if let idle = worker.idleS { Row(label: "Last answered", value: "\(Int(idle)) s ago") }
            if let e = worker.lastError {
                Text(e).font(.caption).foregroundStyle(.orange).lineLimit(3).textSelection(.enabled)
            }
        }
    }
}

struct ScreensCard: View {
    @Environment(StemsClient.self) private var client
    let info: ServerInfo?

    var body: some View {
        let n = client.status?.clients?.apps["stage"] ?? 0
        Card(title: "Lyrics Screen", symbol: "tv", subtitle: "The stage view", status: n == 0 ? ("None connected", .warning) : (n == 1 ? "Connected" : "\(n) connected", .ok)) {
            if let url = info?.stageUrl {
                Row(label: "Address", value: url)
            }
            Text("Open the address on the screen’s computer (the stage app, or a browser in full screen).")
                .font(.caption).foregroundStyle(.secondary)
        }
    }
}

struct PhonesCard: View {
    @Environment(StemsClient.self) private var client

    var body: some View {
        let c = client.status?.clients
        Card(title: "Phones and Remotes", symbol: "iphone", status: ("\((c?.hosts ?? 0) + (c?.guests ?? 0)) connected", .off)) {
            Row(label: "Host devices", value: String(c?.hosts ?? 0))
            Row(label: "Guests", value: String(c?.guests ?? 0))
            if let apps = c?.apps, !apps.isEmpty {
                Text(apps.sorted { $0.key < $1.key }.map { "\(Self.name($0.key)): \($0.value)" }.joined(separator: " · "))
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    static func name(_ app: String) -> String {
        switch app {
        case "ios": "iPhone app"
        case "mac": "Mac app"
        case "remote": "Web remote"
        case "stage": "Lyrics screen"
        default: app
        }
    }
}

// MARK: Manage

/// Where vocal separation runs and how workers connect. Edits stems.toml
/// through the settings API; these take effect after a server restart.
struct ManageMachines: View {
    @Environment(AppModel.self) private var model
    @Environment(StemsClient.self) private var client
    @Environment(\.dismiss) private var dismiss
    let info: ServerInfo?

    @State private var backend = "mdx"
    @State private var acceptWorkers = true
    @State private var listenPort = "7375"
    @State private var addresses: [String] = []
    @State private var newAddress = ""
    @State private var fallback = "never"
    @State private var loaded = false
    @State private var saving = false
    @State private var error: String?
    @State private var needsRestart = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Picker("Separate vocals on", selection: $backend) {
                        Text("This Mac").tag("mdx")
                        Text("Worker machines").tag("remote")
                        Text("Simple filter (no AI)").tag("dsp")
                    }
                } header: {
                    Text("Vocal Separation")
                } footer: {
                    Text("Workers take the heavy lifting off this Mac, e.g. a Windows laptop with a GPU on the same network.")
                }

                if backend == "remote" {
                    Section {
                        Toggle("Let workers connect to this Mac", isOn: $acceptWorkers)
                        if acceptWorkers {
                            LabeledContent("Port") {
                                TextField("7375", text: $listenPort).frame(width: 80).multilineTextAlignment(.trailing)
                            }
                        }
                    } footer: {
                        Text("The easy way: the worker dials in, so nothing needs opening on it (good for work laptops).")
                    }
                    Section {
                        ForEach(addresses, id: \.self) { a in
                            HStack {
                                Image(systemName: "line.3.horizontal").foregroundStyle(.tertiary)
                                Text(a).monospaced()
                                Spacer()
                                Button("Remove", systemImage: "minus.circle") { addresses.removeAll { $0 == a } }
                                    .labelStyle(.iconOnly).buttonStyle(.borderless)
                            }
                        }
                        .onMove { addresses.move(fromOffsets: $0, toOffset: $1) }
                        HStack {
                            TextField("192.168.1.20:7374", text: $newAddress).monospaced().onSubmit(add)
                            Button("Add", action: add).disabled(newAddress.trimmingCharacters(in: .whitespaces).isEmpty)
                        }
                    } header: {
                        Text("Workers This Mac Dials")
                    } footer: {
                        Text("Tried in this order (drag to reorder): the first is preferred.")
                    }
                    Section {
                        Picker("If no worker answers", selection: $fallback) {
                            Text("Wait for one").tag("never")
                            Text("Use this Mac").tag("when_no_worker")
                            Text("Use this Mac as well, always").tag("always")
                        }
                    }
                }

                Section("Add a Machine") {
                    let ip = info?.lanHost.flatMap { $0.split(separator: ":").first.map(String.init) } ?? "this Mac’s IP"
                    VStack(alignment: .leading, spacing: 8) {
                        Label("Windows worker", systemImage: "pc").font(.headline)
                        Text("Unzip stems-worker-win (arm64 or x64) anywhere, run bench.cmd once, then run-worker.cmd and enter \(ip).")
                            .font(.callout)
                        Label("Mac or Linux worker", systemImage: "desktopcomputer").font(.headline).padding(.top, 4)
                        Text("stemsd worker --connect \(ip)").font(.callout.monospaced()).textSelection(.enabled)
                        Label("Lyrics screen", systemImage: "tv").font(.headline).padding(.top, 4)
                        Text(info?.stageUrl ?? "http://\(ip):7373/stage/").font(.callout.monospaced()).textSelection(.enabled)
                        Label("iPhone", systemImage: "iphone").font(.headline).padding(.top, 4)
                        Text("Stems › Pair iPhone… and scan the code.").font(.callout)
                    }
                    .padding(.vertical, 4)
                }

                if let error {
                    Section { Label(error, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange) }
                }
                if needsRestart {
                    Section {
                        HStack {
                            Label("Saved. Restart the server to apply.", systemImage: "arrow.clockwise")
                            Spacer()
                            Button("Restart Now") {
                                model.supervisor.restart()
                                dismiss()
                            }
                            .buttonStyle(.borderedProminent)
                        }
                    }
                }
            }
            .formStyle(.grouped)
            .navigationTitle("Manage Machines")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save", action: save).buttonStyle(.borderedProminent).disabled(!loaded || saving)
                }
            }
            .task { await load() }
        }
    }

    private func add() {
        let a = newAddress.trimmingCharacters(in: .whitespaces)
        guard !a.isEmpty, !addresses.contains(a) else { return }
        addresses.append(a.contains(":") ? a : a + ":7374")
        newAddress = ""
    }

    private func load() async {
        do {
            let doc = try await client.api.settings()
            backend = doc.string("karaoke.backend") ?? "mdx"
            fallback = doc.string("karaoke.local_fallback") ?? "never"
            let parts = (doc.string("karaoke.remote") ?? "")
                .split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
            let listen = parts.first { $0.hasPrefix("listen:") }
            acceptWorkers = listen != nil || parts.isEmpty
            listenPort = listen.map { String($0.dropFirst("listen:".count)) } ?? "7375"
            addresses = parts.filter { !$0.hasPrefix("listen:") }
            loaded = true
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func save() {
        saving = true
        var remote: [String] = []
        if acceptWorkers { remote.append("listen:\(listenPort.trimmingCharacters(in: .whitespaces))") }
        remote += addresses
        let patch = JSONValue.object([
            "karaoke": .object([
                "backend": .string(backend),
                "remote": .string(backend == "remote" ? remote.joined(separator: ",") : ""),
                "local_fallback": .string(fallback),
            ])
        ])
        Task {
            defer { saving = false }
            do {
                let r = try await client.api.patchSettings(patch)
                error = nil
                needsRestart = !r.restartRequired.isEmpty
                if !needsRestart { dismiss() }
            } catch {
                self.error = error.localizedDescription
            }
        }
    }
}
