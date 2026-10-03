import AVFoundation
import SwiftUI

struct PairingScanner: UIViewControllerRepresentable {
    let scanned: (String) -> Void
    let failed: (String) -> Void
    func makeUIViewController(context: Context) -> QRScannerController {
        QRScannerController(scanned: scanned, failed: failed)
    }
    func updateUIViewController(_ controller: QRScannerController, context: Context) {}
    static func dismantleUIViewController(_ controller: QRScannerController, coordinator: ()) { controller.stop() }
}

/// Capture configuration and start/stop stay off the presentation thread.
private final class QRSession: @unchecked Sendable {
    let session = AVCaptureSession()
    private let queue = DispatchQueue(label: "dev.pika.qr.capture")
    func start(delegate: QRScannerController, failed: @escaping @Sendable () -> Void) {
        queue.async {
            guard let camera = AVCaptureDevice.default(for: .video), let input = try? AVCaptureDeviceInput(device: camera) else { failed(); return }
            let output = AVCaptureMetadataOutput()
            self.session.beginConfiguration()
            guard self.session.canAddInput(input), self.session.canAddOutput(output) else {
                self.session.commitConfiguration(); failed(); return
            }
            self.session.addInput(input); self.session.addOutput(output)
            output.setMetadataObjectsDelegate(delegate, queue: .main)
            output.metadataObjectTypes = [.qr]
            self.session.commitConfiguration(); self.session.startRunning()
        }
    }
    func stop() { queue.async { if self.session.isRunning { self.session.stopRunning() } } }
}

@MainActor
final class QRScannerController: UIViewController, @preconcurrency AVCaptureMetadataOutputObjectsDelegate {
    private let camera = QRSession()
    private let scanned: (String) -> Void
    private let failed: (String) -> Void
    private var active = true
    private var preview: AVCaptureVideoPreviewLayer?
    init(scanned: @escaping (String) -> Void, failed: @escaping (String) -> Void) {
        self.scanned = scanned; self.failed = failed
        super.init(nibName: nil, bundle: nil)
    }
    required init?(coder: NSCoder) { fatalError("Not used") }
    override func viewDidLoad() {
        super.viewDidLoad(); view.backgroundColor = .black
        let preview = AVCaptureVideoPreviewLayer(session: camera.session)
        preview.videoGravity = .resizeAspectFill
        view.layer.addSublayer(preview); self.preview = preview
        Task {
            let allowed = await AVCaptureDevice.requestAccess(for: .video)
            guard active else { return }
            guard allowed else { failed("Camera access is off. Allow Pika in Settings, or use manual login."); return }
            let scanner = self
            camera.start(delegate: self) { [weak scanner] in
                Task { @MainActor in
                    guard let scanner, scanner.active else { return }
                    scanner.failed("A camera is unavailable here. Use manual login instead.")
                }
            }
        }
    }
    override func viewDidLayoutSubviews() { super.viewDidLayoutSubviews(); preview?.frame = view.bounds }
    func stop() { active = false; camera.stop() }
    func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput metadataObjects: [AVMetadataObject], from connection: AVCaptureConnection) {
        guard active, let code = metadataObjects.compactMap({ $0 as? AVMetadataMachineReadableCodeObject }).first,
              let text = code.stringValue else { return }
        stop(); scanned(text)
    }
}
