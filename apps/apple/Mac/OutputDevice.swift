// Previewing songs on the Mac's own speakers or headphones — never the
// party output. Clips come from the server as WAV (mix, instrumental or
// vocals) and play through AVAudioEngine pinned to a chosen output device.

import AVFoundation
import CoreAudio
import Observation
import StemsKit

/// An audio output device on this Mac.
struct OutputDevice: Hashable, Identifiable {
    let id: AudioDeviceID
    let uid: String
    let name: String
    let builtIn: Bool

    /// Every device with output channels.
    static func all() -> [OutputDevice] {
        var size: UInt32 = 0
        var addr = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDevices, mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        guard AudioObjectGetPropertyDataSize(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size) == noErr else {
            return []
        }
        var ids = [AudioDeviceID](repeating: 0, count: Int(size) / MemoryLayout<AudioDeviceID>.size)
        guard AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &addr, 0, nil, &size, &ids) == noErr else {
            return []
        }
        return ids.compactMap { id in
            guard outputChannels(id) > 0, let uid = string(id, kAudioDevicePropertyDeviceUID),
                let name = string(id, kAudioObjectPropertyName)
            else { return nil }
            return OutputDevice(id: id, uid: uid, name: name, builtIn: transport(id) == kAudioDeviceTransportTypeBuiltIn)
        }
    }

    /// The saved choice, else the built-in output (speakers or headphones).
    static func preferred() -> OutputDevice? {
        let all = all()
        let saved = UserDefaults.standard.string(forKey: "previewDeviceUID")
        return all.first { $0.uid == saved } ?? all.first(where: \.builtIn)
    }

    private static func outputChannels(_ id: AudioDeviceID) -> Int {
        var addr = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreamConfiguration, mScope: kAudioDevicePropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(id, &addr, 0, nil, &size) == noErr, size > 0 else { return 0 }
        let raw = UnsafeMutableRawPointer.allocate(byteCount: Int(size), alignment: MemoryLayout<AudioBufferList>.alignment)
        defer { raw.deallocate() }
        guard AudioObjectGetPropertyData(id, &addr, 0, nil, &size, raw) == noErr else { return 0 }
        let list = UnsafeMutableAudioBufferListPointer(raw.assumingMemoryBound(to: AudioBufferList.self))
        return list.reduce(0) { $0 + Int($1.mNumberChannels) }
    }

    private static func string(_ id: AudioDeviceID, _ selector: AudioObjectPropertySelector) -> String? {
        var addr = AudioObjectPropertyAddress(
            mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
        var value: Unmanaged<CFString>?
        var size = UInt32(MemoryLayout<Unmanaged<CFString>?>.size)
        guard AudioObjectGetPropertyData(id, &addr, 0, nil, &size, &value) == noErr, let v = value else { return nil }
        return v.takeRetainedValue() as String
    }

    private static func transport(_ id: AudioDeviceID) -> UInt32 {
        var addr = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyTransportType, mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var value: UInt32 = 0
        var size = UInt32(MemoryLayout<UInt32>.size)
        AudioObjectGetPropertyData(id, &addr, 0, nil, &size, &value)
        return value
    }
}

@MainActor
@Observable
final class PreviewPlayer {
    static let shared = PreviewPlayer()

    enum State: Equatable {
        case idle
        case loading(String)
        case playing(String)
        case failed(String)
    }

    private(set) var state: State = .idle
    private(set) var stem: PreviewStem = .mix
    private var engine: AVAudioEngine?
    private var node: AVAudioPlayerNode?
    private var generation = 0

    var playingKey: String? {
        switch state {
        case .loading(let k), .playing(let k): k
        default: nil
        }
    }

    func toggle(_ key: String, stem: PreviewStem, client: StemsClient) {
        if playingKey == key && self.stem == stem {
            stop()
        } else {
            play(key, stem: stem, client: client)
        }
    }

    func play(_ key: String, stem: PreviewStem, client: StemsClient) {
        stop()
        generation += 1
        let mine = generation
        self.stem = stem
        state = .loading(key)
        Task {
            do {
                let data = try await client.api.preview(key, stem: stem)
                guard mine == generation else { return }
                try start(data, key: key)
            } catch {
                if mine == generation { state = .failed(error.localizedDescription) }
            }
        }
    }

    func stop() {
        generation += 1
        node?.stop()
        engine?.stop()
        engine = nil
        node = nil
        state = .idle
    }

    private func start(_ wav: Data, key: String) throws {
        let url = FileManager.default.temporaryDirectory.appending(path: "stems-preview.wav")
        try wav.write(to: url)
        let file = try AVAudioFile(forReading: url)
        let engine = AVAudioEngine()
        let node = AVAudioPlayerNode()
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: file.processingFormat)
        // pin the output to the preview device so it never reaches the party
        if let device = OutputDevice.preferred(), let unit = engine.outputNode.audioUnit {
            var id = device.id
            AudioUnitSetProperty(
                unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &id,
                UInt32(MemoryLayout<AudioDeviceID>.size))
        }
        try engine.start()
        let mine = generation
        node.scheduleFile(file, at: nil) { [weak self] in
            Task { @MainActor in
                if let self, self.generation == mine { self.stop() }
            }
        }
        node.play()
        self.engine = engine
        self.node = node
        state = .playing(key)
    }
}
