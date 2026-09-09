import Foundation
import ScreenCaptureKit
import CoreMedia
import CoreVideo
import AVFoundation
import VideoToolbox
import ImageIO
import UniformTypeIdentifiers
import QuartzCore

struct Frame {
    let sample: CMSampleBuffer
    let buffer: CVPixelBuffer
    let t: Double      // host clock seconds (same clock as input events)
    let pts: CMTime
}

/// ScreenCaptureKit stream for the main display. Cursor visible. SCK delivers a frame only when
/// pixels change; the recorder re-appends the last frame to keep the file at a constant rate.
final class ScreenCapture: NSObject, SCStreamOutput, SCStreamDelegate {
    private var stream: SCStream?
    private let queue = DispatchQueue(label: "agent-snap.capture")
    private(set) var width = 0
    private(set) var height = 0
    private(set) var scale: Double = 1
    private var displayOrigin = CGPoint.zero

    /// Called on the capture queue with the frame and the dirty rects in pixels.
    var onFrame: ((Frame, [CGRect]) -> Void)?

    func start() async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        let mainID = CGMainDisplayID()
        guard let display = content.displays.first(where: { $0.displayID == mainID }) ?? content.displays.first else {
            throw NSError(domain: "agent-snap", code: 1, userInfo: [NSLocalizedDescriptionKey: "No display found"])
        }
        let filter = SCContentFilter(display: display, excludingWindows: [])
        let s = Double(filter.pointPixelScale)
        scale = s
        displayOrigin = display.frame.origin
        width = Int((Double(display.width) * s).rounded()) & ~1
        height = Int((Double(display.height) * s).rounded()) & ~1

        let cfg = SCStreamConfiguration()
        cfg.width = width
        cfg.height = height
        cfg.showsCursor = true
        cfg.minimumFrameInterval = CMTime(value: 1, timescale: 30)
        cfg.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        cfg.queueDepth = 8
        cfg.capturesAudio = false

        let st = SCStream(filter: filter, configuration: cfg, delegate: self)
        try st.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        try await st.startCapture()
        stream = st
    }

    func stop() async {
        try? await stream?.stopCapture()
        stream = nil
    }

    /// Global point (points, top-left origin) -> capture pixels.
    func toPixels(_ p: CGPoint) -> CGPoint {
        CGPoint(x: (p.x - displayOrigin.x) * scale, y: (p.y - displayOrigin.y) * scale)
    }
    func toPixels(_ r: CGRect) -> CGRect {
        CGRect(x: (r.origin.x - displayOrigin.x) * scale, y: (r.origin.y - displayOrigin.y) * scale,
               width: r.width * scale, height: r.height * scale)
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sb: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen,
              let atts = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let att = atts.first,
              let statusRaw = att[.status] as? Int,
              let status = SCFrameStatus(rawValue: statusRaw), status == .complete,
              let pb = CMSampleBufferGetImageBuffer(sb) else { return }

        let frameRect = CGRect(x: 0, y: 0, width: width, height: height)
        var dirty: [CGRect] = []
        if let raw = att[.dirtyRects] as? [Any] {
            for d in raw {
                if let dict = d as? NSDictionary, let r = CGRect(dictionaryRepresentation: dict) {
                    let c = r.intersection(frameRect)
                    if !c.isNull, c.width > 0, c.height > 0 { dirty.append(c) }
                }
            }
        }
        let frame = Frame(sample: sb, buffer: pb, t: CACurrentMediaTime(), pts: CMSampleBufferGetPresentationTimeStamp(sb))
        onFrame?(frame, dirty)
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        fputs("capture stopped: \(error.localizedDescription)\n", stderr)
    }
}

/// Writes captured sample buffers to an HEVC .mov. Frames are sparse (change-driven); the
/// video holds each frame until the next one. 10s fragments keep the file playable if the
/// process dies mid-recording.
final class VideoWriter {
    private let writer: AVAssetWriter
    private let input: AVAssetWriterInput
    private var started = false
    private var failed = false
    private var lastPTS: Double = -.infinity
    private(set) var firstPTS = CMTime.invalid
    private(set) var frames = 0
    private(set) var dropped = 0
    private var notReady = 0
    var log: ((String) -> Void)?

    init(url: URL, width: Int, height: Int) throws {
        try? FileManager.default.removeItem(at: url)
        writer = try AVAssetWriter(outputURL: url, fileType: .mov)
        writer.movieFragmentInterval = CMTime(value: 10, timescale: 1)
        let settings: [String: Any] = [
            AVVideoCodecKey: AVVideoCodecType.hevc,
            AVVideoWidthKey: width,
            AVVideoHeightKey: height,
            AVVideoCompressionPropertiesKey: [
                AVVideoAverageBitRateKey: 24_000_000,
                AVVideoExpectedSourceFrameRateKey: 30,
                AVVideoMaxKeyFrameIntervalKey: 30,
                AVVideoAllowFrameReorderingKey: false,
            ],
        ]
        input = AVAssetWriterInput(mediaType: .video, outputSettings: settings)
        input.expectsMediaDataInRealTime = true
        guard writer.canAdd(input) else { throw NSError(domain: "agent-snap", code: 3, userInfo: [NSLocalizedDescriptionKey: "cannot add video input"]) }
        writer.add(input)
    }

    private var lastSample: CMSampleBuffer?
    static let fps: Int32 = 30

    /// Re-append the last frame with a new timestamp (no new content arrived). Keeps constant fps.
    func appendDuplicate(at pts: CMTime) {
        guard started, !failed, let last = lastSample, pts.seconds > lastPTS, input.isReadyForMoreMediaData else { return }
        var timing = CMSampleTimingInfo(duration: CMTime(value: 1, timescale: VideoWriter.fps), presentationTimeStamp: pts, decodeTimeStamp: .invalid)
        var copy: CMSampleBuffer?
        guard CMSampleBufferCreateCopyWithNewTiming(allocator: kCFAllocatorDefault, sampleBuffer: last, sampleTimingEntryCount: 1,
                                                    sampleTimingArray: &timing, sampleBufferOut: &copy) == noErr, let c = copy else { return }
        if input.append(c) { lastPTS = pts.seconds; frames += 1 }
    }

    /// Returns false if the frame was dropped.
    @discardableResult
    func append(_ f: Frame) -> Bool {
        guard !failed, f.pts.isValid else { dropped += 1; return false }
        if !started {
            guard writer.status == .unknown, writer.startWriting() else {
                failed = true
                log?("writer failed to start: \(writer.error?.localizedDescription ?? "unknown")")
                return false
            }
            writer.startSession(atSourceTime: f.pts)
            firstPTS = f.pts
            started = true
            log?("writer started at pts \(f.pts.seconds)")
        }
        guard input.isReadyForMoreMediaData else { dropped += 1; notReady += 1; return false }
        guard f.pts.seconds > lastPTS else { dropped += 1; return false }
        if input.append(f.sample) {
            lastPTS = f.pts.seconds
            lastSample = f.sample
            frames += 1
            return true
        }
        dropped += 1
        if let e = writer.error { failed = true; log?("video append failed: \(e)") }
        return false
    }

    func finish() async {
        guard started, !failed else { log?("writer finish skipped: started=\(started) failed=\(failed)"); return }
        input.markAsFinished()
        await writer.finishWriting()
        log?("writer finished: status=\(writer.status.rawValue) frames=\(frames) dropped=\(dropped) notReady=\(notReady) error=\(writer.error?.localizedDescription ?? "none")")
    }
}

enum ImageIO {
    static func cgImage(from pb: CVPixelBuffer) -> CGImage? {
        var img: CGImage?
        VTCreateCGImageFromCVPixelBuffer(pb, options: nil, imageOut: &img)
        return img
    }

    static func savePNG(_ img: CGImage, to url: URL) {
        guard let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else { return }
        CGImageDestinationAddImage(dest, img, nil)
        CGImageDestinationFinalize(dest)
    }

    static func loadPNG(_ url: URL) -> CGImage? {
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil) else { return nil }
        return CGImageSourceCreateImageAtIndex(src, 0, nil)
    }
}
