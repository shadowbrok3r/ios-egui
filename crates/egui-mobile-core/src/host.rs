//! Host capability bridge: app-side handle plus the request queue and pushed-in state the
//! platform runtime drains/feeds each frame. Main-thread only (`Rc`/`RefCell`, not `Send`).
//!
//! The queue + pushed-in state are platform-neutral: on iOS the Swift host drains via the C ABI
//! poll functions; on Android the runtime drains internally and calls JNI. Both go through the
//! doc-hidden `drv_*` methods at the bottom of this file.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// Haptic feedback styles (`UIImpactFeedbackGenerator` on iOS, `Vibrator`/`VibrationEffect` on Android).
#[derive(Clone, Copy, Debug)]
pub enum Haptic {
    Light = 0,
    Medium = 1,
    Heavy = 2,
    Success = 3,
    Warning = 4,
    Error = 5,
    Selection = 6,
}

/// Permission domains the app can request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Camera = 0,
    Microphone = 1,
}

/// Safe-area insets in points, pushed in by the platform host on layout.
#[derive(Clone, Copy, Debug, Default)]
pub struct Insets {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

/// A request from the app for the platform host to fulfill. Drained via the driver API.
pub enum HostRequest {
    ShareFile(String),
    Notify { title: String, body: String },
    SetKeyboard(bool),
    Haptic(Haptic),
    OpenUrl(String),
    PickFile { types: Vec<String> },
    RequestCameraPermission,
    RequestMicPermission,
    StartCameraPreview,
    StopCameraPreview,
    SetClipboard(String),
    ShareText(String),
    /// Backend-specific request: a raw (kind, strings, int) enqueued via [`Host::drv_enqueue`].
    /// Used by platform extension traits (e.g. Android `HostExt`) for capabilities beyond the
    /// common surface, without adding a typed variant per capability.
    Raw {
        kind: i32,
        a: Option<String>,
        b: Option<String>,
        i: i32,
    },
}

impl HostRequest {
    /// Integer kind read by the platform host; must match `egui_ios.h` (and the Android dispatcher).
    pub fn kind_code(&self) -> i32 {
        match self {
            HostRequest::ShareFile(_) => 0,
            HostRequest::Notify { .. } => 1,
            HostRequest::SetKeyboard(_) => 2,
            HostRequest::Haptic(_) => 3,
            HostRequest::OpenUrl(_) => 4,
            HostRequest::PickFile { .. } => 5,
            HostRequest::RequestCameraPermission => 6,
            HostRequest::RequestMicPermission => 7,
            HostRequest::StartCameraPreview => 8,
            HostRequest::StopCameraPreview => 9,
            HostRequest::SetClipboard(_) => 10,
            HostRequest::ShareText(_) => 11,
            HostRequest::Raw { kind, .. } => *kind,
        }
    }

    /// Primary string payload.
    pub fn str_a(&self) -> Option<String> {
        match self {
            HostRequest::ShareFile(p) => Some(p.clone()),
            HostRequest::Notify { title, .. } => Some(title.clone()),
            HostRequest::OpenUrl(u) => Some(u.clone()),
            HostRequest::PickFile { types } => Some(types.join("\n")),
            HostRequest::SetClipboard(t) => Some(t.clone()),
            HostRequest::ShareText(t) => Some(t.clone()),
            HostRequest::Raw { a, .. } => a.clone(),
            _ => None,
        }
    }

    /// Secondary string payload.
    pub fn str_b(&self) -> Option<String> {
        match self {
            HostRequest::Notify { body, .. } => Some(body.clone()),
            HostRequest::Raw { b, .. } => b.clone(),
            _ => None,
        }
    }

    /// Integer payload.
    pub fn int(&self) -> i32 {
        match self {
            HostRequest::SetKeyboard(v) => *v as i32,
            HostRequest::Haptic(h) => *h as i32,
            HostRequest::Raw { i, .. } => *i,
            _ => 0,
        }
    }
}

struct HostState {
    queue: VecDeque<HostRequest>,
    current: Option<HostRequest>,
    documents_dir: Option<String>,
    safe_area: Insets,
    keyboard_height: f32,
    active: bool,
    picked_file: Option<String>,
    permissions: [Option<bool>; 2],
    mic_level: f32,
    haptic_cb: Option<extern "C" fn(i32)>,
}

/// Cheap clonable handle the app calls from `update`. All methods are main-thread only.
#[derive(Clone)]
pub struct Host {
    inner: Rc<RefCell<HostState>>,
}

impl Host {
    #[doc(hidden)]
    pub fn new() -> Self {
        Host {
            inner: Rc::new(RefCell::new(HostState {
                queue: VecDeque::new(),
                current: None,
                documents_dir: None,
                safe_area: Insets::default(),
                keyboard_height: 0.0,
                active: true,
                picked_file: None,
                permissions: [None, None],
                mic_level: 0.0,
                haptic_cb: None,
            })),
        }
    }

    fn push(&self, req: HostRequest) {
        self.inner.borrow_mut().queue.push_back(req);
    }

    /// Present the system share sheet for a file on disk.
    pub fn share_file(&self, path: impl Into<String>) {
        self.push(HostRequest::ShareFile(path.into()));
    }

    /// Post a local notification.
    pub fn notify(&self, title: impl Into<String>, body: impl Into<String>) {
        self.push(HostRequest::Notify {
            title: title.into(),
            body: body.into(),
        });
    }

    /// Show or hide the soft keyboard explicitly (egui auto-shows it for focused text too).
    pub fn request_keyboard(&self, visible: bool) {
        self.push(HostRequest::SetKeyboard(visible));
    }

    /// Fire haptic feedback now if a callback is registered, else next host tick.
    pub fn haptic(&self, kind: Haptic) {
        let cb = self.inner.borrow().haptic_cb;
        if let Some(cb) = cb {
            cb(kind as i32);
        } else {
            self.push(HostRequest::Haptic(kind));
        }
    }

    /// Open a URL in the system browser.
    pub fn open_url(&self, url: impl Into<String>) {
        self.push(HostRequest::OpenUrl(url.into()));
    }

    /// Copy text to the system clipboard.
    pub fn copy_text(&self, text: impl Into<String>) {
        self.push(HostRequest::SetClipboard(text.into()));
    }

    /// Present the system share sheet for a plain-text string.
    pub fn share_text(&self, text: impl Into<String>) {
        self.push(HostRequest::ShareText(text.into()));
    }

    /// Present a document picker; the chosen path arrives via [`Host::take_picked_file`].
    pub fn pick_file(&self, types: &[&str]) {
        self.push(HostRequest::PickFile {
            types: types.iter().map(|s| s.to_string()).collect(),
        });
    }

    /// Request a permission; the result arrives via [`Host::permission`].
    pub fn request_permission(&self, perm: Permission) {
        self.push(match perm {
            Permission::Camera => HostRequest::RequestCameraPermission,
            Permission::Microphone => HostRequest::RequestMicPermission,
        });
    }

    /// Start the native camera preview layer behind the egui view.
    pub fn start_camera_preview(&self) {
        self.push(HostRequest::StartCameraPreview);
    }

    /// Stop the native camera preview layer.
    pub fn stop_camera_preview(&self) {
        self.push(HostRequest::StopCameraPreview);
    }

    /// The app's Documents directory, if the host has provided it.
    pub fn documents_dir(&self) -> Option<String> {
        self.inner.borrow().documents_dir.clone()
    }

    /// Current safe-area insets in points.
    pub fn safe_area_insets(&self) -> Insets {
        self.inner.borrow().safe_area
    }

    /// Height in points of the soft keyboard overlapping the view; 0 when hidden.
    pub fn keyboard_height(&self) -> f32 {
        self.inner.borrow().keyboard_height
    }

    /// Whether the app is in the foreground.
    pub fn is_active(&self) -> bool {
        self.inner.borrow().active
    }

    /// Take the most recently picked file path, consuming it.
    pub fn take_picked_file(&self) -> Option<String> {
        self.inner.borrow_mut().picked_file.take()
    }

    /// Last known result for a permission (None until the user responds).
    pub fn permission(&self, perm: Permission) -> Option<bool> {
        self.inner.borrow().permissions[perm as usize]
    }

    /// Latest microphone input level in 0..=1.
    pub fn mic_level(&self) -> f32 {
        self.inner.borrow().mic_level
    }
}

/// Driver API used by the per-platform runtime to drain requests and feed state. Not for apps.
#[doc(hidden)]
impl Host {
    /// Pop the next request into `current`; returns its kind code, or `None` if the queue is empty.
    pub fn drv_pop(&self) -> Option<i32> {
        let mut st = self.inner.borrow_mut();
        if let Some(req) = st.queue.pop_front() {
            let kind = req.kind_code();
            st.current = Some(req);
            Some(kind)
        } else {
            st.current = None;
            None
        }
    }

    pub fn drv_str_a(&self) -> Option<String> {
        self.inner.borrow().current.as_ref().and_then(|r| r.str_a())
    }

    pub fn drv_str_b(&self) -> Option<String> {
        self.inner.borrow().current.as_ref().and_then(|r| r.str_b())
    }

    pub fn drv_int(&self) -> i32 {
        self.inner.borrow().current.as_ref().map(|r| r.int()).unwrap_or(0)
    }

    pub fn drv_set_documents_dir(&self, path: String) {
        self.inner.borrow_mut().documents_dir = Some(path);
    }

    pub fn drv_set_safe_area(&self, top: f32, bottom: f32, left: f32, right: f32) {
        self.inner.borrow_mut().safe_area = Insets { top, bottom, left, right };
    }

    pub fn drv_set_keyboard_height(&self, pts: f32) {
        self.inner.borrow_mut().keyboard_height = pts.max(0.0);
    }

    /// Set the active flag; returns the previous value so the caller can fire on_resume/on_pause.
    pub fn drv_set_active(&self, active: bool) -> bool {
        let mut st = self.inner.borrow_mut();
        let prev = st.active;
        st.active = active;
        prev
    }

    pub fn drv_set_picked_file(&self, path: String) {
        self.inner.borrow_mut().picked_file = Some(path);
    }

    pub fn drv_set_permission(&self, index: usize, granted: bool) {
        if index < 2 {
            self.inner.borrow_mut().permissions[index] = Some(granted);
        }
    }

    pub fn drv_set_mic_level(&self, level: f32) {
        self.inner.borrow_mut().mic_level = level;
    }

    pub fn drv_register_haptic_cb(&self, cb: extern "C" fn(i32)) {
        self.inner.borrow_mut().haptic_cb = Some(cb);
    }

    /// Enqueue a backend-specific request. Used by platform extension traits (e.g. Android
    /// `HostExt`) for capabilities beyond the common surface. The backend's drain loop dispatches
    /// on `kind`.
    pub fn drv_enqueue(&self, kind: i32, a: Option<String>, b: Option<String>, i: i32) {
        self.push(HostRequest::Raw { kind, a, b, i });
    }
}
