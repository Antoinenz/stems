// The app's main areas, shared by the Mac sidebar and the iOS tab bar.

import StemsKit
import SwiftUI

public enum Screen: String, CaseIterable, Identifiable, Hashable, Sendable {
    case nowPlaying, queue, library, machines, guests

    public var id: String { rawValue }

    public var title: LocalizedStringKey {
        switch self {
        case .nowPlaying: "Now Playing"
        case .queue: "Queue"
        case .library: "Library"
        case .machines: "Machines"
        case .guests: "Guests"
        }
    }

    /// Short label for a tab bar.
    public var tabTitle: LocalizedStringKey {
        switch self {
        case .nowPlaying: "Playing"
        default: title
        }
    }

    public var symbol: String {
        switch self {
        case .nowPlaying: "play.circle"
        case .queue: "list.bullet"
        case .library: "music.note.list"
        case .machines: "server.rack"
        case .guests: "person.2"
        }
    }

    /// Keyboard shortcut (⌘1…) for View menu items.
    public var shortcut: KeyEquivalent {
        KeyEquivalent(Character(String((Screen.allCases.firstIndex(of: self) ?? 0) + 1)))
    }
}

/// A compact now-playing strip: artwork, title, play/pause, skip.
public struct MiniPlayer: View {
    @Environment(StemsClient.self) private var client

    public init() {}

    public var body: some View {
        let s = client.state
        HStack(spacing: 10) {
            ArtworkView(url: client.url(for: s.track?.art), size: 32, cornerRadius: 5)
            VStack(alignment: .leading, spacing: 0) {
                Text(s.track?.title ?? "Nothing Playing").font(.subheadline.weight(.semibold)).lineLimit(1)
                if let turn = TrackHeader.turn(s) {
                    Text(turn.text).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                } else if let artists = s.track?.artistLine {
                    Text(artists).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                }
            }
            Spacer(minLength: 4)
            if s.karaoke.enabled {
                Image(systemName: "music.mic")
                    .foregroundStyle(.tint)
                    .accessibilityLabel("Karaoke on")
            }
            TransportButtons(size: .small)
        }
        .padding(.horizontal, 14)
    }
}
