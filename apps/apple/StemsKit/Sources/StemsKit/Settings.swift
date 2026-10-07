// `GET/PATCH /api/settings` (crates/stemsd/src/settings.rs). The config is
// kept as generic JSON so settings the app doesn't know about survive; the
// typed accessors cover what the screens show.

import Foundation

public enum JSONValue: Codable, Hashable, Sendable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let b = try? c.decode(Bool.self) {
            self = .bool(b)
        } else if let n = try? c.decode(Double.self) {
            self = .number(n)
        } else if let s = try? c.decode(String.self) {
            self = .string(s)
        } else if let a = try? c.decode([JSONValue].self) {
            self = .array(a)
        } else {
            self = .object(try c.decode([String: JSONValue].self))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let b): try c.encode(b)
        case .number(let n): try c.encode(n)
        case .string(let s): try c.encode(s)
        case .array(let a): try c.encode(a)
        case .object(let o): try c.encode(o)
        }
    }

    public subscript(key: String) -> JSONValue? {
        if case .object(let o) = self { o[key] } else { nil }
    }

    /// `value(at: "library.target_lufs")`
    public func value(at dotted: String) -> JSONValue? {
        dotted.split(separator: ".").reduce(Optional(self)) { v, k in v?[String(k)] }
    }

    public var bool: Bool? { if case .bool(let b) = self { b } else { nil } }
    public var number: Double? { if case .number(let n) = self { n } else { nil } }
    public var string: String? { if case .string(let s) = self { s } else { nil } }

    /// `{"a": {"b": value}}` for `a.b`: the shape a settings patch takes.
    public static func patch(_ dotted: String, _ value: JSONValue) -> JSONValue {
        dotted.split(separator: ".").reversed().reduce(value) { inner, k in .object([String(k): inner]) }
    }
}

public struct SettingsDocument: Codable, Hashable, Sendable {
    public var config: JSONValue
    /// Dotted keys that apply without restarting stemsd.
    public var liveKeys: [String]

    enum CodingKeys: String, CodingKey {
        case config
        case liveKeys = "live_keys"
    }

    public init(config: JSONValue, liveKeys: [String]) {
        self.config = config
        self.liveKeys = liveKeys
    }

    public func bool(_ key: String) -> Bool? { config.value(at: key)?.bool }
    public func number(_ key: String) -> Double? { config.value(at: key)?.number }
    public func string(_ key: String) -> String? { config.value(at: key)?.string }

    /// Whether guests may use `permission` (`guests.<name>`).
    public func guestAllowed(_ p: Permission) -> Bool { bool("guests.\(p.rawValue)") ?? false }

    public func isLive(_ key: String) -> Bool { liveKeys.contains(key) }

    /// The same document with one value replaced, for showing a change
    /// before the server confirms it.
    public func setting(_ key: String, to value: JSONValue) -> SettingsDocument {
        var copy = self
        copy.config = copy.config.merging(.patch(key, value))
        return copy
    }
}

extension JSONValue {
    /// Deep merge (objects merge, anything else is replaced), like the
    /// server applies a patch.
    public func merging(_ other: JSONValue) -> JSONValue {
        guard case .object(var mine) = self, case .object(let theirs) = other else { return other }
        for (k, v) in theirs {
            mine[k] = mine[k].map { $0.merging(v) } ?? v
        }
        return .object(mine)
    }
}

/// What a settings patch changed.
public struct SettingsUpdate: Codable, Hashable, Sendable {
    public var changed: [String]
    /// Changed keys that only take effect after restarting stemsd.
    public var restartRequired: [String]

    enum CodingKeys: String, CodingKey {
        case changed
        case restartRequired = "restart_required"
    }
}

/// What `stemsd config --json` prints; the Mac app reads it to find the
/// port and host token of the server it runs.
public struct DaemonConfig: Codable, Hashable, Sendable {
    public var configFile: String
    public var cacheDir: String
    public var modelsDir: String
    public var libraryDir: String
    public var portable: Bool
    public var port: Int
    public var hostToken: String
    public var deviceName: String
    public var config: JSONValue

    enum CodingKeys: String, CodingKey {
        case portable, port, config
        case configFile = "config_file"
        case cacheDir = "cache_dir"
        case modelsDir = "models_dir"
        case libraryDir = "library_dir"
        case hostToken = "host_token"
        case deviceName = "device_name"
    }
}
