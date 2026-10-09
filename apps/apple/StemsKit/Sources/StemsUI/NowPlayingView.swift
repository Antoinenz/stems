// What's playing, who's singing, and the controls the host reaches for
// most: play/pause, skip, karaoke on/off, guide vocals, volume.

import StemsKit
import SwiftUI

public struct NowPlayingView: View {
    @Environment(StemsClient.self) private var client
    @Environment(\.horizontalSizeClass) private var sizeClass

    public init() {}

    public var body: some View {
        ScrollView {
            Group {
                if sizeClass == .compact {
                    VStack(spacing: 24) {
                        artwork.frame(maxWidth: 260)
                        controls
                    }
                } else {
                    HStack(alignment: .top, spacing: 32) {
                        artwork.frame(minWidth: 200, maxWidth: 360)
                        controls.frame(minWidth: 300, maxWidth: 520)
                    }
                }
            }
            .padding(.horizontal, 24)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity)
        }
        .navigationTitle("Now Playing")
    }

    private var artwork: some View {
        ArtworkView(url: client.url(for: client.state.track?.art), cornerRadius: 14)
            .shadow(color: .black.opacity(0.25), radius: 16, y: 8)
    }

    private var controls: some View {
        VStack(alignment: .leading, spacing: 22) {
            StageBanner()
            TrackHeader()
            ProgressSection()
            TransportButtons(size: .large)
                .frame(maxWidth: .infinity)
            if client.state.isLibrary || client.state.source == nil {
                ShowControls()
            }
            KaraokeSection()
            VolumeSection()
            LyricPreview()
            UpNextCard()
        }
    }
}

/// Title, artists, and whose turn it is.
struct TrackHeader: View {
    @Environment(StemsClient.self) private var client

    var body: some View {
        let s = client.state
        VStack(alignment: .leading, spacing: 4) {
            if let who = Self.turn(s) {
                Label(who.text, systemImage: who.symbol)
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.tint)
            }
            Text(s.track?.title ?? "Nothing Playing")
                .font(.title2.bold())
                .lineLimit(2)
            if let artists = s.track?.artistLine, !artists.isEmpty {
                Text(artists)
                    .font(.title3)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .accessibilityElement(children: .combine)
    }

    static func turn(_ s: PlaybackState) -> (text: String, symbol: String)? { s.turn }
}

extension PlaybackState {
    /// Whose turn it is, for headers and the menu bar.
    public var turn: (text: String, symbol: String)? {
        if intermission { return ("Background music", "music.note.house") }
        guard let item = current else { return nil }
        switch item.kind {
        case .sing:
            if let singer = item.singer, !singer.isEmpty { return ("\(singer) on the mic", "music.mic") }
            return ("Karaoke", "music.mic")
        case .play:
            return ("Playing for everyone", "music.note")
        }
    }
}

/// Elapsed / remaining with a seek slider.
public struct ProgressSection: View {
    @Environment(StemsClient.self) private var client
    @State private var scrubbing: Double?

    public init() {}

    public var body: some View {
        let duration = Double(client.state.track?.durationMs ?? 0)
        TimelineView(.periodic(from: .now, by: 0.25)) { _ in
            let position = scrubbing ?? Double(min(client.position(), Int(duration)))
            VStack(spacing: 4) {
                Slider(
                    value: Binding(get: { position }, set: { scrubbing = $0 }),
                    in: 0...max(duration, 1)
                ) { editing in
                    if !editing, let target = scrubbing {
                        client.send(.seek(positionMs: Int(target)))
                        // keep showing the target until the clock catches up
                        Task {
                            try? await Task.sleep(for: .milliseconds(400))
                            scrubbing = nil
                        }
                    }
                }
                .disabled(duration == 0 || !client.allowed(.queue))
                .accessibilityLabel("Position")
                .accessibilityValue("\(Format.time(ms: Int(position))) of \(Format.time(ms: Int(duration)))")
                HStack {
                    Text(Format.time(ms: Int(position)))
                    Spacer()
                    Text("-" + Format.time(ms: Int(max(0, duration - position))))
                }
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
            }
        }
    }
}

/// Play/pause and skip. `large` for Now Playing, `small` for bars.
public struct TransportButtons: View {
    public enum Size { case small, large }

    @Environment(StemsClient.self) private var client
    var size: Size

    public init(size: Size) { self.size = size }

    public var body: some View {
        let playing = client.state.status == .playing
        HStack(spacing: size == .large ? 36 : 14) {
            Button {
                client.togglePlayPause()
            } label: {
                Image(systemName: playing ? "pause.fill" : "play.fill")
                    .contentTransition(.symbolEffect(.replace))
                    .frame(width: size == .large ? 44 : 24, height: size == .large ? 44 : 24)
            }
            .accessibilityLabel(playing ? "Pause" : "Play")

            Button {
                client.send(.next)
            } label: {
                Image(systemName: "forward.fill")
                    .frame(width: size == .large ? 44 : 24, height: size == .large ? 44 : 24)
            }
            .accessibilityLabel("Next Song")
        }
        .font(size == .large ? .largeTitle : .title3)
        .buttonStyle(.plain)
        .disabled(!client.allowed(.queue) || client.state.track == nil && client.state.queue.isEmpty)
    }
}

/// Karaoke on/off and how much of the original voice stays in as a guide.
struct KaraokeSection: View {
    @Environment(StemsClient.self) private var client
    @State private var gain: Double?

    var body: some View {
        let k = client.state.karaoke
        GroupBox {
            VStack(alignment: .leading, spacing: 12) {
                Toggle(isOn: Binding(get: { k.enabled }, set: { client.send(.setKaraoke($0)) })) {
                    Label("Karaoke", systemImage: "music.mic")
                }
                .disabled(!client.allowed(.karaoke) || k.backend == nil)

                VStack(alignment: .leading, spacing: 4) {
                    Text("Guide Vocals").font(.subheadline)
                    HStack {
                        Image(systemName: "speaker.slash").foregroundStyle(.secondary)
                        Slider(
                            value: Binding(
                                get: { gain ?? k.vocalGain },
                                set: {
                                    gain = $0
                                    client.sendThrottled(.setVocalGain($0), key: "vocals")
                                }),
                            in: 0...1
                        ) { editing in if !editing { gain = nil } }
                        .accessibilityLabel("Guide vocals")
                        .accessibilityValue(Format.percent(gain ?? k.vocalGain))
                        Image(systemName: "person.wave.2").foregroundStyle(.secondary)
                    }
                }
                .disabled(!k.enabled || !client.allowed(.vocals))

                if let stems = k.stems, !stems.done, stems.totalMs > 0 {
                    ProgressView(value: Double(stems.readyMs), total: Double(stems.totalMs)) {
                        Text("Separating vocals")
                    } currentValueLabel: {
                        Text(stems.speed > 0 ? "\(Format.percent(Double(stems.readyMs) / Double(stems.totalMs))) · \(stems.speed, format: .number.precision(.fractionLength(1)))× real time" : "Starting…")
                    }
                    .font(.caption)
                }
                if k.backend == nil {
                    Text("Vocal removal isn’t available on this server.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }
}

/// The server's output level (the speakers), not this device's volume.
struct VolumeSection: View {
    @Environment(StemsClient.self) private var client
    @State private var level: Double?

    var body: some View {
        HStack {
            Image(systemName: "speaker.fill").foregroundStyle(.secondary)
            Slider(
                value: Binding(
                    get: { level ?? client.state.volume },
                    set: {
                        level = $0
                        client.sendThrottled(.setVolume($0), key: "volume")
                    }),
                in: 0...1
            ) { editing in if !editing { level = nil } }
            .accessibilityLabel("Speaker volume")
            .accessibilityValue(Format.percent(level ?? client.state.volume))
            Image(systemName: "speaker.wave.3.fill").foregroundStyle(.secondary)
        }
        .disabled(!client.allowed(.volume))
    }
}

/// The line being sung and the next one, as the stage shows them.
struct LyricPreview: View {
    @Environment(StemsClient.self) private var client

    var body: some View {
        if let lyrics = client.state.lyrics, lyrics.isSynced {
            TimelineView(.periodic(from: .now, by: 0.2)) { _ in
                let i = lyrics.lineIndex(at: client.position())
                let nextIndex = (i ?? -1) + 1
                VStack(alignment: .leading, spacing: 6) {
                    Text(i.map { lyrics.lines[$0].text } ?? "♪")
                        .font(.headline)
                    if nextIndex < lyrics.lines.count {
                        Text(lyrics.lines[nextIndex].text)
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                    }
                }
                .lineLimit(2)
                .frame(maxWidth: .infinity, alignment: .leading)
                .animation(.default, value: i)
            }
            .accessibilityElement(children: .combine)
            .accessibilityLabel("Lyrics")
        } else if client.state.track != nil {
            Label(client.state.lyrics == nil ? "No synced lyrics" : "Lyrics aren’t timed", systemImage: "text.quote")
                .font(.subheadline)
                .foregroundStyle(.secondary)
        }
    }
}

/// The next request, so the host can call the singer up.
struct UpNextCard: View {
    @Environment(StemsClient.self) private var client

    var body: some View {
        if let next = client.state.queue.first {
            GroupBox("Up Next") {
                QueueRow(item: next)
            }
        }
    }
}
