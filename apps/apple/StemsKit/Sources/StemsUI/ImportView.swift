// Add songs: paste Spotify links (tracks, albums, playlists) or file paths
// on the server, then watch them get downloaded, separated and synced.

import StemsKit
import SwiftUI

public struct ImportView: View {
    @Environment(StemsClient.self) private var client
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""
    @State private var status = ImportStatus()
    @State private var sending = false
    @State private var error: String?

    public init() {}

    private var sources: [String] {
        text.split(whereSeparator: \.isNewline).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
    }

    public var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextEditor(text: $text)
                        .font(.body.monospaced())
                        .frame(minHeight: 110)
                        .autocorrectionDisabled()
                        #if os(iOS)
                        .textInputAutocapitalization(.never)
                        #endif
                        .overlay(alignment: .topLeading) {
                            if text.isEmpty {
                                Text("https://open.spotify.com/playlist/…")
                                    .foregroundStyle(.tertiary)
                                    .padding(.top, 8)
                                    .padding(.leading, 5)
                                    .allowsHitTesting(false)
                            }
                        }
                } header: {
                    Text("Links or Files")
                } footer: {
                    Text("One per line: Spotify track, album or playlist links, or audio files and folders on the server. Each song takes about a minute with a worker connected.")
                }
                if let error {
                    Section {
                        Label(error, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    }
                }
                progress
            }
            .formStyle(.grouped)
            .navigationTitle("Import Songs")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Import") { Task { await submit() } }
                        .buttonStyle(.borderedProminent)
                        .disabled(sources.isEmpty || sending)
                }
            }
            .task {
                while !Task.isCancelled {
                    if let s = try? await client.api.importStatus() { status = s }
                    try? await Task.sleep(for: .seconds(status.running ? 1.5 : 4))
                }
            }
        }
    }

    @ViewBuilder
    private var progress: some View {
        if let current = status.current {
            Section("Importing") {
                HStack(spacing: 10) {
                    ProgressView().controlSize(.small)
                    Text(current).lineLimit(2)
                }
                ForEach(Array(status.waiting.enumerated()), id: \.offset) { _, title in
                    Label(title, systemImage: "clock").foregroundStyle(.secondary).lineLimit(1)
                }
            }
        }
        if !status.finished.isEmpty {
            Section("Recently Added") {
                ForEach(status.finished.prefix(20)) { f in
                    VStack(alignment: .leading, spacing: 4) {
                        StatusLabel(
                            text: f.title,
                            level: !f.playable ? .problem : f.complete ? .ok : .warning)
                        ForEach(f.problems, id: \.self) { p in
                            Text(p).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
            }
        }
        if !status.errors.isEmpty {
            Section("Problems") {
                ForEach(Array(status.errors.prefix(10).enumerated()), id: \.offset) { _, e in
                    StatusLabel(text: e, level: .problem)
                }
            }
        }
    }

    private func submit() async {
        sending = true
        defer { sending = false }
        do {
            try await client.api.importSources(sources)
            text = ""
            error = nil
            status = (try? await client.api.importStatus()) ?? status
        } catch {
            self.error = error.localizedDescription
        }
    }
}
