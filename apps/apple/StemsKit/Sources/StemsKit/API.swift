// The REST half of the server API (crates/stems-server/src/lib.rs).

import Foundation

public struct APIError: LocalizedError, Sendable {
    public var status: Int?
    public var message: String

    public var errorDescription: String? { message }

    public init(status: Int? = nil, message: String) {
        self.status = status
        self.message = message
    }
}

public struct API: Sendable {
    public var base: URL
    public var token: String?
    var session: URLSession

    public init(base: URL, token: String?, session: URLSession = .stems) {
        self.base = base
        self.token = token
        self.session = session
    }

    public static func decoder() -> JSONDecoder { JSONDecoder() }

    private func request(_ path: String, query: [URLQueryItem] = [], method: String = "GET", body: Data? = nil) -> URLRequest {
        var c = URLComponents(url: base.appending(path: path), resolvingAgainstBaseURL: false)!
        if !query.isEmpty { c.queryItems = query }
        var r = URLRequest(url: c.url!)
        r.httpMethod = method
        if let token { r.setValue(token, forHTTPHeaderField: "x-stems-token") }
        if let body {
            r.httpBody = body
            r.setValue("application/json", forHTTPHeaderField: "content-type")
        }
        return r
    }

    private func send(_ r: URLRequest) async throws -> Data {
        let data: Data
        let response: URLResponse
        do {
            (data, response) = try await session.data(for: r)
        } catch let e as URLError {
            throw APIError(message: Self.describe(e))
        }
        guard let http = response as? HTTPURLResponse else { return data }
        guard (200..<300).contains(http.statusCode) else {
            let msg = (try? JSONDecoder().decode([String: String].self, from: data))?["error"]
                ?? String(data: data, encoding: .utf8).flatMap { $0.isEmpty ? nil : $0 }
                ?? HTTPURLResponse.localizedString(forStatusCode: http.statusCode)
            throw APIError(status: http.statusCode, message: msg)
        }
        return data
    }

    private func get<T: Decodable>(_ path: String, query: [URLQueryItem] = [], as: T.Type = T.self) async throws -> T {
        let data = try await send(request(path, query: query))
        return try Self.decoder().decode(T.self, from: data)
    }

    static func describe(_ e: URLError) -> String {
        switch e.code {
        case .timedOut: "The server didn’t answer in time."
        case .cannotConnectToHost: "The server isn’t running or can’t be reached."
        case .notConnectedToInternet, .networkConnectionLost: "Not connected to the network."
        case .cannotFindHost, .dnsLookupFailed: "Can’t find the server on this network."
        default: e.localizedDescription
        }
    }

    // MARK: Endpoints

    public func health() async throws {
        _ = try await send(request("/api/health"))
    }

    public func me() async throws -> Me { try await get("/api/me") }
    public func info() async throws -> ServerInfo { try await get("/api/info") }
    public func state() async throws -> PlaybackState { try await get("/api/state") }
    public func status() async throws -> MachineStatus { try await get("/api/status") }

    public func command(_ cmd: Command) async throws {
        _ = try await send(request("/api/command", method: "POST", body: try JSONEncoder().encode(cmd)))
    }

    public func library(query: String = "") async throws -> [LibraryTrack] {
        try await get("/api/library", query: [URLQueryItem(name: "q", value: query)])
    }

    public func importStatus() async throws -> ImportStatus { try await get("/api/library/import") }

    public func importSources(_ sources: [String]) async throws {
        let body = try JSONEncoder().encode(["sources": sources])
        _ = try await send(request("/api/library/import", method: "POST", body: body))
    }

    public func settings() async throws -> SettingsDocument { try await get("/api/settings") }

    @discardableResult
    public func patchSettings(_ patch: JSONValue) async throws -> SettingsUpdate {
        let data = try await send(request("/api/settings", method: "PATCH", body: try JSONEncoder().encode(patch)))
        return try Self.decoder().decode(SettingsUpdate.self, from: data)
    }
}

extension URLSession {
    /// Short timeouts: everything is on the LAN, and a hung request should
    /// turn into an error people can see, not a frozen control.
    public static let stems: URLSession = {
        let c = URLSessionConfiguration.ephemeral
        c.timeoutIntervalForRequest = 6
        c.timeoutIntervalForResource = 30
        c.waitsForConnectivity = false
        c.requestCachePolicy = .reloadIgnoringLocalCacheData
        return URLSession(configuration: c)
    }()
}
