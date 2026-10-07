// Decodes every payload the Rust side writes (`cargo test -p stemsd
// fixtures`), so the app and the server can't drift apart unnoticed.

import Foundation
import Testing

@testable import StemsKit

func fixture(_ name: String) throws -> Data {
    let url = try #require(Bundle.module.url(forResource: name, withExtension: "json", subdirectory: "Fixtures"))
    return try Data(contentsOf: url)
}

func decode<T: Decodable>(_ type: T.Type, _ name: String) throws -> T {
    try JSONDecoder().decode(T.self, from: fixture(name))
}

@Suite struct Fixtures {
    @Test func everyFixtureIsCovered() throws {
        let dir = try #require(Bundle.module.url(forResource: "Fixtures", withExtension: nil))
        let names = try FileManager.default.contentsOfDirectory(atPath: dir.path)
            .filter { $0.hasSuffix(".json") }.map { String($0.dropLast(5)) }.sorted()
        let covered: Set = [
            "config_json", "import_status_busy", "import_status_idle", "info", "library_guest", "library_host",
            "me_guest", "me_host", "settings", "status", "status_spotify", "ws_clock", "ws_error", "ws_state",
            "ws_state_idle", "ws_state_intermission", "ws_status",
        ]
        #expect(Set(names) == covered, "add a test for each new fixture")
    }

    @Test func playingState() throws {
        guard case .state(let s) = try decode(Inbound.self, "ws_state") else {
            Issue.record("not a state message")
            return
        }
        #expect(s.status == .playing)
        #expect(s.isLibrary)
        #expect(s.track?.title == "Ni**as In Paris")
        #expect(s.track?.artistLine == "JAY-Z, Kanye West")
        #expect(s.current?.singer == "Mia")
        #expect(s.current?.kind == .sing)
        #expect(s.queue.map(\.id) == [3, 4])
        #expect(s.queue[0].kind == .play)
        #expect(s.queue[0].singer == nil)
        #expect(s.queue[1].singer == "Sam & Alex")
        #expect(s.karaoke.enabled)
        #expect(abs(s.karaoke.vocalGain - 0.15) < 1e-6)
        #expect(s.karaoke.stems?.done == true)
        #expect(s.lyrics?.lines.first?.words.count == 5)
        #expect(s.lyrics?.lines.last?.words.isEmpty == true)
        #expect(s.lyrics?.offsetMs == -120)
        #expect(s.viewMode == .video)
        #expect(s.media.videoOffsetMs == 350)
        #expect(s.revision == 42)
        #expect(s.nextTrack?.durationMs == 196_652)
    }

    @Test func idleAndIntermissionStates() throws {
        guard case .state(let idle) = try decode(Inbound.self, "ws_state_idle"),
            case .state(let bg) = try decode(Inbound.self, "ws_state_intermission")
        else {
            Issue.record("not state messages")
            return
        }
        #expect(idle.status == .idle)
        #expect(idle.track == nil)
        #expect(idle.queue.isEmpty)
        #expect(bg.intermission)
        #expect(bg.current == nil)
        #expect(bg.lyrics?.isSynced == false)
        #expect(bg.lyrics?.plain?.contains("Second line") == true)
    }

    @Test func clockAndError() throws {
        guard case .clock(let t) = try decode(Inbound.self, "ws_clock") else {
            Issue.record("not a clock message")
            return
        }
        #expect(t.positionMs == 16_250)
        #expect(t.status == .playing)
        guard case .error(let m) = try decode(Inbound.self, "ws_error") else {
            Issue.record("not an error message")
            return
        }
        #expect(m.contains("queue"))
    }

    @Test func machineStatus() throws {
        let s = try decode(MachineStatus.self, "status")
        #expect(s.mode == "library")
        #expect(s.audio?.dropoutSeconds == 1)
        #expect(s.audio?.uptimeS == 3725)
        let sep = try #require(s.separation)
        #expect(sep.localFallback == "never")
        #expect(sep.remote == "listen:7375")
        #expect(sep.workers.count == 1)
        #expect(sep.workers[0].provider == "directml")
        #expect(sep.workers[0].isConnected)
        #expect(sep.workers[0].lastError != nil)
        let lib = try #require(s.library)
        #expect(lib.available)
        #expect(lib.tracks == 2)
        #expect(lib.playable == 2)
        #expect(lib.complete == 1)
        #expect(lib.freeBytes == 156_814_767_167)
        #expect(lib.import?.running == false)
        #expect(s.system?.memTotalMb == 8192)
        #expect(s.system?.hottest?.celsius == 71.5)
        #expect(s.clients?.apps["remote"] == 3)
        #expect(s.clients?.hosts == 2)

        guard case .status(let pushed) = try decode(Inbound.self, "ws_status") else {
            Issue.record("not a status message")
            return
        }
        #expect(pushed == s)

        let bare = try decode(MachineStatus.self, "status_spotify")
        #expect(bare.mode == "spotify")
        #expect(bare.library == nil)
        #expect(bare.separation?.workers.isEmpty == true)
        #expect(bare.clients?.apps.isEmpty == true)
    }

    @Test func library() throws {
        let host = try decode([LibraryTrack].self, "library_host")
        #expect(host.count == 2)
        let demo = try #require(host.first { $0.key.hasPrefix("local_") })
        #expect(demo.playable)
        #expect(demo.complete == false)
        #expect(demo.stages?.lyrics == .failed("no lyrics found on LRCLIB"))
        #expect(demo.stages?.video == .skipped)
        #expect(demo.art == nil)
        let full = try #require(host.first { $0.key.hasPrefix("spotify_") })
        #expect(full.complete == true)
        #expect(full.stages?.all.allSatisfy { $0.state.isDone } == true)
        #expect(full.videoOffsetMs == 350)
        #expect(full.loudnessLufs != nil)

        let guest = try decode([LibraryTrack].self, "library_guest")
        #expect(guest.count == 2)
        #expect(guest.allSatisfy { $0.stages == nil && $0.complete == nil })
    }

    @Test func importStatus() throws {
        let idle = try decode(ImportStatus.self, "import_status_idle")
        #expect(!idle.running)
        #expect(idle.finished.isEmpty)
        let busy = try decode(ImportStatus.self, "import_status_busy")
        #expect(busy.running)
        #expect(busy.current?.contains("Paris") == true)
        #expect(busy.waiting.count == 1)
        #expect(busy.finished.first?.problems.first?.hasPrefix("lyrics:") == true)
        #expect(busy.errors.count == 1)
    }

    @Test func settingsMeInfoConfig() throws {
        let s = try decode(SettingsDocument.self, "settings")
        #expect(s.string("host_token") == "XH7ZT8")
        #expect(s.number("library.target_lufs") == -14)
        #expect(s.bool("library.auto_karaoke") == true)
        #expect(s.guestAllowed(.queue) == false)
        #expect(s.isLive("guests.queue"))
        #expect(!s.isLive("port"))
        for p in Permission.allCases {
            #expect(s.isLive("guests.\(p.rawValue)"), "guests.\(p.rawValue) should apply live")
        }

        let host = try decode(Me.self, "me_host")
        #expect(host.role == .host)
        #expect(host.permissions == Permission.allCases)
        let guest = try decode(Me.self, "me_guest")
        #expect(guest.role == .guest)
        #expect(guest.permissions == [.karaoke, .vocals, .stage]) // the defaults

        let info = try decode(ServerInfo.self, "info")
        #expect(info.lanHost == "192.168.8.10:7373")

        let cfg = try decode(DaemonConfig.self, "config_json")
        #expect(cfg.port == 7373)
        #expect(cfg.hostToken == "XH7ZT8")
        #expect(cfg.libraryDir == "/Volumes/Party/stems-library")
        #expect(cfg.config.value(at: "karaoke.backend")?.string == "dsp")
    }
}
