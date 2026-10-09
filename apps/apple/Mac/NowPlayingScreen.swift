// Now Playing, as in the host's mockup: the mode and what happens after
// this song in the toolbar; the song centred with its transport, progress
// and Fade Out; a DAW-style mixer bottom right; and the queue on the right
// with the auto-DJ's next picks under the requests.

import StemsKit
import StemsUI
import SwiftUI

struct NowPlayingScreen: View {
    @Environment(StemsClient.self) private var client
    @State private var confirmClear = false

    var body: some View {
        HStack(spacing: 0) {
            PlayerColumn()
                .frame(minWidth: 520, maxWidth: .infinity, maxHeight: .infinity)
                .surface(Surface.content)
            Divider()
            QueueColumn()
                .frame(width: 380)
                .surface(Surface.pane)
        }
        .navigationTitle("Now Playing")
        .toolbar {
            ToolbarItemGroup(placement: .principal) {
                ShowToolbar()
            }
            ToolbarItemGroup(placement: .primaryAction) {
                TransportToolbar(confirmClear: $confirmClear)
            }
            ToolbarItem(placement: .primaryAction) { DownloadsButton() }
        }
        .confirmationDialog(
            "Remove all \(client.state.queue.count) requests?", isPresented: $confirmClear, titleVisibility: .visible
        ) {
            Button("Clear Queue", role: .destructive) { client.send(.clearQueue) }
        } message: {
            Text("Singers will have to request their songs again.")
        }
    }
}

// MARK: Toolbar

/// The mode and what happens when this song ends.
private struct ShowToolbar: View {
    @Environment(StemsClient.self) private var client

    var body: some View {
        let show = client.state.show
        Picker("Mode", selection: Binding(get: { show.mode }, set: setMode)) {
            ForEach(PartyMode.allCases) { m in Text(m.title).tag(m) }
        }
        .pickerStyle(.segmented)
        .help(show.mode.explanation)
        .disabled(!client.state.isLibrary)

        Menu {
            Picker("After This Song", selection: Binding(get: { show.afterSong }, set: { client.send(.setAfterSong($0)) })) {
                ForEach(AfterSong.allCases) { a in Label(a.title, systemImage: a.symbol).tag(a) }
            }
            .pickerStyle(.inline)
        } label: {
            Label(show.afterSong.title, systemImage: "playpause")
                .labelStyle(.titleAndIcon)
        }
        .help("What happens when this song ends (once)")
        .disabled(!client.state.isLibrary)
    }

    /// The mode also decides whether guests can request songs.
    private func setMode(_ m: PartyMode) {
        client.send(.setMode(m))
        Task { try? await client.setSetting("guests.queue", .bool(m != .background)) }
    }
}

private struct TransportToolbar: View {
    @Environment(StemsClient.self) private var client
    @Binding var confirmClear: Bool

    var body: some View {
        let playing = client.state.status == .playing
        Button(playing ? "Pause" : "Play", systemImage: playing ? "pause.fill" : "play.fill") {
            client.togglePlayPause()
        }
        .help("Play or pause")
        Button("Next Song", systemImage: "forward.fill") { client.send(.next) }
            .help("Skip to the next song")
        Toggle(isOn: Binding(get: { client.state.karaoke.enabled }, set: { client.send(.setKaraoke($0)) })) {
            Label("Karaoke", systemImage: "music.mic")
        }
        .help("Karaoke: take the vocals out")
        .disabled(client.state.karaoke.backend == nil)
        Menu {
            Picker("Lyrics Screen", selection: Binding(get: { client.state.viewMode }, set: { client.send(.setViewMode($0)) })) {
                ForEach(ViewMode.allCases, id: \.self) { m in Label(m.title, systemImage: m.symbol).tag(m) }
            }
            .pickerStyle(.inline)
        } label: {
            Label("Lyrics Screen", systemImage: "tv")
        }
        .help("What the lyrics screen shows")
        Button("Clear Queue", systemImage: "trash") { confirmClear = true }
            .help("Remove every request")
            .disabled(client.state.queue.isEmpty)
    }
}

// MARK: Player

private struct PlayerColumn: View {
    @Environment(StemsClient.self) private var client

    var body: some View {
        VStack(spacing: 0) {
            StageBanner()
                .padding(.horizontal, 24)
                .padding(.top, 16)
            Spacer(minLength: 16)
            VStack(spacing: 14) {
                ArtworkView(url: client.url(for: client.state.track?.art), size: 220, cornerRadius: 8)
                    .shadow(color: .black.opacity(0.25), radius: 12, y: 4)
                VStack(spacing: 4) {
                    if let turn = client.state.turn {
                        Label(turn.text, systemImage: turn.symbol)
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.tint)
                    }
                    Text(client.state.track?.title ?? "Nothing Playing")
                        .font(.title2.bold())
                        .multilineTextAlignment(.center)
                        .lineLimit(2)
                    if let a = client.state.track?.artistLine, !a.isEmpty {
                        Text(a).font(.title3).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                .accessibilityElement(children: .combine)
                TransportButtons(size: .large)
                ProgressSection()
                    .frame(maxWidth: 440)
                HStack(spacing: 10) {
                    Button("Fade Out", systemImage: "speaker.minus") { client.send(.fadeOut(ms: nil)) }
                        .help("Fade to silence over 3 seconds, then hold for an announcement")
                        .disabled(client.state.status != .playing)
                    TalkOverButton()
                }
                .disabled(!client.state.isLibrary)
            }
            .padding(.horizontal, 24)
            Spacer(minLength: 16)
            HStack {
                Spacer()
                MixerView()
            }
            .padding(16)
        }
    }
}

// MARK: Mixer

/// A DAW-style mixer: music, guide vocals, the mics, and the line out,
/// each with a fader, a meter and a mute. The mic strips are ready for a
/// DAW connection; until then they only show.
struct MixerView: View {
    @Environment(StemsClient.self) private var client
    /// Mic channel names, comma separated (editable from the mixer's menu).
    @AppStorage("mixerMics") private var micsRaw = "blue,red"
    /// Levels to go back to when unmuting.
    @AppStorage("mixerUnmuteMusic") private var unmuteMusic = 1.0
    @AppStorage("mixerUnmuteVocals") private var unmuteVocals = 0.2
    @AppStorage("mixerUnmuteOut") private var unmuteOut = 0.8
    @State private var naming: String?
    @State private var newName = ""

    private var mics: [String] {
        micsRaw.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
    }

    var body: some View {
        let s = client.state
        let karaoke = s.karaoke.enabled
        HStack(alignment: .bottom, spacing: 18) {
            ChannelStrip(
                name: "music", symbol: nil, meter: \.music,
                value: Binding(get: { s.musicGain }, set: { client.sendThrottled(.setMusicGain($0), key: "music") }),
                muted: s.musicGain == 0,
                mute: { toggle(s.musicGain, restore: &unmuteMusic) { client.send(.setMusicGain($0)) } },
                help: karaoke ? "The backing track" : "The music")
            ChannelStrip(
                name: "vocals", symbol: nil, meter: \.vocals,
                value: Binding(get: { s.karaoke.vocalGain }, set: { client.sendThrottled(.setVocalGain($0), key: "vocals") }),
                muted: s.karaoke.vocalGain == 0,
                mute: { toggle(s.karaoke.vocalGain, restore: &unmuteVocals) { client.send(.setVocalGain($0)) } },
                help: karaoke ? "Guide vocals: how much of the singer stays in" : "Karaoke is off: the vocals are in the music",
                enabled: karaoke)
            ForEach(mics, id: \.self) { m in
                ChannelStrip(
                    name: m, symbol: "music.mic", meter: nil, value: .constant(0.75), muted: false, mute: {},
                    help: "Mic “\(m)”: will be controlled through your DAW once one is connected", enabled: false)
                    .contextMenu {
                        Button("Rename…") {
                            newName = m
                            naming = m
                        }
                        Button("Remove", role: .destructive) { micsRaw = mics.filter { $0 != m }.joined(separator: ",") }
                    }
            }
            Divider().frame(height: 160)
            ChannelStrip(
                name: "line out", symbol: "speaker.wave.2", meter: nil, stereo: (\.outL, \.outR),
                value: Binding(get: { s.volume }, set: { client.sendThrottled(.setVolume($0), key: "volume") }),
                muted: s.volume == 0,
                mute: { toggle(s.volume, restore: &unmuteOut) { client.send(.setVolume($0)) } },
                help: "Everything going to the speakers")
        }
        .padding(14)
        .background(Surface.raised, in: .rect(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(.separator))
        .contextMenu {
            Button("Add Mic Channel…") {
                newName = ""
                naming = ""
            }
        }
        .alert(naming?.isEmpty == false ? "Rename Mic" : "Add Mic Channel", isPresented: Binding(get: { naming != nil }, set: { if !$0 { naming = nil } })) {
            TextField("Name", text: $newName)
            Button("Cancel", role: .cancel) { naming = nil }
            Button("OK") {
                let n = newName.trimmingCharacters(in: .whitespaces).replacingOccurrences(of: ",", with: " ")
                guard !n.isEmpty, let old = naming else { return }
                var list = mics
                if old.isEmpty {
                    if !list.contains(n) { list.append(n) }
                } else if let i = list.firstIndex(of: old) {
                    list[i] = n
                }
                micsRaw = list.joined(separator: ",")
                naming = nil
            }
        }
        .disabled(!client.connection.isConnected)
    }

    /// Mute goes to zero and remembers the level; unmute brings it back.
    private func toggle(_ current: Double, restore: inout Double, send: (Double) -> Void) {
        if current > 0 {
            restore = current
            send(0)
        } else {
            send(restore > 0 ? restore : 0.8)
        }
    }
}

/// One mixer channel: name on top, fader beside its meter, mute below.
struct ChannelStrip: View {
    let name: String
    let symbol: String?
    /// Mono meter source, or `stereo` for two.
    let meter: KeyPath<Levels, Double>?
    var stereo: (KeyPath<Levels, Double>, KeyPath<Levels, Double>)? = nil
    @Binding var value: Double
    let muted: Bool
    let mute: () -> Void
    let help: String
    var enabled = true

    var body: some View {
        VStack(spacing: 8) {
            HStack(spacing: 3) {
                if let symbol { Image(systemName: symbol).imageScale(.small) }
                Text(name).lineLimit(1)
            }
            .font(.caption.weight(.semibold))
            .foregroundStyle(enabled ? .primary : .secondary)
            .frame(minWidth: 44)
            HStack(alignment: .bottom, spacing: 5) {
                VerticalFader(value: $value)
                    .disabled(!enabled)
                if let stereo {
                    LevelMeter(source: stereo.0)
                    LevelMeter(source: stereo.1)
                } else if let meter {
                    LevelMeter(source: meter)
                } else {
                    LevelMeter(source: nil)
                }
            }
            .frame(height: 140)
            Button(action: mute) {
                Image(systemName: muted && enabled ? "speaker.slash.fill" : "speaker.wave.1")
                    .foregroundStyle(muted && enabled ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                    .frame(width: 22, height: 18)
            }
            .buttonStyle(.borderless)
            .disabled(!enabled)
            .help(muted ? "Unmute" : "Mute")
            .accessibilityLabel(muted ? "Unmute \(name)" : "Mute \(name)")
        }
        .opacity(enabled ? 1 : 0.55)
        .help(help)
        .accessibilityElement(children: .contain)
        .accessibilityLabel(name)
    }
}

/// A DAW-style fader: a thin track with a knob, dragged up and down.
struct VerticalFader: View {
    @Binding var value: Double
    @Environment(\.isEnabled) private var isEnabled
    private let knob = CGSize(width: 22, height: 12)

    var body: some View {
        GeometryReader { geo in
            let travel = geo.size.height - knob.height
            let y = CGFloat(1 - min(max(value, 0), 1)) * travel
            ZStack(alignment: .top) {
                Capsule()
                    .fill(.quaternary)
                    .frame(width: 4)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                Capsule()
                    .fill(isEnabled ? AnyShapeStyle(.tint) : AnyShapeStyle(.tertiary))
                    .frame(width: 4, height: max(0, geo.size.height - y - knob.height / 2))
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottom)
                RoundedRectangle(cornerRadius: 3)
                    .fill(Color(nsColor: .controlColor))
                    .overlay(RoundedRectangle(cornerRadius: 3).strokeBorder(.separator))
                    .overlay(Rectangle().fill(.secondary).frame(height: 1))
                    .frame(width: knob.width, height: knob.height)
                    .shadow(color: .black.opacity(0.3), radius: 1, y: 1)
                    .offset(y: y)
            }
            .contentShape(.rect)
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { g in
                        let v = Double(1 - (g.location.y - knob.height / 2) / max(travel, 1))
                        value = (min(max(v, 0), 1) * 100).rounded() / 100
                    }
            )
            .onTapGesture(count: 2) { value = 0.8 }
        }
        .frame(width: knob.width)
        .accessibilityElement()
        .accessibilityLabel("Level")
        .accessibilityValue("\(Int((value * 100).rounded())) percent")
        .accessibilityAdjustableAction { dir in
            switch dir {
            case .increment: value = min(1, value + 0.05)
            case .decrement: value = max(0, value - 0.05)
            @unknown default: break
            }
        }
    }
}

/// A peak meter: green, yellow near the top, red at the top; falls back
/// smoothly between reports.
struct LevelMeter: View {
    @Environment(StemsClient.self) private var client
    let source: KeyPath<Levels, Double>?
    @State private var shown = 0.0

    var body: some View {
        let target = source.map { Self.fraction(client.meters.levels[keyPath: $0]) } ?? 0
        GeometryReader { geo in
            ZStack(alignment: .bottom) {
                Capsule().fill(.quaternary)
                LinearGradient(
                    stops: [
                        .init(color: .green, location: 0),
                        .init(color: .green, location: 0.7),
                        .init(color: .yellow, location: 0.85),
                        .init(color: .red, location: 1),
                    ], startPoint: .bottom, endPoint: .top
                )
                .mask(alignment: .bottom) {
                    Rectangle().frame(height: geo.size.height * CGFloat(shown))
                }
                .clipShape(.capsule)
            }
        }
        .frame(width: 6)
        .onChange(of: target, initial: true) { _, t in
            // jump up, fall back slowly, like a DAW meter
            if t >= shown {
                shown = t
            } else {
                withAnimation(.easeOut(duration: 0.35)) { shown = t }
            }
        }
        .accessibilityHidden(true)
    }

    /// Linear peak → meter height on a 60 dB scale.
    static func fraction(_ peak: Double) -> Double {
        guard peak > 0.000_001 else { return 0 }
        let db = 20 * log10(peak)
        return min(max((db + 60) / 60, 0), 1)
    }
}

// MARK: Queue

private struct QueueColumn: View {
    @Environment(StemsClient.self) private var client
    @State private var choosingPool = false

    var body: some View {
        let s = client.state
        VStack(spacing: 0) {
            AddSongsField()
                .padding(12)
            Divider()
            List {
                Section {
                    ForEach(s.queue) { item in
                        QueueRow(item: item)
                            .contextMenu {
                                Button("Play Now", systemImage: "play.fill") {
                                    client.send(.dequeue(id: item.id))
                                    client.send(.playTrack(key: item.key, singer: item.singer, kind: item.kind))
                                }
                                Button("Move to Top", systemImage: "arrow.up.to.line") { client.send(.moveInQueue(id: item.id, to: 0)) }
                                    .disabled(s.queue.first?.id == item.id)
                                Divider()
                                Button("Remove", systemImage: "minus.circle", role: .destructive) { client.send(.dequeue(id: item.id)) }
                            }
                    }
                    .onMove { from, to in
                        guard let f = from.first else { return }
                        client.send(.moveInQueue(id: s.queue[f].id, to: to > f ? to - 1 : to))
                    }
                    .onDelete { offsets in
                        for i in offsets { client.send(.dequeue(id: s.queue[i].id)) }
                    }
                    if s.queue.isEmpty {
                        Text(s.show.autoDj ? "No requests: the auto-DJ keeps the music going." : "No requests.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                } header: {
                    Text("Up Next")
                }

                if s.isLibrary {
                    Section {
                        if s.show.autoDj {
                            ForEach(s.show.upcoming) { item in
                                QueueRow(item: item)
                                    .opacity(0.8)
                                    .contextMenu {
                                        Button("Play Now", systemImage: "play.fill") {
                                            client.send(.playTrack(key: item.key, singer: nil, kind: .play))
                                        }
                                        Button("Add to Queue", systemImage: "text.badge.plus") {
                                            client.send(.enqueue(key: item.key, singer: nil, next: false, kind: .play))
                                        }
                                    }
                            }
                        } else {
                            Text("Off: the music stops when the queue runs out.")
                                .font(.callout)
                                .foregroundStyle(.secondary)
                        }
                    } header: {
                        AutoDJHeader(choosingPool: $choosingPool)
                    }
                }
            }
            .listStyle(.inset)
            .scrollContentBackground(.hidden)
        }
        .sheet(isPresented: $choosingPool) { PoolPicker().frame(minWidth: 380, minHeight: 440) }
    }
}

private struct AutoDJHeader: View {
    @Environment(StemsClient.self) private var client
    @Binding var choosingPool: Bool

    var body: some View {
        let show = client.state.show
        HStack {
            Text("Auto DJ")
            Spacer()
            Button {
                choosingPool = true
            } label: {
                Label(show.pool.isEmpty ? "Whole Library" : show.pool.count == 1 ? "1 Playlist" : "\(show.pool.count) Playlists", systemImage: "music.note.list")
            }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .help("Which playlists the auto-DJ picks from")
            Toggle("Auto DJ", isOn: Binding(get: { show.autoDj }, set: { client.send(.setAutoDj($0)) }))
                .toggleStyle(.switch)
                .controlSize(.small)
                .labelsHidden()
                .disabled(show.mode == .background)
                .help(show.mode == .background ? "Background mode always keeps the music going" : "Keep music going when the queue is empty")
        }
        .textCase(nil)
    }
}
