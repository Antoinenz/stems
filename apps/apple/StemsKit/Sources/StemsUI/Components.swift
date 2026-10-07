// Small pieces every screen uses.

import CoreImage
import CoreImage.CIFilterBuiltins
import StemsKit
import SwiftUI

// MARK: Formatting

enum Format {
    /// `3:05`, `1:02:03`.
    static func time(ms: Int) -> String {
        let total = max(0, ms / 1000)
        let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60)
        return h > 0 ? String(format: "%d:%02d:%02d", h, m, s) : String(format: "%d:%02d", m, s)
    }

    static func bytes(_ b: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: b, countStyle: .file)
    }

    /// `2 h 5 min`, `40 s`.
    static func duration(seconds: Int) -> String {
        let f = DateComponentsFormatter()
        f.allowedUnits = seconds >= 3600 ? [.hour, .minute] : seconds >= 60 ? [.minute, .second] : [.second]
        f.unitsStyle = .abbreviated
        return f.string(from: TimeInterval(seconds)) ?? "\(seconds) s"
    }

    static func percent(_ fraction: Double) -> String {
        fraction.formatted(.percent.precision(.fractionLength(0)))
    }
}

// MARK: Artwork

/// Cover art from the server (or Spotify), with a symbol while it loads.
public struct ArtworkView: View {
    var url: URL?
    var size: CGFloat?
    var cornerRadius: CGFloat

    public init(url: URL?, size: CGFloat? = nil, cornerRadius: CGFloat = 6) {
        self.url = url
        self.size = size
        self.cornerRadius = cornerRadius
    }

    public var body: some View {
        AsyncImage(url: url, transaction: Transaction(animation: .default)) { phase in
            switch phase {
            case .success(let image):
                image.resizable().aspectRatio(contentMode: .fill)
            default:
                ZStack {
                    Rectangle().fill(.quaternary)
                    Image(systemName: "music.note")
                        .font(.system(size: (size ?? 120) * 0.4))
                        .foregroundStyle(.secondary)
                }
            }
        }
        .frame(width: size, height: size)
        .aspectRatio(1, contentMode: .fit)
        .clipShape(.rect(cornerRadius: cornerRadius))
        .accessibilityHidden(true)
    }
}

// MARK: QR code

/// A crisp QR code for a pairing link or the guests' web address.
public struct QRCodeView: View {
    var text: String

    public init(_ text: String) { self.text = text }

    public var body: some View {
        if let image = Self.render(text) {
            Image(decorative: image, scale: 1)
                .interpolation(.none)
                .resizable()
                .aspectRatio(1, contentMode: .fit)
                .padding(10)
                .background(.white, in: .rect(cornerRadius: 12))
        } else {
            ContentUnavailableView("No QR Code", systemImage: "qrcode")
        }
    }

    static func render(_ text: String) -> CGImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage else { return nil }
        return CIContext().createCGImage(output, from: output.extent)
    }
}

// MARK: Status bits

/// A dot plus words: state is never shown by color alone.
struct StatusLabel: View {
    enum Level { case ok, warning, problem, off }

    var text: String
    var level: Level

    var body: some View {
        // an HStack, not a Label: a Label inside LabeledContent in a Form
        // gets a runaway row height on iOS 26
        HStack(spacing: 6) {
            Image(systemName: symbol).foregroundStyle(color)
            Text(text)
        }
        .accessibilityElement(children: .combine)
    }

    private var symbol: String {
        switch level {
        case .ok: "checkmark.circle.fill"
        case .warning: "exclamationmark.triangle.fill"
        case .problem: "xmark.octagon.fill"
        case .off: "minus.circle"
        }
    }

    private var color: Color {
        switch level {
        case .ok: .green
        case .warning: .orange
        case .problem: .red
        case .off: .secondary
        }
    }
}

/// Shown above content while the server can't be reached, instead of an
/// alert (the content stays visible with its last known values).
public struct ConnectionBanner: View {
    @Environment(StemsClient.self) private var client

    public init() {}

    public var body: some View {
        switch client.connection {
        case .connected, .idle:
            EmptyView()
        case .connecting:
            banner("Connecting to \(client.endpoint.name)…", symbol: "antenna.radiowaves.left.and.right", showRetry: false)
        case .waiting(let reason, _):
            banner(reason, symbol: "wifi.exclamationmark", showRetry: true)
        }
    }

    private func banner(_ text: String, symbol: String, showRetry: Bool) -> some View {
        HStack(spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(.orange)
                .symbolEffect(.pulse, isActive: !showRetry)
            Text(text).font(.callout)
            Spacer(minLength: 0)
            if showRetry {
                Button("Retry") { client.reconnectNow() }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .background(.orange.opacity(0.12))
        .accessibilityElement(children: .combine)
    }
}

/// A command error, shown briefly under the content's top edge.
struct ErrorToast: ViewModifier {
    @Environment(StemsClient.self) private var client

    func body(content: Content) -> some View {
        @Bindable var client = client
        content.overlay(alignment: .top) {
            if let message = client.lastError {
                Label(message, systemImage: "exclamationmark.circle.fill")
                    .font(.callout)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .background(.regularMaterial, in: .capsule)
                    .padding(.top, 8)
                    .transition(.move(edge: .top).combined(with: .opacity))
                    .onTapGesture { client.lastError = nil }
                    .task(id: message) {
                        try? await Task.sleep(for: .seconds(4))
                        if client.lastError == message { client.lastError = nil }
                    }
                    .accessibilityAddTraits(.isStaticText)
            }
        }
        .animation(.default, value: client.lastError)
    }
}

extension View {
    public func stemsErrorToast() -> some View { modifier(ErrorToast()) }
}

/// What a request is: someone sings, or the song just plays.
extension QueueKind {
    var title: LocalizedStringKey {
        switch self {
        case .sing: "Sing"
        case .play: "Just Play"
        }
    }

    var symbol: String {
        switch self {
        case .sing: "music.mic"
        case .play: "music.note"
        }
    }
}
