// The song page: everything about the selected song, a preview on the
// Mac's own speakers, and the request actions.

import StemsKit
import StemsUI
import SwiftUI

struct SongInspector: View {
    @Environment(StemsClient.self) private var client
    @State private var detail: SongDetail?
    @State private var error: String?
    @State private var requesting: RequestTarget?
    @State private var stem: PreviewStem = .mix
    private var preview: PreviewPlayer { PreviewPlayer.shared }
    let key: String?

    var body: some View {
        Group {
            if let d = detail, d.track.key == key {
                content(d)
            } else if let error {
                ContentUnavailableView("Can’t Load Song", systemImage: "exclamationmark.triangle", description: Text(error))
            } else if key == nil {
                ContentUnavailableView("No Song Selected", systemImage: "music.note", description: Text("Select a song to see its details."))
            } else {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .task(id: key) {
            guard let key else { return }
            do {
                detail = try await client.api.song(key)
                error = nil
            } catch {
                self.error = error.localizedDescription
            }
        }
        .sheet(item: $requesting) { RequestSheet(track: $0).frame(minWidth: 420, minHeight: 380) }
    }

    private func content(_ d: SongDetail) -> some View {
        let t = d.track
        return ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                ArtworkView(url: client.url(for: t.art), cornerRadius: 10)
                    .frame(maxWidth: .infinity)
                VStack(alignment: .leading, spacing: 3) {
                    Text(t.title).font(.title3.bold()).textSelection(.enabled)
                    Text(t.artistLine).foregroundStyle(.secondary)
                    Text([t.album, d.info.releaseDate.map { String($0.prefix(4)) }, d.info.label].compactMap { $0 }.joined(separator: " · "))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                // actions
                HStack {
                    Button("Sing…", systemImage: "music.mic") { requesting = RequestTarget(t) }
                        .disabled(!t.playable)
                    Button("Add", systemImage: "plus") {
                        client.send(.enqueue(key: t.key, singer: nil, next: false, kind: .play))
                    }
                    .disabled(!t.playable)
                    Button("Next", systemImage: "text.line.first.and.arrowtriangle.forward") {
                        client.send(.enqueue(key: t.key, singer: nil, next: true, kind: .play))
                    }
                    .disabled(!t.playable)
                }
                .controlSize(.small)

                previewBox(t)

                if let a = d.analysis { analysis(a) }

                if !d.playlists.isEmpty {
                    section("In Playlists") {
                        ForEach(d.playlists) { p in
                            Label(p.displayName, systemImage: "music.note.list").font(.callout)
                        }
                    }
                }

                section("Plays") {
                    if d.plays.isEmpty {
                        Text("Never played").font(.callout).foregroundStyle(.secondary)
                    } else {
                        ForEach(Array(d.plays.prefix(8).enumerated()), id: \.offset) { _, p in
                            HStack {
                                Image(systemName: p.kind == "sing" ? "music.mic" : "music.note").foregroundStyle(.secondary)
                                Text(p.singer ?? (p.background ? "Background" : "Played"))
                                Spacer()
                                Text(Fmt.ago(unix: p.at)).foregroundStyle(.secondary)
                            }
                            .font(.callout)
                        }
                    }
                }

                if let stages = t.stages {
                    section("Import") {
                        ForEach(stages.all, id: \.name) { s in
                            HStack {
                                Text(s.name)
                                Spacer()
                                stageLabel(s.state)
                            }
                            .font(.callout)
                        }
                        if let m = t.stemsModel {
                            Text("Stems: \(m)").font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                        }
                    }
                }

                if let lyrics = d.lyricsText, !lyrics.isEmpty {
                    section("Lyrics") {
                        Text(lyrics).font(.callout).textSelection(.enabled)
                    }
                }
            }
            .padding(16)
        }
    }

    private func previewBox(_ t: LibraryTrack) -> some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 8) {
                Picker("Preview", selection: $stem) {
                    Text("Mix").tag(PreviewStem.mix)
                    Text("Instrumental").tag(PreviewStem.instrumental)
                    Text("Vocals").tag(PreviewStem.vocals)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .disabled(!t.playable)
                HStack {
                    let mine = preview.playingKey == t.key
                    Button {
                        preview.toggle(t.key, stem: stem, client: client)
                    } label: {
                        Label(mine ? "Stop" : "Preview", systemImage: mine ? "stop.fill" : "headphones")
                    }
                    if case .loading(let k) = preview.state, k == t.key { ProgressView().controlSize(.small) }
                    Spacer()
                    Text("On this Mac: \(OutputDevice.preferred()?.name ?? "default output")")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                if case .failed(let e) = preview.state {
                    Text(e).font(.caption).foregroundStyle(.orange)
                }
            }
        }
        .onChange(of: stem) {
            if preview.playingKey == t.key { preview.play(t.key, stem: stem, client: client) }
        }
        .onDisappear { preview.stop() }
    }

    private func analysis(_ a: Analysis) -> some View {
        section("Sound") {
            HStack(spacing: 16) {
                if let bpm = a.bpm { stat("BPM", String(Int(bpm.rounded()))) }
                if let k = a.key { stat("Key", k) }
                if let v = a.vocals { stat("Vocals", "\(Int((v * 100).rounded()))%") }
            }
            if let d = a.danceability { meter("Danceable", d) }
            ForEach(a.moods.sorted { $0.value > $1.value }.prefix(5), id: \.key) { m in
                meter(m.key.capitalized, m.value)
            }
            if !a.genres.isEmpty {
                Text(a.genres.prefix(3).map(\.name).joined(separator: "\n"))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func stat(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(value).font(.headline).monospacedDigit()
            Text(label).font(.caption2).foregroundStyle(.secondary)
        }
    }

    private func meter(_ label: String, _ v: Double) -> some View {
        HStack {
            Text(label).font(.callout).frame(width: 90, alignment: .leading)
            Gauge(value: v) { EmptyView() }
                .gaugeStyle(.accessoryLinearCapacity)
            Text("\(Int((v * 100).rounded()))").font(.caption).monospacedDigit().foregroundStyle(.secondary)
                .frame(width: 26, alignment: .trailing)
        }
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder
    private func stageLabel(_ s: StageState) -> some View {
        switch s {
        case .done: Label("Done", systemImage: "checkmark.circle.fill").foregroundStyle(.green).labelStyle(.titleAndIcon)
        case .pending: Text("Pending").foregroundStyle(.secondary)
        case .missing: Text("Not found").foregroundStyle(.secondary)
        case .skipped: Text("Skipped").foregroundStyle(.secondary)
        case .failed(let e): Label("Failed", systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange).help(e)
        case .unknown(let x): Text(x)
        }
    }

    private func section<Content: View>(_ title: LocalizedStringKey, @ViewBuilder _ content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.subheadline.weight(.semibold)).foregroundStyle(.secondary)
            content()
        }
    }
}
