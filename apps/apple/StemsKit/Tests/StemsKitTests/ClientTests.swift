import Foundation
import Testing

@testable import StemsKit

@Suite struct Clock {
    func tick(_ pos: Int, _ status: PlaybackStatus = .playing) -> ClockTick {
        ClockTick(revision: 1, status: status, positionMs: pos, serverMs: 0)
    }

    @Test func extrapolatesWhilePlaying() {
        var c = PlaybackClock()
        c.apply(tick(10_000), now: 1_000)
        #expect(c.position(now: 1_500) == 10_500)
    }

    @Test func holdsWhilePaused() {
        var c = PlaybackClock()
        c.apply(tick(10_000, .paused), now: 1_000)
        #expect(c.position(now: 9_000) == 10_000)
    }

    @Test func slewsSmallErrorsAndSnapsLargeOnes() {
        var c = PlaybackClock()
        c.apply(tick(10_000), now: 0)
        // 100 ms ahead of the extrapolation: slewed by 15 %
        c.apply(tick(10_200), now: 100)
        #expect(c.position(now: 100) == 10_115)
        // a seek: snaps
        c.apply(tick(60_000), now: 200)
        #expect(c.position(now: 200) == 60_000)
    }
}

@Suite struct Endpoints {
    @Test func pairingLinkRoundTrips() throws {
        let e = ServerEndpoint(name: "stems", hosts: ["192.168.8.10", "Antoines-Air.local"], port: 7373, token: "XH7ZT8")
        let url = try #require(e.pairingURL)
        #expect(url.scheme == "stems")
        #expect(ServerEndpoint(pairingURL: url) == e)
    }

    @Test func pairingLinkNeedsAHost() {
        #expect(ServerEndpoint(pairingURL: URL(string: "stems://pair?port=7373")!) == nil)
        #expect(ServerEndpoint(pairingURL: URL(string: "https://example.com/pair?host=x")!) == nil)
    }

    @Test func typedAddresses() throws {
        let a = try #require(ServerEndpoint(typed: " 192.168.8.10 "))
        #expect(a.hosts == ["192.168.8.10"])
        #expect(a.port == 7373)
        #expect(a.token == nil)
        let b = try #require(ServerEndpoint(typed: "http://192.168.8.10:7399/remote/?token=ABC"))
        #expect(b.port == 7399)
        #expect(b.token == "ABC")
        let c = try #require(ServerEndpoint(typed: "mac.local:8000", token: "T"))
        #expect(c.hosts == ["mac.local"])
        #expect(c.port == 8000)
        #expect(c.token == "T")
        #expect(ServerEndpoint(typed: "   ") == nil)
    }

    @Test func resolvesServerPathsAndPassesURLsThrough() {
        let e = ServerEndpoint.local(port: 7373, token: nil)
        #expect(
            e.resolve("/media/library/k/cover.jpg", host: "127.0.0.1")?.absoluteString
                == "http://127.0.0.1:7373/media/library/k/cover.jpg")
        #expect(e.resolve("https://i.scdn.co/image/x", host: "127.0.0.1")?.host == "i.scdn.co")
        #expect(e.resolve(nil, host: "127.0.0.1") == nil)
    }
}

@Suite struct Commands {
    func json(_ c: Command) throws -> [String: JSONValue] {
        let data = try JSONEncoder().encode(c)
        guard case .object(let o) = try JSONDecoder().decode(JSONValue.self, from: data) else { return [:] }
        return o
    }

    @Test func encodeLikeTheServerExpects() throws {
        #expect(try json(.play) == ["cmd": .string("play")])
        #expect(try json(.setKaraoke(true)) == ["cmd": .string("set_karaoke"), "enabled": .bool(true)])
        #expect(try json(.seek(positionMs: 1500)) == ["cmd": .string("seek"), "position_ms": .number(1500)])
        #expect(
            try json(.enqueue(key: "k", singer: nil, next: true, kind: .play)) == [
                "cmd": .string("enqueue"), "key": .string("k"), "next": .bool(true), "kind": .string("play"),
            ])
        #expect(
            try json(.playTrack(key: "k", singer: "Mia", kind: .sing)) == [
                "cmd": .string("play_track"), "key": .string("k"), "singer": .string("Mia"), "kind": .string("sing"),
            ])
        #expect(try json(.moveInQueue(id: 4, to: 0)) == ["cmd": .string("move_in_queue"), "id": .number(4), "to": .number(0)])
    }

    @Test func settingsPatchShape() {
        #expect(JSONValue.patch("guests.queue", .bool(true)) == .object(["guests": .object(["queue": .bool(true)])]))
        let doc = SettingsDocument(config: .object(["guests": .object(["queue": .bool(false), "vocals": .bool(true)])]), liveKeys: [])
        let changed = doc.setting("guests.queue", to: .bool(true))
        #expect(changed.guestAllowed(.queue))
        #expect(changed.guestAllowed(.vocals))
    }
}
