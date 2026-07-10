[package]
name = "{{project_name}}"
version = "0.1.0"
edition = "2024"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
# Git (default); use a local path for development against a clone:
#   egui-mobile = { path = "../ios-egui/crates/egui-mobile" }
egui-mobile = { git = "https://github.com/shadowbrok3r/egui-ios" }
egui = "0.35"

# cargo-apk2 reads this to generate the APK manifest.
[package.metadata.android]
package = "{{package_id}}"

[package.metadata.android.sdk]
min_sdk_version = 26
target_sdk_version = 35

[package.metadata.android.application]
label = "{{display_name}}"

# The launcher activity (NativeActivity loads lib{{project_name}}.so via android-activity).
[[package.metadata.android.application.activity]]
name = "android.app.NativeActivity"
config_changes = "orientation|keyboardHidden|screenSize|screenLayout|density"
[[package.metadata.android.application.activity.meta_data]]
name = "android.app.lib_name"
value = "{{project_name}}"
[[package.metadata.android.application.activity.intent_filter]]
actions = ["android.intent.action.MAIN"]
categories = ["android.intent.category.LAUNCHER"]
