// Against a running server, only when asked:
//   STEMS_LIVE=127.0.0.1:7399 STEMS_LIVE_TOKEN=… swift test --filter Live

import Foundation
import Testing

@testable import StemsKit

let liveEndpoint: ServerEndpoint? = ProcessInfo.processInfo.environment["STEMS_LIVE"].flatMap {
    ServerEndpoint(typed: $0, token: ProcessInfo.processInfo.environment["STEMS_LIVE_TOKEN"])
}

@Suite(.enabled(if: liveEndpoint != nil), .serialized)
@MainActor
struct Live {
    func waitFor(_ what: String, timeout: Double = 5, _ cond: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !cond() {
            if Date() > deadline { throw APIError(message: "timed out waiting for \(what)") }
            try await Task.sleep(for: .milliseconds(50))
        }
    }

    @Test func connectsAndFollowsState() async throws {
        let client = StemsClient(endpoint: liveEndpoint!, app: "tests")
        client.start()
        defer { client.stop() }
        try await waitFor("connection") { client.connection.isConnected }
        try await waitFor("first state") { client.state.revision > 0 }
        try await waitFor("role") { client.me != nil }
        if client.isHost {
            try await waitFor("status push") { client.status != nil }
            let lib = try await client.api.library()
            #expect(!lib.isEmpty)
            _ = try await client.api.settings()
            _ = try await client.api.importStatus()
        }
        // volume round trip through the server
        let before = client.state.volume
        let target = before > 0.5 ? 0.4 : 0.6
        client.send(.setVolume(target))
        let rev = client.state.revision
        try await waitFor("server echo") { client.state.revision > rev }
        #expect(abs(client.state.volume - target) < 0.01)
        client.send(.setVolume(before))
    }
}
