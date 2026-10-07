// The server sends `clock` ticks at ~10 Hz. Between ticks the position is
// extrapolated from a monotonic clock; small differences are slewed so the
// progress bar doesn't jitter, large ones (seek, new track) snap. Same
// rules as web/shared/client.svelte.ts.

import Foundation

public struct PlaybackClock: Hashable, Sendable {
    static let snapThresholdMs = 150.0
    static let slew = 0.15

    /// position − now at the last tick.
    private var offset = 0.0
    private var lastTickPosition = 0
    public private(set) var status: PlaybackStatus = .idle

    public init() {}

    /// Milliseconds on a clock that never jumps (uptime).
    public static func now() -> Double { ProcessInfo.processInfo.systemUptime * 1000 }

    public mutating func apply(_ tick: ClockTick, now: Double = PlaybackClock.now()) {
        lastTickPosition = tick.positionMs
        let wasPlaying = status == .playing
        status = tick.status
        let target = Double(tick.positionMs) - now
        if !wasPlaying || abs(target - offset) > Self.snapThresholdMs {
            offset = target
        } else {
            offset += (target - offset) * Self.slew
        }
    }

    /// Where playback is at `now`, in ms.
    public func position(now: Double = PlaybackClock.now()) -> Int {
        guard status == .playing else { return lastTickPosition }
        return max(0, Int(offset + now))
    }
}
