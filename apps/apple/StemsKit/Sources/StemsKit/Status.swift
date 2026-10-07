// The machines view: `GET /api/status`, also pushed over the WebSocket to
// hosts once a second. Built in crates/stemsd/src/status.rs (+ `system` and
// `clients` from the server). Every section is optional so a server in
// another mode (Spotify: no library) still decodes.

import Foundation

public struct MachineStatus: Codable, Hashable, Sendable {
    /// `library`, `spotify`, `file`.
    public var mode: String
    public var audio: AudioStatus?
    public var separation: SeparationStatus?
    public var library: LibraryStatus?
    public var system: SystemStats?
    public var clients: Clients?

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        mode = try c.decodeIfPresent(String.self, forKey: .mode) ?? ""
        audio = try c.decodeIfPresent(AudioStatus.self, forKey: .audio)
        separation = try c.decodeIfPresent(SeparationStatus.self, forKey: .separation)
        library = try c.decodeIfPresent(LibraryStatus.self, forKey: .library)
        system = try c.decodeIfPresent(SystemStats.self, forKey: .system)
        clients = try c.decodeIfPresent(Clients.self, forKey: .clients)
    }
}

public struct AudioStatus: Codable, Hashable, Sendable {
    public var source: String
    public var sink: String
    public var sinkLatencyMs: Int
    /// Separation on the audio path (`prebuffer`, `mdx`, `dsp`…).
    public var processor: String
    public var processorDelayMs: Int
    /// Audio between the source and the speakers right now.
    public var bufferedMs: Int
    /// Silence inserted while playing (a dropout) since start…
    public var dropoutMs: Int
    /// …and in how many separate seconds it happened.
    public var dropoutSeconds: Int
    public var uptimeS: Int

    enum CodingKeys: String, CodingKey {
        case source, sink, processor
        case sinkLatencyMs = "sink_latency_ms"
        case processorDelayMs = "processor_delay_ms"
        case bufferedMs = "buffered_ms"
        case dropoutMs = "dropout_ms"
        case dropoutSeconds = "dropout_seconds"
        case uptimeS = "uptime_s"
    }
}

public struct SeparationStatus: Codable, Hashable, Sendable {
    public var backend: String
    public var mode: String
    public var device: String
    /// Worker addresses (`host:port`, `listen:7375`), comma separated.
    public var remote: String
    /// `never`, `when_no_worker`, `always`.
    public var localFallback: String
    public var workers: [WorkerStat]

    enum CodingKeys: String, CodingKey {
        case backend, mode, device, remote, workers
        case localFallback = "local_fallback"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        backend = try c.decodeIfPresent(String.self, forKey: .backend) ?? ""
        mode = try c.decodeIfPresent(String.self, forKey: .mode) ?? ""
        device = try c.decodeIfPresent(String.self, forKey: .device) ?? ""
        remote = try c.decodeIfPresent(String.self, forKey: .remote) ?? ""
        localFallback = try c.decodeIfPresent(String.self, forKey: .localFallback) ?? ""
        workers = try c.decodeIfPresent([WorkerStat].self, forKey: .workers) ?? []
    }
}

public struct WorkerStat: Codable, Hashable, Identifiable, Sendable {
    public var host: String
    public var model: String
    /// ONNX Runtime provider the worker uses (`directml`, `coreml`, `cpu`).
    public var provider: String
    public var align: Bool
    /// Open connections right now (0 = gone).
    public var connections: Int
    public var requests: Int
    public var failures: Int
    /// Smoothed time per separated window.
    public var windowMs: Double
    public var lastError: String?
    /// Seconds since the worker last answered.
    public var idleS: Double?

    public var id: String { host }
    public var isConnected: Bool { connections > 0 }

    enum CodingKeys: String, CodingKey {
        case host, model, provider, align, connections, requests, failures
        case windowMs = "window_ms"
        case lastError = "last_error"
        case idleS = "idle_s"
    }
}

public struct LibraryStatus: Codable, Hashable, Sendable {
    public var dir: String
    /// The drive is plugged in and the folder readable.
    public var available: Bool
    public var tracks: Int
    public var playable: Int
    public var complete: Int
    public var freeBytes: Int64
    public var totalBytes: Int64
    public var `import`: ImportSummary?

    enum CodingKeys: String, CodingKey {
        case dir, available, tracks, playable, complete, `import`
        case freeBytes = "free_bytes"
        case totalBytes = "total_bytes"
    }
}

public struct ImportSummary: Codable, Hashable, Sendable {
    public var running: Bool
    public var current: String?
    public var waiting: Int
    public var lastError: String?

    enum CodingKeys: String, CodingKey {
        case running, current, waiting
        case lastError = "last_error"
    }
}

public struct SystemStats: Codable, Hashable, Sendable {
    public var cpuPercent: Double
    public var memUsedMb: Int
    public var memTotalMb: Int
    /// Hottest sensors first (none on some platforms).
    public var temps: [Temperature]

    public var hottest: Temperature? { temps.first }

    enum CodingKeys: String, CodingKey {
        case temps
        case cpuPercent = "cpu_percent"
        case memUsedMb = "mem_used_mb"
        case memTotalMb = "mem_total_mb"
    }
}

public struct Temperature: Codable, Hashable, Sendable {
    public var label: String
    public var celsius: Double
}

/// Open sessions: per app (`?app=` — `stage`, `remote`, `ios`, `mac`…) and
/// per role.
public struct Clients: Codable, Hashable, Sendable {
    public var apps: [String: Int]
    public var hosts: Int
    public var guests: Int
}
