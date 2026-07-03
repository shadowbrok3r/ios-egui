@preconcurrency import AVFoundation
import UIKit

/// Manages camera permission and a preview layer shown behind the (transparent) egui surface.
@MainActor
final class CameraController {
    private let session = AVCaptureSession()
    private let sessionQueue = DispatchQueue(label: "egui-ios.camera")
    private var previewLayer: AVCaptureVideoPreviewLayer?
    private weak var container: UIView?
    private var pendingStart = false

    /// Permission result callback (granted/denied).
    var onPermission: ((Bool) -> Void)?

    func attach(container: UIView) {
        self.container = container
    }

    func requestPermission() {
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized:
            onPermission?(true)
            if pendingStart { pendingStart = false; startSession() }
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .video) { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self else { return }
                    self.onPermission?(granted)
                    if granted, self.pendingStart { self.pendingStart = false; self.startSession() }
                }
            }
        default:
            onPermission?(false)
        }
    }

    func start() {
        guard previewLayer == nil else { return }
        if AVCaptureDevice.authorizationStatus(for: .video) == .authorized {
            startSession()
        } else {
            pendingStart = true
            requestPermission()
        }
    }

    private func startSession() {
        guard previewLayer == nil, let container else { return }
        guard
            let device = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .front)
                ?? AVCaptureDevice.default(for: .video),
            let input = try? AVCaptureDeviceInput(device: device),
            session.canAddInput(input)
        else { return }

        session.beginConfiguration()
        session.addInput(input)
        session.commitConfiguration()

        let preview = AVCaptureVideoPreviewLayer(session: session)
        preview.videoGravity = .resizeAspectFill
        preview.frame = container.bounds
        container.layer.insertSublayer(preview, at: 0)
        previewLayer = preview

        sessionQueue.async { [session] in session.startRunning() }
    }

    func stop() {
        pendingStart = false
        sessionQueue.async { [session] in session.stopRunning() }
        previewLayer?.removeFromSuperlayer()
        previewLayer = nil
    }

    func layout(_ frame: CGRect) {
        previewLayer?.frame = frame
    }
}
