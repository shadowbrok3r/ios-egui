import EguiKitC
import SwiftUI
import UIKit
import UniformTypeIdentifiers

/// Drop-in SwiftUI view that hosts the egui renderer full-screen. The app's `@main` scene only
/// needs `EguiAppView()`.
public struct EguiAppView: View {
    public init() {}
    public var body: some View {
        EguiView().ignoresSafeArea()
    }
}

struct EguiView: UIViewRepresentable {
    func makeCoordinator() -> EguiCoordinator { EguiCoordinator() }

    func makeUIView(context: Context) -> UIView {
        let container = UIView()
        container.backgroundColor = .black

        let host = MetalHostView(frame: .zero)
        host.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(host)
        NSLayoutConstraint.activate([
            host.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            host.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            host.topAnchor.constraint(equalTo: container.topAnchor),
            host.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])

        context.coordinator.attach(container: container, host: host)
        return container
    }

    func updateUIView(_ uiView: UIView, context: Context) {}

    static func dismantleUIView(_ uiView: UIView, coordinator: EguiCoordinator) {
        coordinator.teardown()
    }
}

@MainActor
final class EguiCoordinator: NSObject, UIDocumentPickerDelegate {
    private var renderer: EguiRenderer?
    private var link: CADisplayLink?
    private var startTime = CACurrentMediaTime()
    private weak var host: MetalHostView?
    private weak var container: UIView?
    private var keyboardShown = false
    /// Last keyboard end frame in screen coordinates; replayed once the renderer exists.
    private var keyboardFrame: CGRect?
    private var failed = false
    private let camera = CameraController()
    private let mic = MicLevelMonitor()

    func attach(container: UIView, host: MetalHostView) {
        self.container = container
        self.host = host
        camera.attach(container: container)

        host.onTouch = { [weak self] phase, p in self?.renderer?.touch(phase, p) }
        host.onText = { [weak self] t in self?.renderer?.insertText(t) }
        host.onDeleteBackward = { [weak self] in self?.renderer?.deleteBackward() }
        host.onKey = { [weak self] hid, mods, pressed in
            self?.renderer?.keyEvent(hid: hid, mods: mods, pressed: pressed)
        }
        host.onScroll = { [weak self] dx, dy in self?.renderer?.scroll(Float(dx), Float(dy)) }
        host.onHover = { [weak self] p in self?.renderer?.pointerMoved(p) }
        host.onLayout = { [weak self] in self?.handleLayout() }

        camera.onPermission = { [weak self] granted in
            self?.renderer?.onPermissionResult(kind: 0, granted: granted)
        }
        mic.onPermission = { [weak self] granted in
            self?.renderer?.onPermissionResult(kind: 1, granted: granted)
            if granted { self?.mic.start() }
        }
        mic.onLevel = { [weak self] level in self?.renderer?.onMicLevel(level) }

        NotificationCenter.default.addObserver(
            self, selector: #selector(keyboardWillChange(_:)),
            name: UIResponder.keyboardWillChangeFrameNotification, object: nil
        )
        NotificationCenter.default.addObserver(
            self, selector: #selector(keyboardWillHide(_:)),
            name: UIResponder.keyboardWillHideNotification, object: nil
        )
    }

    @objc private func keyboardWillChange(_ note: Notification) {
        guard
            let end = (note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? NSValue)?
                .cgRectValue
        else { return }
        keyboardFrame = end
        pushKeyboardHeight()
    }

    @objc private func keyboardWillHide(_ note: Notification) {
        keyboardFrame = nil
        pushKeyboardHeight()
    }

    private func pushKeyboardHeight() {
        guard let r = renderer, let view = container, let window = view.window else { return }
        var overlap: CGFloat = 0
        if let frame = keyboardFrame {
            // Docked keyboards are bottom-anchored and near-full-width; floating/undocked
            // iPad keyboards reserve no space.
            let screen = window.screen.bounds
            if frame.maxY >= screen.maxY - 1, frame.width >= screen.width * 0.7 {
                let frameInView = view.convert(
                    window.convert(frame, from: window.screen.coordinateSpace), from: window
                )
                overlap = max(0, view.bounds.maxY - frameInView.minY)
            }
        }
        r.setKeyboardHeight(Float(overlap))
    }

    private func ensureRenderer() {
        guard !failed, renderer == nil, let host, host.bounds.width > 0, host.bounds.height > 0
        else { return }
        assert(egui_ios_abi_version() == 2, "egui-ios ABI mismatch: rebuild the Rust staticlib")

        let scale = host.window?.screen.scale ?? UIScreen.main.scale
        let w = UInt32(max(1, host.bounds.width * scale))
        let h = UInt32(max(1, host.bounds.height * scale))
        let layerPtr = Unmanaged.passUnretained(host.metalLayer).toOpaque()
        guard let r = EguiRenderer(layer: layerPtr, widthPx: w, heightPx: h, ppp: Float(scale)) else {
            failed = true
            showError(EguiRenderer.lastError() ?? "egui_ios_new returned null")
            return
        }
        renderer = r

        if let docs = try? FileManager.default.url(
            for: .documentDirectory, in: .userDomainMask, appropriateFor: nil, create: true
        ) {
            r.setDocumentsDir(docs.path)
        }
        pushSafeArea()
        pushKeyboardHeight()

        let link = CADisplayLink(target: self, selector: #selector(tick))
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    private func handleLayout() {
        ensureRenderer()
        guard let r = renderer, let host else { return }
        let scale = host.window?.screen.scale ?? UIScreen.main.scale
        r.resize(
            UInt32(max(1, host.bounds.width * scale)),
            UInt32(max(1, host.bounds.height * scale))
        )
        r.setPixelsPerPoint(Float(scale))
        pushSafeArea()
        pushKeyboardHeight()
        camera.layout(host.bounds)
    }

    private func pushSafeArea() {
        guard let r = renderer, let v = container else { return }
        let i = v.safeAreaInsets
        r.setSafeArea(
            top: Float(i.top), bottom: Float(i.bottom), left: Float(i.left), right: Float(i.right)
        )
    }

    @objc private func tick() {
        guard let r = renderer else { return }
        r.render(CACurrentMediaTime() - startTime)

        let want = r.wantsKeyboard()
        if want != keyboardShown {
            keyboardShown = want
            if want { _ = host?.becomeFirstResponder() } else { _ = host?.resignFirstResponder() }
        }

        r.pollRequests { [weak self] kind in self?.dispatch(kind) }
    }

    private func dispatch(_ kind: Int32) {
        guard let r = renderer else { return }
        switch kind {
        case 0: Capabilities.share(path: r.requestStrA(), from: host)
        case 1: Capabilities.notify(title: r.requestStrA(), body: r.requestStrB())
        case 2:
            let show = r.requestInt() != 0
            if show { _ = host?.becomeFirstResponder() } else { _ = host?.resignFirstResponder() }
        case 3: Capabilities.haptic(r.requestInt())
        case 4: Capabilities.openURL(r.requestStrA())
        case 5: presentDocumentPicker(types: r.requestStrA())
        case 6: camera.requestPermission()
        case 7: mic.requestPermission()
        case 8: camera.start()
        case 9: camera.stop()
        case 10: UIPasteboard.general.string = r.requestStrA()
        default: break
        }
    }

    private func presentDocumentPicker(types: String) {
        let utis = types.split(separator: "\n").compactMap { UTType(String($0)) }
        let picker = UIDocumentPickerViewController(
            forOpeningContentTypes: utis.isEmpty ? [.item] : utis
        )
        picker.delegate = self
        picker.allowsMultipleSelection = false
        Capabilities.presenter(from: host)?.present(picker, animated: true)
    }

    private func showError(_ message: String) {
        guard let container else { return }
        let label = UILabel()
        label.numberOfLines = 0
        label.textColor = .systemRed
        label.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        label.text = "egui-ios failed to start:\n\n\(message)"
        label.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: container.safeAreaLayoutGuide.leadingAnchor, constant: 16),
            label.trailingAnchor.constraint(equalTo: container.safeAreaLayoutGuide.trailingAnchor, constant: -16),
            label.topAnchor.constraint(equalTo: container.safeAreaLayoutGuide.topAnchor, constant: 16),
        ])
    }

    func teardown() {
        link?.invalidate()
        link = nil
        camera.stop()
        mic.stop()
        renderer = nil
    }

    // MARK: UIDocumentPickerDelegate

    func documentPicker(
        _ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]
    ) {
        guard let url = urls.first else { return }
        let needsScope = url.startAccessingSecurityScopedResource()
        defer { if needsScope { url.stopAccessingSecurityScopedResource() } }

        // Copy into Documents so the egui app can read it without a security scope.
        if let docs = try? FileManager.default.url(
            for: .documentDirectory, in: .userDomainMask, appropriateFor: nil, create: true
        ) {
            let dest = docs.appendingPathComponent(url.lastPathComponent)
            try? FileManager.default.removeItem(at: dest)
            if (try? FileManager.default.copyItem(at: url, to: dest)) != nil {
                renderer?.onFilePicked(dest.path)
                return
            }
        }
        renderer?.onFilePicked(url.path)
    }
}
