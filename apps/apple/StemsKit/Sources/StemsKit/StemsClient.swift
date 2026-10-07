// The live connection to a stems server: a WebSocket for state, clock and
// (hosts) status pushes, REST for commands and everything else.
//
// The socket reconnects on its own with backoff (0.5 s … 10 s), tries each
// of the endpoint's hosts in turn, and treats 4 s without a message as a
// dead connection (the server ticks the clock 10× a second), so a Mac that
// went away is noticed quickly even when TCP doesn't say so.

import Foundation
import Observation

public enum ConnectionState: Equatable, Sendable {
    case idle
    case connecting
    case connected(host: String)
    case waiting(reason: String, retryIn: TimeInterval)

    public var isConnected: Bool { if case .connected = self { true } else { false } }
}

@MainActor
@Observable
public final class StemsClient {
    public private(set) var endpoint: ServerEndpoint
    public private(set) var connection: ConnectionState = .idle
    public private(set) var state = PlaybackState.empty
    public private(set) var clock = PlaybackClock()
    /// The machines view; hosts only, refreshed about once a second.
    public private(set) var status: MachineStatus?
    public private(set) var me: Me?
    /// Last thing that went wrong with a command, for a non-modal banner.
    public var lastError: String?
    /// The host that answered last; URLs for artwork resolve against it.
    public private(set) var activeHost: String

    /// `app` names this client in the server's status (`mac`, `ios`…).
    private let app: String
    private var loop: Task<Void, Never>?
    private var socket: URLSessionWebSocketTask?
    private var lastMessage = Date.distantPast
    private var throttled: [String: Command] = [:]
    private var throttleTasks: [String: Task<Void, Never>] = [:]
    private var wake: CheckedContinuation<Void, Never>?
    private var wakeGeneration = 0

    public init(endpoint: ServerEndpoint, app: String) {
        self.endpoint = endpoint
        self.app = app
        activeHost = endpoint.hosts.first ?? "127.0.0.1"
    }

    public var isHost: Bool { me?.role == .host }

    public func allowed(_ p: Permission) -> Bool { me.map { $0.role == .host || $0.permissions.contains(p) } ?? false }

    public var api: API {
        API(base: endpoint.baseURL(host: activeHost) ?? URL(string: "http://127.0.0.1:7373")!, token: endpoint.token)
    }

    public func url(for pathOrURL: String?) -> URL? { endpoint.resolve(pathOrURL, host: activeHost) }

    /// Current position, extrapolated.
    public func position(now: Double = PlaybackClock.now()) -> Int { clock.position(now: now) }

    // MARK: Lifecycle

    public func start() {
        guard loop == nil else { return }
        loop = Task { [weak self] in await self?.run() }
    }

    public func stop() {
        loop?.cancel()
        loop = nil
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        connection = .idle
        wakeUp()
    }

    /// Point at another server (pairing, a new port).
    public func switchTo(_ endpoint: ServerEndpoint) {
        guard endpoint != self.endpoint else { return }
        stop()
        self.endpoint = endpoint
        activeHost = endpoint.hosts.first ?? activeHost
        state = .empty
        status = nil
        me = nil
        start()
    }

    /// Skip the backoff wait (app came to the foreground, network changed).
    public func reconnectNow() {
        if case .connected = connection {
            // a socket that silently died gets noticed by the watchdog; a
            // fresh check costs nothing
            if Date().timeIntervalSince(lastMessage) > 1.5 { socket?.cancel(with: .goingAway, reason: nil) }
            return
        }
        if loop == nil { start() }
        wakeUp()
    }

    private func wakeUp() {
        wake?.resume()
        wake = nil
    }

    private func run() async {
        var attempt = 0
        while !Task.isCancelled {
            var reason = "Can’t reach \(endpoint.name)."
            for host in endpoint.hosts where !Task.isCancelled {
                connection = .connecting
                if let failure = await session(host: host) {
                    reason = failure
                } else {
                    attempt = 0 // was connected; retry quickly
                }
            }
            if Task.isCancelled { break }
            let delay = min(10, 0.5 * pow(2, Double(attempt)))
            attempt += 1
            connection = .waiting(reason: reason, retryIn: delay)
            await sleepOrWake(delay)
        }
    }

    private func sleepOrWake(_ seconds: TimeInterval) async {
        wakeGeneration += 1
        let generation = wakeGeneration
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            wake = c
            Task { [weak self] in
                try? await Task.sleep(for: .seconds(seconds))
                // only end the wait it was started for
                if let self, self.wakeGeneration == generation { self.wakeUp() }
            }
        }
    }

    /// One connection to `host` until it drops. Returns why it failed
    /// before connecting, or nil if it was connected for a while.
    private func session(host: String) async -> String? {
        guard let base = endpoint.baseURL(host: host),
            var c = URLComponents(url: base.appending(path: "/api/ws"), resolvingAgainstBaseURL: false)
        else { return "Bad server address." }
        c.scheme = "ws"
        var q = [URLQueryItem(name: "app", value: app)]
        if let t = endpoint.token { q.append(URLQueryItem(name: "token", value: t)) }
        c.queryItems = q
        guard let url = c.url else { return "Bad server address." }

        let ws = URLSession.stems.webSocketTask(with: url)
        ws.maximumMessageSize = 8 << 20
        socket = ws
        ws.resume()
        lastMessage = Date()
        var connected = false
        let watchdog = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(1))
                guard let self else { return }
                if Date().timeIntervalSince(self.lastMessage) > 4 {
                    ws.cancel(with: .goingAway, reason: nil)
                    return
                }
            }
        }
        defer { watchdog.cancel() }

        while !Task.isCancelled {
            let message: URLSessionWebSocketTask.Message
            do {
                message = try await ws.receive()
            } catch {
                if socket === ws { socket = nil }
                return connected ? nil : API.describe((error as? URLError) ?? URLError(.cannotConnectToHost))
            }
            lastMessage = Date()
            if !connected {
                connected = true
                activeHost = host
                connection = .connected(host: host)
                Task { await self.refreshMe() }
            }
            let data: Data
            switch message {
            case .string(let s): data = Data(s.utf8)
            case .data(let d): data = d
            @unknown default: continue
            }
            handle(data)
        }
        ws.cancel(with: .goingAway, reason: nil)
        return nil
    }

    private func handle(_ data: Data) {
        guard let msg = try? JSONDecoder().decode(Inbound.self, from: data) else { return }
        switch msg {
        case .state(let s): state = s
        case .clock(let t): clock.apply(t)
        case .status(let s): status = s
        case .error(let m): lastError = m
        case .unknown: break
        }
    }

    public func refreshMe() async {
        me = try? await api.me()
    }

    // MARK: Commands

    /// Send a command; shows the change right away (the server's next
    /// state confirms or corrects it) and reports failures in `lastError`.
    public func send(_ cmd: Command) {
        applyLocally(cmd)
        Task {
            do {
                try await api.command(cmd)
            } catch {
                lastError = error.localizedDescription
            }
        }
    }

    /// For sliders: at most one request in flight per `key`; the latest
    /// value always gets sent.
    public func sendThrottled(_ cmd: Command, key: String) {
        applyLocally(cmd)
        throttled[key] = cmd
        guard throttleTasks[key] == nil else { return }
        throttleTasks[key] = Task {
            while let next = throttled.removeValue(forKey: key) {
                do {
                    try await api.command(next)
                } catch {
                    lastError = error.localizedDescription
                }
                try? await Task.sleep(for: .milliseconds(60))
            }
            throttleTasks[key] = nil
        }
    }

    private func applyLocally(_ cmd: Command) {
        switch cmd {
        case .setKaraoke(let on): state.karaoke.enabled = on
        case .setVocalGain(let g): state.karaoke.vocalGain = g
        case .setVolume(let v): state.volume = v
        case .setViewMode(let m): state.viewMode = m
        case .dequeue(let id): state.queue.removeAll { $0.id == id }
        case .clearQueue: state.queue.removeAll()
        case .moveInQueue(let id, let to):
            if let from = state.queue.firstIndex(where: { $0.id == id }) {
                let item = state.queue.remove(at: from)
                state.queue.insert(item, at: min(max(0, to), state.queue.count))
            }
        default: break
        }
    }

    public func togglePlayPause() {
        send(state.status == .playing ? .pause : .play)
    }

    // MARK: Settings

    /// Change one setting. Returns keys that need a restart of stemsd.
    @discardableResult
    public func setSetting(_ key: String, _ value: JSONValue) async throws -> [String] {
        try await api.patchSettings(.patch(key, value)).restartRequired
    }
}
