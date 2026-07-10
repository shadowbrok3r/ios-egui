//! iOS backend for the shared [`egui_mobile_core`] runtime. Implement [`EguiApp`] and invoke
//! [`app!`]; the macro emits the C ABI consumed by the `EguiKit` Swift package. No Swift or FFI
//! in app code. The `EguiApp` trait and `Host` bridge live in `egui-mobile-core` so the same app
//! compiles for Android too.

pub use egui;

mod input;
mod render_core;

#[cfg(feature = "plugins")]
pub mod plugins;

#[doc(hidden)]
pub mod __ffi;

/// The app-author API, re-exported from the platform-neutral core.
pub use egui_mobile_core::{CreateContext, EguiApp, Haptic, Host, Insets, Permission};
pub use render_core::RenderCore;

/// The default egui visual theme (near-black surfaces, pink/purple accents). Applied to the
/// context automatically before [`EguiApp::theme`]; call [`theme::apply`] to reapply it.
pub use egui_ios_plugin_abi::theme;

/// ABI version asserted by the Swift host at startup; bump on any breaking C ABI change.
pub const ABI_VERSION: u32 = 2;

/// Owns the renderer, the boxed app, and the host bridge behind the opaque FFI handle.
pub struct Runtime {
    pub core: RenderCore,
    pub app: Box<dyn EguiApp>,
    pub host: Host,
}

/// Generates the `egui_ios_*` C ABI for a type implementing [`EguiApp`].
///
/// `factory` is any `Fn(&CreateContext) -> impl EguiApp`, e.g. `app!(MyApp::new)`.
#[macro_export]
macro_rules! app {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_abi_version() -> u32 {
            $crate::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_new(
            ca_metal_layer: *mut ::core::ffi::c_void,
            w_px: u32,
            h_px: u32,
            ppp: f32,
        ) -> *mut ::core::ffi::c_void {
            let cc = $crate::CreateContext {
                width_px: w_px,
                height_px: h_px,
                pixels_per_point: ppp,
            };
            let app: ::std::boxed::Box<dyn $crate::EguiApp> =
                ::std::boxed::Box::new($factory(&cc));
            unsafe { $crate::__ffi::new(app, ca_metal_layer, w_px, h_px, ppp) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_free(h: *mut ::core::ffi::c_void) {
            unsafe { $crate::__ffi::free(h) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_resize(h: *mut ::core::ffi::c_void, w_px: u32, h_px: u32) {
            unsafe { $crate::__ffi::resize(h, w_px, h_px) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_set_pixels_per_point(h: *mut ::core::ffi::c_void, ppp: f32) {
            unsafe { $crate::__ffi::set_pixels_per_point(h, ppp) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_render(h: *mut ::core::ffi::c_void, time_seconds: f64) {
            unsafe { $crate::__ffi::render(h, time_seconds) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_touch_began(h: *mut ::core::ffi::c_void, x_pt: f32, y_pt: f32) {
            unsafe { $crate::__ffi::touch(h, $crate::__ffi::TouchPhase::Began, x_pt, y_pt) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_touch_moved(h: *mut ::core::ffi::c_void, x_pt: f32, y_pt: f32) {
            unsafe { $crate::__ffi::touch(h, $crate::__ffi::TouchPhase::Moved, x_pt, y_pt) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_touch_ended(h: *mut ::core::ffi::c_void, x_pt: f32, y_pt: f32) {
            unsafe { $crate::__ffi::touch(h, $crate::__ffi::TouchPhase::Ended, x_pt, y_pt) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_touch_cancelled(h: *mut ::core::ffi::c_void, x_pt: f32, y_pt: f32) {
            unsafe { $crate::__ffi::touch(h, $crate::__ffi::TouchPhase::Cancelled, x_pt, y_pt) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_wants_keyboard(h: *mut ::core::ffi::c_void) -> bool {
            unsafe { $crate::__ffi::wants_keyboard(h) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_insert_text(h: *mut ::core::ffi::c_void, utf8: *const ::core::ffi::c_char) {
            unsafe { $crate::__ffi::insert_text(h, utf8) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_delete_backward(h: *mut ::core::ffi::c_void) {
            unsafe { $crate::__ffi::delete_backward(h) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_key_event(
            h: *mut ::core::ffi::c_void,
            hid_key_code: i32,
            modifier_flags: i32,
            pressed: bool,
        ) {
            unsafe { $crate::__ffi::key_event(h, hid_key_code, modifier_flags, pressed) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_scroll(h: *mut ::core::ffi::c_void, dx_pt: f32, dy_pt: f32) {
            unsafe { $crate::__ffi::scroll(h, dx_pt, dy_pt) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_pointer_moved(h: *mut ::core::ffi::c_void, x_pt: f32, y_pt: f32) {
            unsafe { $crate::__ffi::pointer_moved(h, x_pt, y_pt) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_pointer_gone(h: *mut ::core::ffi::c_void) {
            unsafe { $crate::__ffi::pointer_gone(h) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_set_documents_dir(h: *mut ::core::ffi::c_void, path: *const ::core::ffi::c_char) {
            unsafe { $crate::__ffi::set_documents_dir(h, path) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_set_safe_area_insets(
            h: *mut ::core::ffi::c_void,
            top: f32,
            bottom: f32,
            left: f32,
            right: f32,
        ) {
            unsafe { $crate::__ffi::set_safe_area_insets(h, top, bottom, left, right) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_set_keyboard_height(h: *mut ::core::ffi::c_void, pts: f32) {
            unsafe { $crate::__ffi::set_keyboard_height(h, pts) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_set_app_active(h: *mut ::core::ffi::c_void, active: bool) {
            unsafe { $crate::__ffi::set_app_active(h, active) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_poll_request(h: *mut ::core::ffi::c_void, out_kind: *mut i32) -> bool {
            unsafe { $crate::__ffi::poll_request(h, out_kind) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_request_str_a(h: *mut ::core::ffi::c_void) -> *mut ::core::ffi::c_char {
            unsafe { $crate::__ffi::request_str_a(h) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_request_str_b(h: *mut ::core::ffi::c_void) -> *mut ::core::ffi::c_char {
            unsafe { $crate::__ffi::request_str_b(h) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_request_int(h: *mut ::core::ffi::c_void) -> i32 {
            unsafe { $crate::__ffi::request_int(h) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_on_file_picked(h: *mut ::core::ffi::c_void, path: *const ::core::ffi::c_char) {
            unsafe { $crate::__ffi::on_file_picked(h, path) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_on_permission_result(h: *mut ::core::ffi::c_void, kind: i32, granted: bool) {
            unsafe { $crate::__ffi::on_permission_result(h, kind, granted) }
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_on_mic_level(h: *mut ::core::ffi::c_void, level: f32) {
            unsafe { $crate::__ffi::on_mic_level(h, level) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_register_haptic_cb(
            h: *mut ::core::ffi::c_void,
            cb: extern "C" fn(i32),
        ) {
            unsafe { $crate::__ffi::register_haptic_cb(h, cb) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_string_free(s: *mut ::core::ffi::c_char) {
            unsafe { $crate::__ffi::string_free(s) }
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn egui_ios_last_error() -> *mut ::core::ffi::c_char {
            unsafe { $crate::__ffi::last_error() }
        }
    };
}
