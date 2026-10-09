// A song's preview window: the whole song on the Mac's own output (never
// the party speakers), as the mix, the karaoke track or just the vocals,
// with its synced lyrics and music video.

import AVFoundation
import AVKit
import VideoToolbox
import StemsKit
import StemsUI
import SwiftUI

/// Plays one song's clip files through AVAudioEngine pinned to a device,
/// with seeking and switching stems in place.
@MainActor
@Observable
final class SongPlayer {
    enum Phase: Equatable {
        case idle, loading, ready, failed(String)
    }

    private(set) var phase: Phase = .idle
    private(set) var playing = false
    private(set) var durationMs = 0
    private(set) var stem: PreviewStem = .mix
    var device: OutputDevice? {
        didSet { if oldValue != device { rebuild() } }
    }

    private var files: [PreviewStem: AVAudioFile] = [:]
    private var engine: AVAudioEngine?
    private var node: AVAudioPlayerNode?
    /// Where the scheduled segment starts, in frames.
    private var segmentStart: AVAudioFramePosition = 0
    private var pausedAt: AVAudioFramePosition = 0
    /// Bumped by each load; a stale load drops its result.
    private var generation = 0
    /// Bumped whenever playback stops, so a stale "finished" is ignored.
    private var playGen = 0
    private let key: String

    init(key: String) {
        self.key = key
        device = OutputDevice.preferred()
    }

    private var file: AVAudioFile? { files[stem] }
    private var rate: Double { file?.processingFormat.sampleRate ?? 44_100 }

    /// Playback position now.
    var positionMs: Int {
        Int(Double(currentFrame) / rate * 1000)
    }

    private var currentFrame: AVAudioFramePosition {
        guard playing, let node, let t = node.lastRenderTime, let pt = node.playerTime(forNodeTime: t) else {
            return pausedAt
        }
        return min(segmentStart + pt.sampleTime, file?.length ?? 0)
    }

    func select(_ stem: PreviewStem, client: StemsClient) {
        let at = currentFrame
        let wasPlaying = playing  // never starts by itself: press Play
        self.stem = stem
        stopNode()
        pausedAt = at
        if files[stem] != nil {
            if wasPlaying && phase != .idle { start(from: at) }
            return
        }
        generation += 1
        let mine = generation
        phase = .loading
        Task {
            do {
                let data = try await client.api.preview(key, stem: stem, startMs: 0, secs: 1200)
                guard mine == generation else { return }
                let url = FileManager.default.temporaryDirectory
                    .appending(path: "stems-preview-\(key)-\(stem.rawValue).wav")
                try data.write(to: url)
                let f = try AVAudioFile(forReading: url)
                files[stem] = f
                durationMs = Int(Double(f.length) / f.processingFormat.sampleRate * 1000)
                phase = .ready
                if wasPlaying { start(from: pausedAt) }
            } catch {
                if mine == generation { phase = .failed(error.localizedDescription) }
            }
        }
    }

    func toggle() {
        if playing {
            pausedAt = currentFrame
            stopNode()
        } else if phase == .ready {
            start(from: pausedAt >= (file?.length ?? 0) ? 0 : pausedAt)
        }
    }

    func seek(ms: Int) {
        let frame = AVAudioFramePosition(Double(max(0, ms)) / 1000 * rate)
        let was = playing
        stopNode()
        pausedAt = min(frame, file?.length ?? 0)
        if was { start(from: pausedAt) }
    }

    func stop() {
        generation += 1
        stopNode()
        engine?.stop()
        engine = nil
        node = nil
    }

    private func stopNode() {
        playGen += 1
        node?.stop()
        playing = false
    }

    private func rebuild() {
        let at = currentFrame
        let was = playing
        stopNode()
        engine?.stop()
        engine = nil
        node = nil
        pausedAt = at
        if was { start(from: at) }
    }

    private func ensureEngine(format: AVAudioFormat) throws -> AVAudioPlayerNode {
        if let node, let engine, engine.isRunning { return node }
        let engine = AVAudioEngine()
        let node = AVAudioPlayerNode()
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        // pinned to the preview device so it never reaches the party
        if let device, let unit = engine.outputNode.audioUnit {
            var id = device.id
            AudioUnitSetProperty(
                unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &id,
                UInt32(MemoryLayout<AudioDeviceID>.size))
        }
        try engine.start()
        self.engine = engine
        self.node = node
        return node
    }

    private func start(from frame: AVAudioFramePosition) {
        guard let file else { return }
        do {
            let node = try ensureEngine(format: file.processingFormat)
            let count = AVAudioFrameCount(max(0, file.length - frame))
            guard count > 0 else { return }
            playGen += 1
            let mine = playGen
            segmentStart = frame
            node.scheduleSegment(file, startingFrame: frame, frameCount: count, at: nil) { [weak self] in
                Task { @MainActor in
                    guard let self, self.playGen == mine else { return }
                    self.playing = false
                    self.pausedAt = file.length
                }
            }
            node.play()
            playing = true
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }
}

struct SongPreviewWindow: View {
    @Environment(StemsClient.self) private var client
    let key: String
    @State private var detail: SongDetail?
    @State private var player: SongPlayer
    @Environment(AppModel.self) private var app
    @State private var video: AVPlayer?
    /// Why the video can't play here, when it can't.
    @State private var videoProblem: String?
    /// AV1 without a hardware decoder: decoded in software from this file.
    @State private var av1File: URL?
    @State private var scrub: Double?
    @AppStorage("previewShowVideo") private var showVideo = true

    init(key: String) {
        self.key = key
        _player = State(initialValue: SongPlayer(key: key))
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            HSplitView {
                if showVideo, let video {
                    Group {
                        if let av1File, let d = detail {
                            AV1PlayerView(url: av1File) { [player] in
                                Double(player.positionMs + d.videoOffsetMs) / 1000
                            }
                        } else {
                            PlayerView(player: video)
                        }
                    }
                        .overlay {
                            if let videoProblem {
                                ContentUnavailableView("Video Can’t Play Here", systemImage: "film", description: Text(videoProblem))
                                    .background(.background)
                            }
                        }
                        .frame(minWidth: 320)
                }
                LyricsPane(lyrics: detail?.lyrics, plain: detail?.lyricsText, player: player)
                    .frame(minWidth: 280)
            }
            Divider()
            transport
        }
        .navigationTitle(detail.map { "\($0.track.title) — \($0.track.artistLine)" } ?? "Preview")
        .task(id: client.connection.isConnected) {
            // the window can open before the app has connected
            guard client.connection.isConnected, detail == nil else { return }
            detail = try? await client.api.song(key)
            if detail?.track.stages?.video == .done, let url = client.url(for: "/media/library/\(key)/video.mp4") {
                let p = AVPlayer(url: url)
                p.isMuted = true
                video = p
                if await Self.needsSoftwareAV1(url) {
                    av1File = await localVideo(remote: url)
                    if av1File == nil { videoProblem = "Couldn’t load the video." }
                }
                syncVideo(force: true)  // show the first frame, not a placeholder
            }
            if player.phase != .ready { player.select(player.stem, client: client) }
        }
        .task {
            // keep the video on the audio's clock
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(250))
                syncVideo()
            }
        }
        .onDisappear {
            player.stop()
            video?.pause()
        }
    }

    private var header: some View {
        HStack(spacing: 12) {
            ArtworkView(url: client.url(for: detail?.track.art), size: 44, cornerRadius: 6)
            VStack(alignment: .leading, spacing: 2) {
                Text(detail?.track.title ?? " ").font(.headline).lineLimit(1)
                Text(detail?.track.artistLine ?? " ").foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer()
            Picker("Listen to", selection: Binding(get: { player.stem }, set: { player.select($0, client: client) })) {
                Text("Song").tag(PreviewStem.mix)
                Text("Karaoke").tag(PreviewStem.instrumental)
                Text("Vocals").tag(PreviewStem.vocals)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            .disabled(detail?.track.playable == false)
            .help("The whole song, the karaoke track (no vocals), or the vocals alone")
            Picker("Output", selection: $player.device) {
                ForEach(OutputDevice.all()) { d in Text(d.name).tag(Optional(d)) }
            }
            .frame(maxWidth: 220)
            .help("Previews play here, never on the party speakers")
            Toggle("Video", systemImage: "film", isOn: $showVideo)
                .toggleStyle(.button)
                .disabled(video == nil)
                .help(video == nil ? "No music video for this song" : "Show the music video")
        }
        .padding(12)
    }

    private var transport: some View {
        TimelineView(.periodic(from: .now, by: 0.25)) { _ in
            let pos = player.positionMs
            HStack(spacing: 12) {
                Button {
                    player.toggle()
                    syncVideo(force: true)
                } label: {
                    Image(systemName: player.playing ? "pause.fill" : "play.fill")
                        .frame(width: 20)
                }
                .controlSize(.large)
                .disabled(player.phase != .ready)
                .help(player.playing ? "Pause" : "Play")
                Text(Fmt.time(ms: Int(scrub ?? Double(pos)))).monospacedDigit().foregroundStyle(.secondary)
                Slider(
                    value: Binding(get: { scrub ?? Double(pos) }, set: { scrub = $0 }),
                    in: 0...Double(max(player.durationMs, 1))
                ) { editing in
                    if !editing, let s = scrub {
                        player.seek(ms: Int(s))
                        scrub = nil
                        syncVideo(force: true)
                    }
                }
                .disabled(player.phase != .ready)
                Text(Fmt.time(ms: player.durationMs)).monospacedDigit().foregroundStyle(.secondary)
                switch player.phase {
                case .loading: ProgressView().controlSize(.small)
                case .failed(let e): Label(e, systemImage: "exclamationmark.triangle").foregroundStyle(.orange).lineLimit(1)
                default: EmptyView()
                }
            }
            .padding(12)
        }
    }

    /// AV1 on a Mac without its hardware decoder (before M3): Apple's
    /// player can't show it, so it goes through dav1d instead.
    private static func needsSoftwareAV1(_ url: URL) async -> Bool {
        let asset = AVURLAsset(url: url)
        guard let track = try? await asset.loadTracks(withMediaType: .video).first,
            let format = try? await track.load(.formatDescriptions).first
        else { return false }
        return CMFormatDescriptionGetMediaSubType(format) == kCMVideoCodecType_AV1
            && !VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1)
    }

    /// The video as a local file: straight from the library when it's on
    /// this Mac, else downloaded once to a temporary file.
    private func localVideo(remote: URL) async -> URL? {
        if let dir = app.supervisor.config?.libraryDir {
            let f = URL(fileURLWithPath: dir).appending(path: "tracks/\(key)/video.mp4")
            if FileManager.default.isReadableFile(atPath: f.path) { return f }
        }
        let tmp = FileManager.default.temporaryDirectory.appending(path: "stems-video-\(key).mp4")
        if FileManager.default.fileExists(atPath: tmp.path) { return tmp }
        guard let (file, response) = try? await URLSession.shared.download(from: remote),
            (response as? HTTPURLResponse)?.statusCode == 200
        else { return nil }
        try? FileManager.default.removeItem(at: tmp)
        return (try? FileManager.default.moveItem(at: file, to: tmp)) != nil ? tmp : nil
    }

    private func syncVideo(force: Bool = false) {
        guard av1File == nil, let video, let detail else { return }
        let want = Double(player.positionMs + detail.videoOffsetMs) / 1000
        if player.playing {
            if video.rate == 0 { video.play() }
        } else if video.rate != 0 {
            video.pause()
        }
        let drift = abs(video.currentTime().seconds - want)
        if force || drift > 0.3 {
            video.seek(to: CMTime(seconds: max(0, want), preferredTimescale: 600), toleranceBefore: .zero, toleranceAfter: .zero)
        }
    }
}

/// The music video, driven by the audio (no controls of its own). AVKit's
/// SwiftUI `VideoPlayer` crashes on load on this macOS, so this wraps the
/// AppKit view.
struct PlayerView: NSViewRepresentable {
    let player: AVPlayer

    func makeNSView(context: Context) -> AVPlayerView {
        let v = AVPlayerView()
        v.controlsStyle = .none
        v.videoGravity = .resizeAspect
        v.player = player
        return v
    }

    func updateNSView(_ v: AVPlayerView, context: Context) {
        if v.player !== player { v.player = player }
    }
}

/// Synced lyrics that follow the preview, the current line highlighted.
struct LyricsPane: View {
    let lyrics: Lyrics?
    let plain: String?
    let player: SongPlayer

    var body: some View {
        if let lyrics, lyrics.isSynced {
            TimelineView(.periodic(from: .now, by: 0.2)) { _ in
                let t = player.positionMs - lyrics.offsetMs
                let current = lyrics.lines.lastIndex { $0.timeMs <= t }
                ScrollViewReader { proxy in
                    ScrollView {
                        VStack(alignment: .leading, spacing: 10) {
                            ForEach(Array(lyrics.lines.enumerated()), id: \.offset) { i, line in
                                Text(line.text.isEmpty ? "♪" : line.text)
                                    .font(.title3.weight(i == current ? .bold : .regular))
                                    .foregroundStyle(i == current ? AnyShapeStyle(.primary) : AnyShapeStyle(.tertiary))
                                    .id(i)
                                    .onTapGesture { player.seek(ms: line.timeMs + lyrics.offsetMs) }
                            }
                        }
                        .padding(20)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .onChange(of: current) { _, c in
                        guard let c else { return }
                        withAnimation(.easeInOut(duration: 0.3)) { proxy.scrollTo(c, anchor: .center) }
                    }
                }
            }
        } else if let plain, !plain.isEmpty {
            ScrollView {
                Text(plain).font(.title3).padding(20).frame(maxWidth: .infinity, alignment: .leading).textSelection(.enabled)
            }
        } else {
            ContentUnavailableView("No Lyrics", systemImage: "text.alignleft")
        }
    }
}
