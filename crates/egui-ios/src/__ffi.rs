//! Generic, non-`#[no_mangle]` FFI bodies. The [`crate::app!`] macro emits the thin
//! `extern "C"` shims that call these on the boxed [`crate::Runtime`] behind the opaque handle.
//! Internal: not a stable API. All functions are main-thread only.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::Mutex;

use crate::host::HostRequest;
use crate::{EguiApp, Host, RenderCore, Runtime};

/// Last panic message, surfaced via `egui_ios_last_error` so the host can display init failures.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if let Ok(mut g) = LAST_ERROR.lock() {
                *g = Some(info.to_string());
            }
            prev(info);
        }));
    });
}

#[doc(hidden)]
pub enum TouchPhase {
    Began,
    Moved,
    Ended,
    Cancelled,
}

unsafe fn rt<'a>(h: *mut c_void) -> Option<&'a mut Runtime> {
    if h.is_null() {
        None
    } else {
        Some(unsafe { &mut *(h as *mut Runtime) })
    }
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

fn into_cstr(s: Option<String>) -> *mut c_char {
    match s {
        Some(s) => CString::new(s).unwrap_or_default().into_raw(),
        None => std::ptr::null_mut(),
    }
}

pub unsafe fn new(
    app: Box<dyn EguiApp>,
    ca_metal_layer: *mut c_void,
    w_px: u32,
    h_px: u32,
    ppp: f32,
) -> *mut c_void {
    install_panic_hook();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        #[cfg_attr(not(feature = "plugins"), allow(unused_mut))]
        let mut core = RenderCore::new(ca_metal_layer, w_px, h_px, ppp);
        #[cfg(feature = "plugins")]
        core.install_plugin_painter();
        let host = Host::new();
        let mut runtime = Box::new(Runtime { core, app, host });
        crate::theme::apply(&runtime.core.egui_ctx);
        runtime.app.theme(&runtime.core.egui_ctx);
        {
            let app = &mut runtime.app;
            let host = &runtime.host;
            app.on_start(&runtime.core.egui_ctx, host);
        }
        Box::into_raw(runtime) as *mut c_void
    }));
    result.unwrap_or(std::ptr::null_mut())
}

pub unsafe fn last_error() -> *mut c_char {
    let msg = LAST_ERROR.lock().ok().and_then(|g| g.clone());
    into_cstr(msg)
}

pub unsafe fn free(h: *mut c_void) {
    if !h.is_null() {
        drop(unsafe { Box::from_raw(h as *mut Runtime) });
    }
}

pub unsafe fn resize(h: *mut c_void, w_px: u32, h_px: u32) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.core.resize(w_px, h_px);
    }
}

pub unsafe fn set_pixels_per_point(h: *mut c_void, ppp: f32) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.core.set_pixels_per_point(ppp);
    }
}

pub unsafe fn render(h: *mut c_void, time_seconds: f64) {
    if let Some(rt) = unsafe { rt(h) } {
        let core = &mut rt.core;
        let app = &mut rt.app;
        let host = &rt.host;
        core.render(time_seconds, |ui| app.update(ui, host));
        if let Some(url) = core.take_open_url() {
            host.open_url(url);
        }
        if let Some(text) = core.take_copied_text() {
            host.copy_text(text);
        }
    }
}

pub unsafe fn touch(h: *mut c_void, phase: TouchPhase, x_pt: f32, y_pt: f32) {
    if let Some(rt) = unsafe { rt(h) } {
        match phase {
            TouchPhase::Began => rt.core.touch_began(x_pt, y_pt),
            TouchPhase::Moved => rt.core.touch_moved(x_pt, y_pt),
            TouchPhase::Ended => rt.core.touch_ended(x_pt, y_pt),
            TouchPhase::Cancelled => rt.core.touch_cancelled(x_pt, y_pt),
        }
    }
}

pub unsafe fn wants_keyboard(h: *mut c_void) -> bool {
    unsafe { rt(h) }.map(|rt| rt.core.wants_keyboard).unwrap_or(false)
}

pub unsafe fn insert_text(h: *mut c_void, utf8: *const c_char) {
    if let Some(rt) = unsafe { rt(h) } {
        let text = unsafe { cstr(utf8) };
        rt.core.insert_text(&text);
    }
}

pub unsafe fn delete_backward(h: *mut c_void) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.core.delete_backward();
    }
}

pub unsafe fn key_event(h: *mut c_void, hid_key_code: i32, modifier_flags: i32, pressed: bool) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.core.key_event(hid_key_code, modifier_flags, pressed);
    }
}

pub unsafe fn scroll(h: *mut c_void, dx_pt: f32, dy_pt: f32) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.core.scroll(dx_pt, dy_pt);
    }
}

pub unsafe fn pointer_moved(h: *mut c_void, x_pt: f32, y_pt: f32) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.core.pointer_moved(x_pt, y_pt);
    }
}

pub unsafe fn pointer_gone(h: *mut c_void) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.core.pointer_gone();
    }
}

pub unsafe fn set_documents_dir(h: *mut c_void, path: *const c_char) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.host.inner.borrow_mut().documents_dir = Some(unsafe { cstr(path) });
    }
}

pub unsafe fn set_safe_area_insets(h: *mut c_void, top: f32, bottom: f32, left: f32, right: f32) {
    if let Some(rt) = unsafe { rt(h) } {
        let mut st = rt.host.inner.borrow_mut();
        st.safe_area.top = top;
        st.safe_area.bottom = bottom;
        st.safe_area.left = left;
        st.safe_area.right = right;
    }
}

pub unsafe fn set_keyboard_height(h: *mut c_void, pts: f32) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.host.inner.borrow_mut().keyboard_height = pts.max(0.0);
    }
}

pub unsafe fn set_app_active(h: *mut c_void, active: bool) {
    if let Some(rt) = unsafe { rt(h) } {
        let was = rt.host.inner.borrow().active;
        rt.host.inner.borrow_mut().active = active;
        rt.core.set_active(active);
        if active && !was {
            let host = &rt.host;
            rt.app.on_resume(host);
        } else if !active && was {
            let host = &rt.host;
            rt.app.on_pause(host);
        }
    }
}

pub unsafe fn poll_request(h: *mut c_void, out_kind: *mut i32) -> bool {
    let Some(rt) = (unsafe { rt(h) }) else {
        return false;
    };
    let mut st = rt.host.inner.borrow_mut();
    if let Some(req) = st.queue.pop_front() {
        if !out_kind.is_null() {
            unsafe { *out_kind = req.kind_code() };
        }
        st.current = Some(req);
        true
    } else {
        st.current = None;
        false
    }
}

pub unsafe fn request_str_a(h: *mut c_void) -> *mut c_char {
    match unsafe { rt(h) } {
        Some(rt) => into_cstr(rt.host.inner.borrow().current.as_ref().and_then(|r| r.str_a())),
        None => std::ptr::null_mut(),
    }
}

pub unsafe fn request_str_b(h: *mut c_void) -> *mut c_char {
    match unsafe { rt(h) } {
        Some(rt) => into_cstr(rt.host.inner.borrow().current.as_ref().and_then(|r| r.str_b())),
        None => std::ptr::null_mut(),
    }
}

pub unsafe fn request_int(h: *mut c_void) -> i32 {
    unsafe { rt(h) }
        .and_then(|rt| rt.host.inner.borrow().current.as_ref().map(|r| r.int()))
        .unwrap_or(0)
}

pub unsafe fn on_file_picked(h: *mut c_void, path: *const c_char) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.host.inner.borrow_mut().picked_file = Some(unsafe { cstr(path) });
    }
}

pub unsafe fn on_permission_result(h: *mut c_void, kind: i32, granted: bool) {
    if let Some(rt) = unsafe { rt(h) } {
        if kind == 0 || kind == 1 {
            rt.host.inner.borrow_mut().permissions[kind as usize] = Some(granted);
        }
    }
}

pub unsafe fn on_mic_level(h: *mut c_void, level: f32) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.host.inner.borrow_mut().mic_level = level;
    }
}

pub unsafe fn register_haptic_cb(h: *mut c_void, cb: extern "C" fn(i32)) {
    if let Some(rt) = unsafe { rt(h) } {
        rt.host.inner.borrow_mut().haptic_cb = Some(cb);
    }
}

pub unsafe fn string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

// Reference the request enum so a future variant addition is a compile error here.
const _: fn() = || {
    let _ = HostRequest::SetKeyboard(true).kind_code();
};
