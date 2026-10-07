// First run: scan the pairing code the Mac shows (Settings › Pair iPhone),
// or type the Mac's address to connect as a guest.

import StemsKit
import StemsUI
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
    var found: (ServerEndpoint) -> Void
    @State private var hint = "Point the camera at the code on the Mac."

    var body: some View {
        ZStack(alignment: .bottom) {
            QRScanner { text in
                if let url = URL(string: text), let e = ServerEndpoint(pairingURL: url) {
                    found(e)
                    return true
                }
                hint = "That’s not a Stems pairing code."
                return false
            }
            .ignoresSafeArea()
            Text(hint)
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
                .background(.regularMaterial, in: .capsule)
                .padding(.bottom, 40)
        }
        .overlay(alignment: .topTrailing) {
            Button("Close", systemImage: "xmark") { dismiss() }
                .labelStyle(.iconOnly)
                .font(.title2)
                .buttonStyle(.glass)
                .padding()
        }
    }
}

/// VisionKit's data scanner, set up for QR codes only.
struct QRScanner: UIViewControllerRepresentable {
    /// Return true to stop scanning.
    var onCode: @MainActor (String) -> Bool

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let vc = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .fast,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false,
            isHighlightingEnabled: true)
        vc.delegate = context.coordinator
        try? vc.startScanning()
        return vc
    }

    func updateUIViewController(_ vc: DataScannerViewController, context: Context) {}

    static func dismantleUIViewController(_ vc: DataScannerViewController, coordinator: Coordinator) {
        vc.stopScanning()
    }

    func makeCoordinator() -> Coordinator { Coordinator(onCode: onCode) }

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
