// Playing AV1 music videos on Macs without a hardware AV1 decoder (all
// before M3), where AVFoundation can't decode them. AVAssetReader still
// demuxes the mp4 (compressed samples), dav1d (VideoLAN's decoder, built
// by scripts/build-dav1d.sh) turns them into frames, and an
// AVSampleBufferDisplayLayer shows the frame for the time asked for, so
// the video follows the preview's audio clock.

import AVFoundation
import CDav1d
import CoreMedia
import CoreVideo
import SwiftUI

/// One decoded frame and when it is shown, in seconds.
struct VideoFrame: @unchecked Sendable {
    let time: Double
    let buffer: CVPixelBuffer
}

/// dav1d behind a small, synchronous interface. Not thread-safe: one
/// queue at a time.
final class AV1Decoder {
    private var ctx: OpaquePointer?
    private var formats: [CMVideoFormatDescription] = []

    init() throws {
        var s = Dav1dSettings()
        dav1d_default_settings(&s)
        s.n_threads = 0  // every core
        s.max_frame_delay = 1  // a frame out for each frame in: seeks stay quick
        var c: OpaquePointer?
        guard dav1d_open(&c, &s) == 0, let c else { throw AV1Error.open }
        ctx = c
    }

    deinit { dav1d_close(&ctx) }

    /// Forget everything in flight (before decoding from a new position).
    func flush() { dav1d_flush(ctx) }

    /// Feed raw OBUs (the av1C config, or one sample) stamped with `time`
    /// seconds; returns the frames that came out.
    func decode(_ bytes: UnsafeRawBufferPointer, time: Double) -> [VideoFrame] {
        var out: [VideoFrame] = []
        guard bytes.count > 0, let base = bytes.baseAddress else { return out }
        var data = Dav1dData()
        guard let dst = dav1d_data_create(&data, bytes.count) else { return out }
        dst.update(from: base.assumingMemoryBound(to: UInt8.self), count: bytes.count)
        data.m.timestamp = Int64((time * 1_000_000).rounded())
        while data.sz > 0 {
            let r = dav1d_send_data(ctx, &data)
            if r < 0 && r != -EAGAIN {
                dav1d_data_unref(&data)
                break
            }
            drain(into: &out)
            if r == 0 && data.sz == 0 { break }
        }
        drain(into: &out)
        return out
    }

    private func drain(into out: inout [VideoFrame]) {
        while true {
            var pic = Dav1dPicture()
            guard dav1d_get_picture(ctx, &pic) == 0 else { return }
            if let buf = Self.pixelBuffer(&pic) {
                out.append(VideoFrame(time: Double(pic.m.timestamp) / 1_000_000, buffer: buf))
            }
            dav1d_picture_unref(&pic)
        }
    }

    /// I420 (8 or 10 bit) → an NV12 video-range pixel buffer.
    private static func pixelBuffer(_ pic: inout Dav1dPicture) -> CVPixelBuffer? {
        let w = Int(pic.p.w), h = Int(pic.p.h)
        guard w > 0, h > 0, pic.p.layout == DAV1D_PIXEL_LAYOUT_I420,
            let ys = pic.data.0, let us = pic.data.1, let vs = pic.data.2
        else { return nil }
        let attrs: [String: Any] = [
            kCVPixelBufferIOSurfacePropertiesKey as String: [:] as [String: Any],
            kCVPixelBufferMetalCompatibilityKey as String: true,
        ]
        var pb: CVPixelBuffer?
        guard CVPixelBufferCreate(nil, w, h, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, attrs as CFDictionary, &pb) == kCVReturnSuccess,
            let pb
        else { return nil }
        CVPixelBufferLockBaseAddress(pb, [])
        defer { CVPixelBufferUnlockBaseAddress(pb, []) }
        let high = pic.p.bpc > 8
        let yStride = pic.stride.0, cStride = pic.stride.1
        // luma
        let yDst = CVPixelBufferGetBaseAddressOfPlane(pb, 0)!.assumingMemoryBound(to: UInt8.self)
        let yDstStride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0)
        for row in 0..<h {
            let src = ys.advanced(by: row * yStride)
            let dst = yDst.advanced(by: row * yDstStride)
            if high {
                let s16 = src.assumingMemoryBound(to: UInt16.self)
                for x in 0..<w { dst[x] = UInt8(truncatingIfNeeded: s16[x] >> 2) }
            } else {
                dst.update(from: src.assumingMemoryBound(to: UInt8.self), count: w)
            }
        }
        // chroma, interleaved Cb Cr
        let cw = (w + 1) / 2, ch = (h + 1) / 2
        let cDst = CVPixelBufferGetBaseAddressOfPlane(pb, 1)!.assumingMemoryBound(to: UInt8.self)
        let cDstStride = CVPixelBufferGetBytesPerRowOfPlane(pb, 1)
        for row in 0..<ch {
            let u = us.advanced(by: row * cStride), v = vs.advanced(by: row * cStride)
            let dst = cDst.advanced(by: row * cDstStride)
            if high {
                let u16 = u.assumingMemoryBound(to: UInt16.self), v16 = v.assumingMemoryBound(to: UInt16.self)
                for x in 0..<cw {
                    dst[2 * x] = UInt8(truncatingIfNeeded: u16[x] >> 2)
                    dst[2 * x + 1] = UInt8(truncatingIfNeeded: v16[x] >> 2)
                }
            } else {
                let u8 = u.assumingMemoryBound(to: UInt8.self), v8 = v.assumingMemoryBound(to: UInt8.self)
                for x in 0..<cw {
                    dst[2 * x] = u8[x]
                    dst[2 * x + 1] = v8[x]
                }
            }
        }
        CVBufferSetAttachment(pb, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2, .shouldPropagate)
        CVBufferSetAttachment(pb, kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_709_2, .shouldPropagate)
        CVBufferSetAttachment(pb, kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_ITU_R_709_2, .shouldPropagate)
        return pb
    }
}

enum AV1Error: Error { case open, noTrack }

/// Reads and decodes a video file ahead of the playhead on a background
/// queue; the main thread asks for "the frame for time t".
final class AV1VideoSource: @unchecked Sendable {
    private let asset: AVURLAsset
    private let queue = DispatchQueue(label: "stems.av1", qos: .userInitiated)
    private let lock = NSLock()
    // guarded by lock
    private var frames: [VideoFrame] = []
    /// The last frame before a seek target: shown when paused between
    /// frames, until the next one is due.
    private var primer: VideoFrame?
    private var generation = 0
    private var wanted = 0.0
    private var running = false
    private var ended = false
    /// Frames decoded ahead of the playhead.
    private let ahead = 12

    init(url: URL) { asset = AVURLAsset(url: url) }

    /// Start decoding from `time` (seconds), dropping what was queued.
    func seek(to time: Double) {
        lock.lock()
        generation += 1
        let mine = generation
        frames.removeAll()
        primer = nil
        wanted = time
        ended = false
        lock.unlock()
        queue.async { [self] in run(from: time, generation: mine) }
    }

    /// The newest frame due at `time`, if a new one is due. Older frames
    /// are discarded; frames run out when decoding falls behind.
    func frame(at time: Double) -> VideoFrame? {
        lock.lock()
        defer { lock.unlock() }
        wanted = time
        var due: VideoFrame?
        while let f = frames.first, f.time <= time {
            due = f
            frames.removeFirst()
        }
        if due == nil, let p = primer {
            due = p
        }
        primer = nil
        return due
    }

    /// Whether `time` is outside what's queued (a seek is needed).
    func needsSeek(for time: Double) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if ended { return false }
        guard let first = frames.first, let last = frames.last else { return false }
        return time < first.time - 0.5 || time > last.time + 2
    }

    private func current(_ g: Int) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return g == generation
    }

    private func run(from time: Double, generation g: Int) {
        guard current(g) else { return }
        let sem = DispatchSemaphore(value: 0)
        nonisolated(unsafe) var track: AVAssetTrack?
        nonisolated(unsafe) var format: CMFormatDescription?
        Task {
            track = try? await asset.loadTracks(withMediaType: .video).first
            if let t = track { format = try? await t.load(.formatDescriptions).first }
            sem.signal()
        }
        sem.wait()
        guard let track, let reader = try? AVAssetReader(asset: asset) else { return }
        // passthrough: compressed AV1 samples; the reader starts at the
        // sync sample before `time` so decoding can begin there
        let start = CMTime(seconds: max(0, time), preferredTimescale: 600)
        reader.timeRange = CMTimeRange(start: start, duration: .positiveInfinity)
        let output = AVAssetReaderTrackOutput(track: track, outputSettings: nil)
        output.alwaysCopiesSampleData = false
        reader.add(output)
        guard reader.startReading(), let decoder = try? AV1Decoder() else { return }
        if let format, let config = Self.configOBUs(format) {
            config.withUnsafeBytes { _ = decoder.decode($0, time: -1) }
        }
        while current(g) {
            // keep a few frames ahead, no more
            lock.lock()
            let full = frames.count >= ahead
            lock.unlock()
            if full {
                Thread.sleep(forTimeInterval: 0.01)
                continue
            }
            guard let sample = output.copyNextSampleBuffer() else {
                lock.lock()
                if g == generation { ended = true }
                lock.unlock()
                return
            }
            guard let block = CMSampleBufferGetDataBuffer(sample) else { continue }
            let t = CMSampleBufferGetPresentationTimeStamp(sample).seconds
            var length = 0
            var ptr: UnsafeMutablePointer<CChar>?
            guard CMBlockBufferGetDataPointer(block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length, dataPointerOut: &ptr) == kCMBlockBufferNoErr,
                let ptr
            else { continue }
            let decoded = decoder.decode(UnsafeRawBufferPointer(start: ptr, count: length), time: t)
            lock.lock()
            if g == generation {
                // frames before the seek target only prime the decoder;
                // the last of them stands in until the next is due
                let cut = wanted - 0.02
                for f in decoded where f.time >= 0 {
                    if f.time < cut && frames.isEmpty {
                        primer = f
                    } else if f.time >= cut {
                        frames.append(f)
                    }
                }
                frames.sort { $0.time < $1.time }
            }
            lock.unlock()
        }
        reader.cancelReading()
    }

    /// The sequence header etc. stored in the mp4's av1C box (after its
    /// 4-byte header), fed to dav1d before the first sample.
    private static func configOBUs(_ f: CMFormatDescription) -> Data? {
        guard let ext = CMFormatDescriptionGetExtension(f, extensionKey: kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms) as? [String: Any],
            let av1C = ext["av1C"] as? Data, av1C.count > 4
        else { return nil }
        return av1C.subdata(in: 4..<av1C.count)
    }
}

/// Shows an AV1 video at the time `time()` returns (the preview's audio
/// position plus the video offset), polled every display refresh.
struct AV1PlayerView: NSViewRepresentable {
    let url: URL
    /// Where the video should be now, in seconds.
    let time: @MainActor () -> Double

    func makeNSView(context: Context) -> AV1LayerView {
        let v = AV1LayerView(source: AV1VideoSource(url: url))
        v.time = time
        v.start()
        return v
    }

    func updateNSView(_ v: AV1LayerView, context: Context) { v.time = time }

    static func dismantleNSView(_ v: AV1LayerView, coordinator: ()) { v.stop() }
}

final class AV1LayerView: NSView {
    let source: AV1VideoSource
    var time: (@MainActor () -> Double)?
    private let display = AVSampleBufferDisplayLayer()
    private var timer: Timer?
    private var last = -1.0
    private var format: CMVideoFormatDescription?

    init(source: AV1VideoSource) {
        self.source = source
        super.init(frame: .zero)
        wantsLayer = true
        layer?.backgroundColor = NSColor.black.cgColor
        display.videoGravity = .resizeAspect
        layer?.addSublayer(display)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        display.frame = bounds
    }

    func start() {
        source.seek(to: max(0, time?() ?? 0))
        timer = Timer.scheduledTimer(withTimeInterval: 1.0 / 60, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.tick() }
        }
    }

    func stop() {
        timer?.invalidate()
        timer = nil
    }

    private func tick() {
        guard let t = time?() else { return }
        // jumped (seek, or the audio restarted): decode from there
        if abs(t - last) > 1 || source.needsSeek(for: t) {
            source.seek(to: max(0, t))
        }
        last = t
        if let f = source.frame(at: t) { show(f) }
    }

    private func show(_ f: VideoFrame) {
        if format == nil || !CMVideoFormatDescriptionMatchesImageBuffer(format!, imageBuffer: f.buffer) {
            CMVideoFormatDescriptionCreateForImageBuffer(allocator: nil, imageBuffer: f.buffer, formatDescriptionOut: &format)
        }
        guard let format else { return }
        var timing = CMSampleTimingInfo(duration: .invalid, presentationTimeStamp: .zero, decodeTimeStamp: .invalid)
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreateReadyWithImageBuffer(
            allocator: nil, imageBuffer: f.buffer, formatDescription: format, sampleTiming: &timing, sampleBufferOut: &sample) == noErr,
            let sample
        else { return }
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true) as? [NSMutableDictionary] {
            attachments.first?[kCMSampleAttachmentKey_DisplayImmediately] = true
        }
        if display.sampleBufferRenderer.status == .failed { display.sampleBufferRenderer.flush() }
        display.sampleBufferRenderer.enqueue(sample)
    }
}
