// Where the server is and how to prove we are the host, plus the pairing
// link the Mac shows as a QR code:
//
//   stems://pair?name=stems&host=192.168.8.10&host=Antoines-Air.local&port=7373&token=XH7ZT8
//
// Several hosts can be given: the LAN IP first (works when phones ignore
// mDNS on a no-internet wifi), then the `.local` name (survives the router
// handing the Mac a new IP).

import Foundation

public struct ServerEndpoint: Codable, Hashable, Sendable {
    public var name: String
    /// Tried in order.
    public var hosts: [String]
    public var port: Int
    /// Host token; nil connects as a guest.
    public var token: String?

    public init(name: String, hosts: [String], port: Int, token: String?) {
        self.name = name
        self.hosts = hosts.filter { !$0.isEmpty }
        self.port = port
        self.token = token
    }

    /// The server on this machine.
    public static func local(port: Int, token: String?, name: String = "This Mac") -> ServerEndpoint {
        ServerEndpoint(name: name, hosts: ["127.0.0.1"], port: port, token: token)
    }

    public func baseURL(host: String) -> URL? {
        var c = URLComponents()
        c.scheme = "http"
        // IPv6 literals need brackets in a URL
        c.host = host.contains(":") && !host.hasPrefix("[") ? "[\(host)]" : host
        c.port = port
        return c.url
    }

    /// Resolve a server path (`/media/library/…/cover.jpg`) or pass a full URL through.
    public func resolve(_ pathOrURL: String?, host: String) -> URL? {
        guard let s = pathOrURL, !s.isEmpty else { return nil }
        if s.hasPrefix("http://") || s.hasPrefix("https://") { return URL(string: s) }
        guard let base = baseURL(host: host) else { return nil }
        return URL(string: s, relativeTo: base)?.absoluteURL
    }

    // MARK: Pairing link

    public static let pairScheme = "stems"

    public var pairingURL: URL? {
        var c = URLComponents()
        c.scheme = Self.pairScheme
        c.host = "pair"
        var items = [URLQueryItem(name: "name", value: name)]
        items += hosts.map { URLQueryItem(name: "host", value: $0) }
        items.append(URLQueryItem(name: "port", value: String(port)))
        if let token { items.append(URLQueryItem(name: "token", value: token)) }
        c.queryItems = items
        return c.url
    }

    /// Parse a pairing link; nil when it isn't one.
    public init?(pairingURL url: URL) {
        guard url.scheme?.lowercased() == Self.pairScheme, url.host?.lowercased() == "pair",
            let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems
        else { return nil }
        let hosts = items.filter { $0.name == "host" }.compactMap(\.value).filter { !$0.isEmpty }
        guard !hosts.isEmpty else { return nil }
        let port = items.first { $0.name == "port" }?.value.flatMap(Int.init) ?? 7373
        let token = items.first { $0.name == "token" }?.value.flatMap { $0.isEmpty ? nil : $0 }
        let name = items.first { $0.name == "name" }?.value ?? hosts[0]
        self.init(name: name, hosts: hosts, port: port, token: token)
    }

    /// From what someone types: `192.168.8.10`, `mac.local:7373`, or a
    /// remote URL such as `http://192.168.8.10:7373/remote/?token=…`.
    public init?(typed text: String, token: String? = nil) {
        var s = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !s.isEmpty else { return nil }
        if let url = URL(string: s), let parsed = ServerEndpoint(pairingURL: url) {
            self = parsed
            return
        }
        if !s.contains("://") { s = "http://" + s }
        guard let c = URLComponents(string: s), let host = c.host, !host.isEmpty else { return nil }
        let urlToken = c.queryItems?.first { $0.name == "token" }?.value
        self.init(name: host, hosts: [host], port: c.port ?? 7373, token: token ?? urlToken)
    }
}
