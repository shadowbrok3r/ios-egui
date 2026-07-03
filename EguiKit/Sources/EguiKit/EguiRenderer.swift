import EguiKitC
import Foundation
import UIKit

/// Thin, typed wrapper over the `egui_ios_*` C ABI. Owns the renderer handle for its lifetime.
final class EguiRenderer {
    let handle: EguiHandle

    init?(layer: UnsafeMutableRawPointer, widthPx: UInt32, heightPx: UInt32, ppp: Float) {
        guard let h = egui_ios_new(layer, widthPx, heightPx, ppp) else { return nil }
        handle = h
    }

    deinit { egui_ios_free(handle) }

    /// The last Rust panic/init error, if any (consumed).
    static func lastError() -> String? {
        guard let p = egui_ios_last_error() else { return nil }
        let s = String(cString: p)
        egui_ios_string_free(p)
        return s.isEmpty ? nil : s
    }

    func render(_ time: Double) { egui_ios_render(handle, time) }
    func resize(_ w: UInt32, _ h: UInt32) { egui_ios_resize(handle, w, h) }
    func setPixelsPerPoint(_ p: Float) { egui_ios_set_pixels_per_point(handle, p) }
    func setActive(_ active: Bool) { egui_ios_set_app_active(handle, active) }
    func setDocumentsDir(_ path: String) { egui_ios_set_documents_dir(handle, path) }

    func setSafeArea(top: Float, bottom: Float, left: Float, right: Float) {
        egui_ios_set_safe_area_insets(handle, top, bottom, left, right)
    }

    func setKeyboardHeight(_ pts: Float) { egui_ios_set_keyboard_height(handle, pts) }

    func wantsKeyboard() -> Bool { egui_ios_wants_keyboard(handle) }

    func touch(_ phase: UITouch.Phase, _ p: CGPoint) {
        let x = Float(p.x)
        let y = Float(p.y)
        switch phase {
        case .began: egui_ios_touch_began(handle, x, y)
        case .moved: egui_ios_touch_moved(handle, x, y)
        case .ended: egui_ios_touch_ended(handle, x, y)
        default: egui_ios_touch_cancelled(handle, x, y)
        }
    }

    func insertText(_ s: String) { egui_ios_insert_text(handle, s) }
    func deleteBackward() { egui_ios_delete_backward(handle) }
    func keyEvent(hid: Int32, mods: Int32, pressed: Bool) {
        egui_ios_key_event(handle, hid, mods, pressed)
    }

    func scroll(_ dx: Float, _ dy: Float) { egui_ios_scroll(handle, dx, dy) }
    func pointerMoved(_ p: CGPoint) { egui_ios_pointer_moved(handle, Float(p.x), Float(p.y)) }
    func pointerGone() { egui_ios_pointer_gone(handle) }

    func onFilePicked(_ path: String) { egui_ios_on_file_picked(handle, path) }
    func onPermissionResult(kind: Int32, granted: Bool) {
        egui_ios_on_permission_result(handle, kind, granted)
    }
    func onMicLevel(_ level: Float) { egui_ios_on_mic_level(handle, level) }

    /// Drain queued host requests, invoking `handler` with each request kind.
    func pollRequests(_ handler: (Int32) -> Void) {
        var kind: Int32 = 0
        while egui_ios_poll_request(handle, &kind) {
            handler(kind)
        }
    }

    func requestStrA() -> String { consume(egui_ios_request_str_a(handle)) }
    func requestStrB() -> String { consume(egui_ios_request_str_b(handle)) }
    func requestInt() -> Int32 { egui_ios_request_int(handle) }

    private func consume(_ p: UnsafeMutablePointer<CChar>?) -> String {
        guard let p else { return "" }
        let s = String(cString: p)
        egui_ios_string_free(p)
        return s
    }
}
