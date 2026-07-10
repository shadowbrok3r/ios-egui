//! Android backend for the shared [`egui_mobile_core`] runtime. Implement [`EguiApp`] and invoke
//! [`app!`]; the macro emits `android_main`. The render loop is driven by `eframe` (winit + wgpu,
//! Vulkan/GL) which handles the Android surface-recreation-on-resume dance and input/IME; the
//! `Host` capability bridge is threaded through and (in the JNI layer) drained to Android APIs.

pub use android_activity::AndroidApp;
pub use egui;
pub use egui_mobile_core::{CreateContext, EguiApp, Haptic, Host, Insets, Permission};

/// Adapts an [`EguiApp`] + [`Host`] to `eframe::App`. Each frame it opens a central panel, hands
/// the root `ui` to the app, then drains queued host requests (JNI dispatch lives in `host`).
struct Adapter {
    app: Box<dyn EguiApp>,
    host: Host,
    started: bool,
}

impl eframe::App for Adapter {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.started {
            self.started = true;
            self.app.on_start(ui.ctx(), &self.host);
        }
        // Feed Android WindowInsets (status bar / camera cutout / nav bar) into the host, then
        // inset the UI so app content isn't drawn under the system bars (Android 15 is edge-to-edge
        // by default). Apps can still read `host.safe_area_insets()` for finer control.
        crate::host::update_insets(&self.host, ui.ctx().pixels_per_point());
        let insets = self.host.safe_area_insets();
        let mut rect = ui.max_rect();
        rect.min.x += insets.left;
        rect.min.y += insets.top;
        rect.max.x -= insets.right;
        rect.max.y -= insets.bottom;
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            self.app.update(ui, &self.host);
        });
        crate::host::drain(&self.host);
    }
}

/// Entry point invoked by [`app!`]. Boots logging, installs a panic logger, and runs eframe with
/// the Android app handle and the wgpu renderer.
pub fn run(app: AndroidApp, mut factory: impl FnMut(&CreateContext) -> Box<dyn EguiApp> + 'static) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    std::panic::set_hook(Box::new(|info| {
        log::error!("egui-android panic: {info}");
    }));

    let mut options = eframe::NativeOptions::default();
    options.android_app = Some(app);
    options.renderer = eframe::Renderer::Wgpu;

    let result = eframe::run_native(
        "egui-android",
        options,
        Box::new(move |cc| {
            let cx = CreateContext {
                width_px: 0,
                height_px: 0,
                pixels_per_point: cc.egui_ctx.pixels_per_point(),
            };
            let app = factory(&cx);
            Ok(Box::new(Adapter {
                app,
                host: Host::new(),
                started: false,
            }))
        }),
    );
    if let Err(e) = result {
        log::error!("egui-android run_native failed: {e}");
    }
}

pub mod host;
pub use host::HostExt;

/// Generates `android_main` for a type implementing [`EguiApp`].
///
/// `factory` is any `Fn(&CreateContext) -> impl EguiApp`, e.g. `app!(MyApp::new)`.
#[macro_export]
macro_rules! app {
    ($factory:path) => {
        #[unsafe(no_mangle)]
        fn android_main(app: $crate::AndroidApp) {
            $crate::run(app, |cc| ::std::boxed::Box::new($factory(cc)));
        }
    };
}
