//! Host capability bridge: app-side handle plus the request queue and pushed-in state the
//! Swift host drains/feeds each frame. Main-thread only (`Rc`/`RefCell`, not `Send`).

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

/// Haptic feedback styles (maps to `UIImpactFeedbackGenerator` / notification feedback).
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

/// Safe-area insets in points, pushed in by the Swift host on layout.
#[derive(Clone, Copy, Debug, Default)]
pub struct Insets {
    pub top: f32,
    pub bottom: f32,
    pub left: f32,
    pub right: f32,
}

/// A request from the app for the Swift host to fulfill. Drained via the FFI poll loop.
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
}

impl HostRequest {
    /// Integer kind read by `egui_ios_poll_request`; must match `egui_ios.h`.
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
        }
    }

    /// Primary string payload exposed via `egui_ios_request_str_a`.
    pub fn str_a(&self) -> Option<String> {
        match self {
            HostRequest::ShareFile(p) => Some(p.clone()),
            HostRequest::Notify { title, .. } => Some(title.clone()),
            HostRequest::OpenUrl(u) => Some(u.clone()),
            HostRequest::PickFile { types } => Some(types.join("\n")),
            HostRequest::SetClipboard(t) => Some(t.clone()),
            _ => None,
        }
    }

    /// Secondary string payload exposed via `egui_ios_request_str_b`.
    pub fn str_b(&self) -> Option<String> {
        match self {
            HostRequest::Notify { body, .. } => Some(body.clone()),
            _ => None,
        }
    }

    /// Integer payload exposed via `egui_ios_request_int`.
    pub fn int(&self) -> i32 {
        match self {
            HostRequest::SetKeyboard(v) => *v as i32,
            HostRequest::Haptic(h) => *h as i32,
            _ => 0,
        }
    }
}

pub(crate) struct HostState {
    pub queue: VecDeque<HostRequest>,
    pub current: Option<HostRequest>,
    pub documents_dir: Option<String>,
    pub safe_area: Insets,
    pub keyboard_height: f32,
    pub active: bool,
    pub picked_file: Option<String>,
    pub permissions: [Option<bool>; 2],
    pub mic_level: f32,
    pub haptic_cb: Option<extern "C" fn(i32)>,
}

/// Cheap clonable handle the app calls from `update`. All methods are main-thread only.
#[derive(Clone)]
pub struct Host {
    pub(crate) inner: Rc<RefCell<HostState>>,
}

impl Host {
    pub(crate) fn new() -> Self {
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

    /// Present a document picker; the chosen path arrives via [`Host::take_picked_file`].
    pub fn pick_file(&self, uti_types: &[&str]) {
        self.push(HostRequest::PickFile {
            types: uti_types.iter().map(|s| s.to_string()).collect(),
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
