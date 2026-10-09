// Running the night: the mode, what happens when this song ends, fading
// out for an announcement, talking over the music, the auto-DJ — and the
// big Start button when a singer is staged or the music is held.

import StemsKit
import SwiftUI

extension PartyMode {
    public var title: LocalizedStringKey {
        switch self {
        case .background: "Background"
        case .party: "Party"
        case .karaoke: "Karaoke"
        }
    }

    public var symbol: String {
        switch self {
        case .background: "leaf"
        case .party: "party.popper"
        case .karaoke: "music.mic"
        }
    }

    public var explanation: LocalizedStringKey {
        switch self {
        case .background: "Chill music from your chosen playlists, quieter. Guest requests are off."
        case .party: "Music never stops; guests can request songs to play."
        case .karaoke: "Requests to sing wait on stage until you press Start. Quiet music in between."
        }
    }
}

extension ViewMode {
    public var title: LocalizedStringKey {
        switch self {
        case .lyrics: "Lyrics"
        case .karaoke: "Big Lyrics"
        case .video: "Music Video"
        case .canvas: "Spotify Canvas"
        }
    }

    public var symbol: String {
        switch self {
        case .lyrics: "text.alignleft"
        case .karaoke: "textformat.size.larger"
        case .video: "film"
        case .canvas: "sparkles.tv"
        }
    }
}

extension AfterSong {
    public var title: LocalizedStringKey {
        switch self {
        case .continue: "Keep Playing"
        case .hold: "Stop After This Song"
        case .background: "Quiet Music After This Song"
        }
    }

    public var symbol: String {
        switch self {
        case .continue: "forward.end"
        case .hold: "stop.circle"
        case .background: "speaker.wave.1"
        }
    }
}

/// Shown when the music is held or a singer is waiting: the one action
/// that matters right then.
public struct StageBanner: View {
    @Environment(StemsClient.self) private var client

    public init() {}

    public var body: some View {
        let show = client.state.show
        if let staged = show.staged {
            banner(
                symbol: "music.mic.circle.fill",
                title: "\(staged.singer ?? "Next singer") is up",
                detail: "\(staged.track.title) — \(staged.track.artistLine)",
                action: "Start")
        } else if show.held {
            banner(
                symbol: "pause.circle.fill", title: "Holding",
                detail: client.state.intermission ? "Quiet music is playing until you resume." : "The music is stopped until you resume.",
                action: "Resume")
        }
    }

    private func banner(symbol: String, title: String, detail: String, action: LocalizedStringKey) -> some View {
        HStack(spacing: 12) {
            Image(systemName: symbol)
                .font(.title)
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline)
                Text(detail).font(.subheadline).foregroundStyle(.secondary).lineLimit(2)
            }
            Spacer(minLength: 8)
            Button(action) { client.send(.resume) }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
        }
        .padding(14)
        .background(.tint.opacity(0.12), in: .rect(cornerRadius: 12))
        .accessibilityElement(children: .combine)
    }
}

/// Mode, end-of-song action, fade, talk-over, auto-DJ.
public struct ShowControls: View {
    @Environment(StemsClient.self) private var client
    @State private var choosingPool = false

    public init() {}

    public var body: some View {
        let show = client.state.show
        GroupBox {
            VStack(alignment: .leading, spacing: 12) {
                Picker("Mode", selection: Binding(get: { show.mode }, set: { setMode($0) })) {
                    ForEach(PartyMode.allCases) { m in
                        Label(m.title, systemImage: m.symbol).tag(m)
                    }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                Text(show.mode.explanation)
                    .font(.caption)
                    .foregroundStyle(.secondary)

                HStack(spacing: 8) {
                    Menu {
                        Picker("After This Song", selection: Binding(get: { show.afterSong }, set: { client.send(.setAfterSong($0)) })) {
                            ForEach(AfterSong.allCases) { a in
                                Label(a.title, systemImage: a.symbol).tag(a)
                            }
                        }
                        .pickerStyle(.inline)
                    } label: {
                        Label(show.afterSong.title, systemImage: show.afterSong.symbol)
                    }
                    .fixedSize()
                    .help("What happens when the current song ends (applies once)")

                    Button("Fade Out", systemImage: "speaker.minus") { client.send(.fadeOut(ms: nil)) }
                        .help("Fade to silence over 3 seconds, then hold")
                        .disabled(client.state.status != .playing)

                    TalkOverButton()
                }
                .controlSize(.regular)

                Picker(selection: Binding(get: { client.state.viewMode }, set: { client.send(.setViewMode($0)) })) {
                    ForEach(ViewMode.allCases, id: \.self) { m in
                        Label(m.title, systemImage: m.symbol).tag(m)
                    }
                } label: {
                    Label("Lyrics Screen", systemImage: "tv")
                }
                .help("What the lyrics screen shows")

                HStack {
                    Toggle(isOn: Binding(get: { show.autoDj }, set: { client.send(.setAutoDj($0)) })) {
                        Label(show.mode == .karaoke ? "Music Between Singers" : "Auto-DJ", systemImage: "wand.and.stars")
                    }
                    .disabled(show.mode != .karaoke)
                    .help(show.mode == .karaoke ? "Quiet music while nobody is singing" : "Background and party mode always keep the music going")
                    Spacer()
                    Button {
                        choosingPool = true
                    } label: {
                        Label(poolTitle(show.pool.count), systemImage: "music.note.list")
                    }
                    .help("Which playlists the auto-DJ picks from")
                }
            }
        }
        .sheet(isPresented: $choosingPool) {
            PoolPicker()
                #if os(macOS)
                .frame(minWidth: 380, minHeight: 440)
                #endif
        }
    }

    private func poolTitle(_ n: Int) -> String {
        n == 0 ? "Whole Library" : n == 1 ? "1 Playlist" : "\(n) Playlists"
    }

    /// The mode also decides whether guests can request songs.
    private func setMode(_ m: PartyMode) {
        client.send(.setMode(m))
        Task { try? await client.setSetting("guests.queue", .bool(m != .background)) }
    }
}

/// Ducks the music while held down, like a push-to-talk button.
public struct TalkOverButton: View {
    @Environment(StemsClient.self) private var client
    @State private var pressed = false

    public init() {}

    public var body: some View {
        Label("Talk", systemImage: pressed ? "mic.fill" : "mic")
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .background(pressed ? AnyShapeStyle(.tint) : AnyShapeStyle(.quaternary), in: .capsule)
            .foregroundStyle(pressed ? AnyShapeStyle(.white) : AnyShapeStyle(.primary))
            .contentShape(.capsule)
            .onLongPressGesture(minimumDuration: 600, maximumDistance: 60) {
            } onPressingChanged: { down in
                pressed = down
                client.send(.talkOver(down))
            }
            .help("Hold to duck the music while you talk")
            .accessibilityLabel("Talk over the music")
            .accessibilityHint("Hold to lower the music")
            .accessibilityAddTraits(.isButton)
    }
}

/// Pick the playlists the auto-DJ draws from.
public struct PoolPicker: View {
    @Environment(StemsClient.self) private var client
    @Environment(\.dismiss) private var dismiss
    @State private var playlists: [PlaylistRow] = []
    @State private var chosen: Set<String> = []

    public init() {}

    public var body: some View {
        NavigationStack {
            List {
                Section {
                    Button {
                        chosen.removeAll()
                    } label: {
                        HStack {
                            Text("Whole Library")
                            Spacer()
                            if chosen.isEmpty { Image(systemName: "checkmark").foregroundStyle(.tint) }
                        }
                    }
                    .buttonStyle(.plain)
                } footer: {
                    Text("The auto-DJ plays songs like the one before, from these playlists.")
                }
                Section("Playlists") {
                    ForEach(playlists) { p in
                        Button {
                            if chosen.contains(p.id) { chosen.remove(p.id) } else { chosen.insert(p.id) }
                        } label: {
                            HStack {
                                VStack(alignment: .leading) {
                                    Text(p.displayName)
                                    Text("\(p.playable) songs ready").font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer()
                                if chosen.contains(p.id) { Image(systemName: "checkmark").foregroundStyle(.tint) }
                            }
                            .contentShape(.rect)
                        }
                        .buttonStyle(.plain)
                    }
                }
            }
            .navigationTitle("Auto-DJ Playlists")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel", role: .cancel) { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") {
                        client.send(.setPool(Array(chosen)))
                        dismiss()
                    }
                    .buttonStyle(.borderedProminent)
                }
            }
            .task {
                chosen = Set(client.state.show.pool)
                playlists = (try? await client.api.playlists()) ?? []
            }
        }
    }
}
