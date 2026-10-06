import AVFoundation
import SwiftUI

/// Camera view that reports the first QR code it sees.
struct QRScannerSheet: View {
    let found: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var unavailable = false

    var body: some View {
        NavigationStack {
            ZStack {
                if unavailable {
                    ContentUnavailableView("No camera", systemImage: "camera.fill",
                                           description: Text("Paste the pairing link instead."))
                } else {
                    QRScanner(found: found, unavailable: { unavailable = true })
                        .ignoresSafeArea()
                    RoundedRectangle(cornerRadius: 18, style: .continuous)
                        .stroke(.white.opacity(0.9), lineWidth: 2)
                        .frame(width: 240, height: 240)
                }
            }
            .background(.black)
            .navigationTitle("Scan the QR code")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            }
        }
    }
}

private struct QRScanner: UIViewControllerRepresentable {
    let found: (String) -> Void
    let unavailable: () -> Void

    func makeUIViewController(context: Context) -> ScannerController {
        let controller = ScannerController()
        controller.found = found
        controller.unavailable = unavailable
        return controller
    }

    func updateUIViewController(_ controller: ScannerController, context: Context) {}
}

final class ScannerController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
    var found: ((String) -> Void)?
    var unavailable: (() -> Void)?
    private let session = AVCaptureSession()
    private var reported = false

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        guard let device = AVCaptureDevice.default(for: .video), let input = try? AVCaptureDeviceInput(device: device),
              session.canAddInput(input)
        else {
            unavailable?()
            return
        }
        session.addInput(input)
        let output = AVCaptureMetadataOutput()
        guard session.canAddOutput(output) else {
            unavailable?()
            return
        }
        session.addOutput(output)
        output.setMetadataObjectsDelegate(self, queue: .main)
        output.metadataObjectTypes = [.qr]
        let preview = AVCaptureVideoPreviewLayer(session: session)
        preview.videoGravity = .resizeAspectFill
        preview.frame = view.bounds
        view.layer.addSublayer(preview)
        DispatchQueue.global(qos: .userInitiated).async { [session] in session.startRunning() }
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        view.layer.sublayers?.first?.frame = view.bounds
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        if session.isRunning { DispatchQueue.global(qos: .userInitiated).async { [session] in session.stopRunning() } }
    }

    func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput objects: [AVMetadataObject], from connection: AVCaptureConnection) {
        guard !reported, let code = objects.compactMap({ ($0 as? AVMetadataMachineReadableCodeObject)?.stringValue }).first else { return }
        reported = true
        found?(code)
    }
}
