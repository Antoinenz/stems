// First run: scan the pairing code the Mac shows (Settings › Pair iPhone),
// or type the Mac's address to connect as a guest.

import StemsKit
import StemsUI
import AVFoundation
import SwiftUI
import VisionKit

struct PairingScreen: View {
    @Environment(RemoteModel.self) private var model
    var onDone: (() -> Void)?
    @State private var scanning = false
    @State private var typed = ""
    @State private var problem: String?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    VStack(spacing: 12) {
                        Image(systemName: "qrcode.viewfinder")
                            .font(.system(size: 56))
                            .foregroundStyle(.tint)
                        Text("Connect to Your Mac")
                            .font(.title2.bold())
                        Text("On the Mac, choose Stems › Pair iPhone… and scan the code it shows.")
                            .multilineTextAlignment(.center)
                            .foregroundStyle(.secondary)
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 12)
                    Button {
                        scanning = true
                    } label: {
                        Label("Scan Code", systemImage: "camera")
                            .frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
                    .disabled(!DataScannerViewController.isSupported)
                }
                Section {
                    TextField("192.168.1.10 or mac.local", text: $typed)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .submitLabel(.go)
                        .onSubmit(connectTyped)
                    Button("Connect", action: connectTyped)
                        .disabled(typed.trimmingCharacters(in: .whitespaces).isEmpty)
                } header: {
                    Text("Or Enter an Address")
                } footer: {
                    Text("Without the code you connect as a guest: you can request songs if the host allows it.")
                }
                if let problem {
                    Section { Label(problem, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange) }
                }
            }
            .navigationTitle("Pair")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                if let onDone {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Cancel", role: .cancel) { onDone() }
                    }
                }
            }
            .fullScreenCover(isPresented: $scanning) {
                ScannerSheet { endpoint in
                    scanning = false
                    model.connect(endpoint)
                    onDone?()
                }
            }
        }
    }

    private func connectTyped() {
        guard let e = ServerEndpoint(typed: typed) else {
            problem = "That doesn’t look like an address."
            return
        }
        problem = nil
        model.connect(e)
        onDone?()
    }
}

/// Full-screen camera that closes as soon as it sees a stems pairing code.
struct ScannerSheet: View {
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL
    var found: (ServerEndpoint) -> Void
    @State private var hint = "Point the camera at the code on the Mac."
    @State private var access = AVCaptureDevice.authorizationStatus(for: .video)

    var body: some View {
        ZStack(alignment: .bottom) {
            switch access {
            case .authorized:
                QRScanner { text in
                    if let url = URL(string: text), let e = ServerEndpoint(pairingURL: url) {
                        found(e)
                        return true
                    }
                    hint = "That’s not a Stems pairing code."
                    return false
                } failed: { problem in
                    hint = problem
                }
                .ignoresSafeArea()
                Text(hint)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .background(.regularMaterial, in: .capsule)
                    .padding(.bottom, 40)
            case .notDetermined:
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            default:
                ContentUnavailableView {
                    Label("Camera Off for Stems", systemImage: "camera.badge.ellipsis")
                } description: {
                    Text("Allow camera access in Settings to scan the pairing code, or close this and type the Mac’s address.")
                } actions: {
                    Button("Open Settings") {
                        if let url = URL(string: UIApplication.openSettingsURLString) { openURL(url) }
                    }
                    .buttonStyle(.borderedProminent)
                }
            }
        }
        .overlay(alignment: .topTrailing) {
            Button("Close", systemImage: "xmark") { dismiss() }
                .labelStyle(.iconOnly)
                .font(.title2)
                .buttonStyle(.glass)
                .padding()
        }
        .task {
            if access == .notDetermined {
                _ = await AVCaptureDevice.requestAccess(for: .video)
                access = AVCaptureDevice.authorizationStatus(for: .video)
            }
        }
    }
}

/// VisionKit's data scanner, set up for QR codes only.
struct QRScanner: UIViewControllerRepresentable {
    /// Return true to stop scanning.
    var onCode: @MainActor (String) -> Bool
    /// The camera couldn't start (shown to the user).
    var failed: @MainActor (String) -> Void = { _ in }

    func makeUIViewController(context: Context) -> ScannerHost {
        let host = ScannerHost()
        host.scanner.delegate = context.coordinator
        host.failed = failed
        return host
    }

    func updateUIViewController(_ host: ScannerHost, context: Context) {}

    static func dismantleUIViewController(_ host: ScannerHost, coordinator: Coordinator) {
        host.scanner.stopScanning()
    }

    func makeCoordinator() -> Coordinator { Coordinator(onCode: onCode) }

    /// Holds the scanner and starts it once it is on screen (starting
    /// earlier fails silently and leaves the view black).
    final class ScannerHost: UIViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .fast,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false,
            isHighlightingEnabled: true)
        var failed: @MainActor (String) -> Void = { _ in }

        override func viewDidLoad() {
            super.viewDidLoad()
            view.backgroundColor = .black
            addChild(scanner)
            scanner.view.frame = view.bounds
            scanner.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            view.addSubview(scanner.view)
            scanner.didMove(toParent: self)
        }

        override func viewDidAppear(_ animated: Bool) {
            super.viewDidAppear(animated)
            guard !scanner.isScanning else { return }
            guard DataScannerViewController.isAvailable else {
                failed("The camera isn’t available right now.")
                return
            }
            do {
                try scanner.startScanning()
            } catch {
                failed("The camera didn’t start: \(error.localizedDescription)")
            }
        }

        override func viewWillDisappear(_ animated: Bool) {
            super.viewWillDisappear(animated)
            scanner.stopScanning()
        }
    }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onCode: @MainActor (String) -> Bool
        private var done = false

        init(onCode: @escaping @MainActor (String) -> Bool) { self.onCode = onCode }

        func dataScanner(_ scanner: DataScannerViewController, didAdd items: [RecognizedItem], allItems: [RecognizedItem]) {
            guard !done else { return }
            for case .barcode(let code) in items {
                guard let text = code.payloadStringValue else { continue }
                if onCode(text) {
                    done = true
                    scanner.stopScanning()
                    return
                }
            }
        }
    }
}
