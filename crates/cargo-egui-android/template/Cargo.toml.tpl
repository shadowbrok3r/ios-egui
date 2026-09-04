[package]
name = "{{project_name}}"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
# Git (default); use a local path for development against a clone:
#   egui-mobile = { path = "../EguiMobile/crates/egui-mobile" }
egui-mobile = { git = "https://github.com/shadowbrok3r/ios-egui" }
egui = "0.36"

# cargo-apk2 reads this to generate the APK manifest.
[package.metadata.android]
package = "{{package_id}}"
# egui-android's Java bridge (real InputConnection for Gboard). Copied on `new` and re-synced
# from the resolved egui-android by every `cargo egui-android build`/`run`.
java_sources = "java"

[package.metadata.android.sdk]
min_sdk_version = 26
target_sdk_version = 35

[package.metadata.android.application]
label = "{{display_name}}"
has_code = true

# EguiNativeActivity hosts a hidden EditText for real IME InputConnection (spacebar cursor).
[[package.metadata.android.application.activity]]
name = "com.github.egui_mobile.EguiNativeActivity"
config_changes = "orientation|keyboardHidden|screenSize|screenLayout|density"
window_soft_input_mode = "adjustResize"
[[package.metadata.android.application.activity.meta_data]]
name = "android.app.lib_name"
value = "{{project_name}}"
[[package.metadata.android.application.activity.intent_filter]]
actions = ["android.intent.action.MAIN"]
categories = ["android.intent.category.LAUNCHER"]
