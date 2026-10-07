// Runs stemsd for the Mac app: finds the binary, reads its port and host
// token (`stemsd config --json`), starts it in the chosen mode, keeps its
// log, and restarts it with backoff if it crashes. While it runs, the Mac
// is kept awake and out of App Nap.
//
// If a server is already answering on the port (started from a terminal,
// or left over from a crash of this app), the app uses that one instead of
// starting a second.

import AppKit
import Foundation
import Observation
import StemsKit

enum ServerMode: String, CaseIterable, Identifiable {
    case library, spotify

    var id: String { rawValue }

    var title: String {
        switch self {
        case .library: "Library (offline party)"
        case .spotify: "Spotify Connect"
        }
    }
}

struct LogLine: Identifiable, Hashable {
    let id: Int
    let date: Date
    let text: String

    var isProblem: Bool { text.contains(" WARN ") || text.contains(" ERROR ") || text.hasPrefix("Error") }
}

@MainActor
@Observable
final class ServerSupervisor {
    enum State: Equatable {
        case stopped
        case starting
        case running(pid: Int32)
        /// Another stemsd already serves the port; we just talk to it.
        case external
        case restarting(at: Date, attempt: Int)
        /// Gave up after repeated crashes, or couldn't start at all.
        case failed(String)

        var isUp: Bool {
            switch self {
            case .running, .external: true
            default: false
            }
        }
    }

    private(set) var state: State = .stopped
    private(set) var config: DaemonConfig?
    private(set) var log: [LogLine] = []
    private(set) var binary: URL?
    private(set) var thermal: ProcessInfo.ThermalState = ProcessInfo.processInfo.thermalState
    /// Called with the endpoint once the port and token are known.
    var onEndpoint: ((ServerEndpoint) -> Void)?

    private var process: Process?
    private var stopping = false
    private var crashes: [Date] = []
    private var restartTask: Task<Void, Never>?
    private var activity: NSObjectProtocol?
    private var nextLogID = 0
    private var partial = ""

    static let maxLogLines = 5000

    var mode: ServerMode {
        ServerMode(rawValue: UserDefaults.standard.string(forKey: "serverMode") ?? "") ?? .library
    }

    init() {
        NotificationCenter.default.addObserver(
            forName: ProcessInfo.thermalStateDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.thermal = ProcessInfo.processInfo.thermalState }
        }
    }

    // MARK: Locating things

    /// Custom path from Settings, the copy inside the app, or this
    /// checkout's release build (when running from Xcode without embedding).
    static func findBinary() -> URL? {
        let fm = FileManager.default
        if let custom = UserDefaults.standard.string(forKey: "stemsdPath"), !custom.isEmpty,
            fm.isExecutableFile(atPath: custom)
        {
            return URL(fileURLWithPath: custom)
        }
        if let bundled = Bundle.main.url(forAuxiliaryExecutable: "stemsd"), fm.isExecutableFile(atPath: bundled.path) {
            return bundled
        }
        for build in ["release", "debug"] {
            let dev = repoRoot.appending(path: "target/\(build)/stemsd")
            if fm.isExecutableFile(atPath: dev.path) { return dev }
        }
        return nil
    }

    /// `--config` for stemsd: a stems.toml chosen in Settings (e.g. on the
    /// library drive), or nil for stemsd's default location.
    nonisolated static var configOverride: String? {
        let path = UserDefaults.standard.string(forKey: "stemsdConfig") ?? ""
        return path.isEmpty ? nil : path
    }

    static func findWebDir() -> URL? {
        let fm = FileManager.default
        if let res = Bundle.main.resourceURL?.appending(path: "web"),
            fm.fileExists(atPath: res.appending(path: "remote/dist/index.html").path)
        {
            return res
        }
        let dev = repoRoot.appending(path: "web")
        return fm.fileExists(atPath: dev.path) ? dev : nil
    }

    /// This checkout (apps/apple/Mac/ServerSupervisor.swift → repo root).
    static let repoRoot = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()

    // MARK: Lifecycle

    func start() {
        guard process == nil, !state.isUp else { return }
        restartTask?.cancel()
        stopping = false
        state = .starting
        Task { await launch() }
    }

    func restart() {
        Task {
            await stop()
            crashes.removeAll()
            start()
        }
    }

    /// Ask stemsd to exit (Ctrl-C, so it saves the queue), then insist.
    func stop() async {
        restartTask?.cancel()
        stopping = true
        guard let p = process, p.isRunning else {
            if state != .external { state = .stopped }
            endActivity()
            return
        }
        p.interrupt()
        for _ in 0..<40 where p.isRunning {
            try? await Task.sleep(for: .milliseconds(100))
        }
        if p.isRunning { p.terminate() }
        for _ in 0..<20 where p.isRunning {
            try? await Task.sleep(for: .milliseconds(100))
        }
        if p.isRunning { kill(p.processIdentifier, SIGKILL) }
        process = nil
        state = .stopped
        endActivity()
    }

    private func launch() async {
        guard let bin = Self.findBinary() else {
            state = .failed("stemsd wasn’t found. Build it with `cargo build --release -p stemsd --features coreml`, or choose it in Settings.")
            return
        }
        binary = bin
        do {
            let cfg = try await Self.readConfig(bin)
            config = cfg
            onEndpoint?(.local(port: cfg.port, token: cfg.hostToken, name: Host.current().localizedName ?? "This Mac"))
            if await Self.isServing(port: cfg.port) {
                append("A server is already running on port \(cfg.port); using it.")
                state = .external
                watchExternal(port: cfg.port)
                return
            }
        } catch {
            state = .failed("Couldn’t read the stemsd configuration: \(error.localizedDescription)")
            return
        }
        spawn(bin)
    }

    /// Take over if the server someone else started goes away.
    private func watchExternal(port: Int) {
        restartTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(3))
                if await Self.isServing(port: port) { continue }
                guard let self, self.state == .external, !Task.isCancelled else { return }
                self.append("The other server on port \(port) stopped; starting our own.")
                self.state = .starting
                await self.launch()
                return
            }
        }
    }

    private func spawn(_ bin: URL) {
        let p = Process()
        p.executableURL = bin
        var args: [String] = []
        if let cfg = Self.configOverride { args += ["--config", cfg] }
        if let web = Self.findWebDir() { args += ["--web-dir", web.path] }
        args.append(mode.rawValue)
        p.arguments = args
        var env = ProcessInfo.processInfo.environment
        env["NO_COLOR"] = "1"
        env["STEMS_NO_WEB_BUILD"] = "1"
        p.environment = env
        // relative paths in stems.toml resolve from the home folder
        p.currentDirectoryURL = FileManager.default.homeDirectoryForCurrentUser

        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        pipe.fileHandleForReading.readabilityHandler = { [weak self] h in
            let data = h.availableData
            guard !data.isEmpty else {
                h.readabilityHandler = nil
                return
            }
            let text = String(decoding: data, as: UTF8.self)
            Task { @MainActor in self?.ingest(text) }
        }
        p.terminationHandler = { [weak self] proc in
            let pid = proc.processIdentifier
            let status = proc.terminationStatus
            let reason = proc.terminationReason
            Task { @MainActor in self?.exited(pid: pid, status: status, reason: reason) }
        }
        append("$ \(bin.path) \(args.joined(separator: " "))")
        do {
            try p.run()
        } catch {
            state = .failed("Couldn’t start stemsd: \(error.localizedDescription)")
            return
        }
        process = p
        state = .running(pid: p.processIdentifier)
        beginActivity()
    }

    private func exited(pid: Int32, status: Int32, reason: Process.TerminationReason) {
        // a late callback from a process stop() already replaced
        guard process == nil || process?.processIdentifier == pid else { return }
        process = nil
        if stopping {
            state = .stopped
            return
        }
        let how = reason == .uncaughtSignal ? "was killed (signal \(status))" : "exited with status \(status)"
        append("stemsd \(how)")
        let now = Date()
        crashes = crashes.filter { now.timeIntervalSince($0) < 120 } + [now]
        if crashes.count >= 5 {
            endActivity()
            state = .failed("stemsd stopped \(crashes.count) times in two minutes (it \(how)).")
            return
        }
        let delay = min(30, pow(2, Double(crashes.count - 1)))
        let attempt = crashes.count
        state = .restarting(at: now.addingTimeInterval(delay), attempt: attempt)
        restartTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(delay))
            guard !Task.isCancelled, let self else { return }
            self.state = .starting
            await self.launch()
        }
    }

    // MARK: Sleep and App Nap

    /// No idle sleep, no App Nap, no timer coalescing while the party runs.
    private func beginActivity() {
        guard activity == nil else { return }
        activity = ProcessInfo.processInfo.beginActivity(
            options: [.userInitiated, .idleSystemSleepDisabled, .latencyCritical],
            reason: "stems is running a party")
    }

    private func endActivity() {
        if let a = activity { ProcessInfo.processInfo.endActivity(a) }
        activity = nil
    }

    var preventsSleep: Bool { activity != nil }

    // MARK: Log

    private func ingest(_ text: String) {
        let combined = partial + text
        var lines = combined.components(separatedBy: "\n")
        partial = lines.removeLast()
        for line in lines where !line.isEmpty { append(Self.stripANSI(line)) }
    }

    private func append(_ text: String) {
        log.append(LogLine(id: nextLogID, date: Date(), text: text))
        nextLogID += 1
        if log.count > Self.maxLogLines { log.removeFirst(log.count - Self.maxLogLines) }
    }

    func clearLog() { log.removeAll() }

    static func stripANSI(_ s: String) -> String {
        s.replacingOccurrences(of: "\u{1B}\\[[0-9;]*m", with: "", options: .regularExpression)
    }

    // MARK: Helpers

    static func readConfig(_ bin: URL) async throws -> DaemonConfig {
        let configOverride = Self.configOverride
        return try await Task.detached {
            let p = Process()
            p.executableURL = bin
            p.arguments = (configOverride.map { ["--config", $0] } ?? []) + ["config", "--json"]
            let out = Pipe()
            p.standardOutput = out
            p.standardError = FileHandle.nullDevice
            try p.run()
            let data = out.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            guard p.terminationStatus == 0 else {
                throw APIError(message: "`stemsd config --json` exited with status \(p.terminationStatus)")
            }
            return try JSONDecoder().decode(DaemonConfig.self, from: data)
        }.value
    }

    static func isServing(port: Int) async -> Bool {
        guard let url = URL(string: "http://127.0.0.1:\(port)") else { return false }
        return (try? await API(base: url, token: nil).health()) != nil
    }
}

extension ProcessInfo.ThermalState {
    var title: String {
        switch self {
        case .nominal: "Normal"
        case .fair: "Warm"
        case .serious: "Hot (slowing down)"
        case .critical: "Critical"
        @unknown default: "Unknown"
        }
    }
}
