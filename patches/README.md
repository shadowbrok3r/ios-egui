# Patched crates

## `android-activity`

Vendored from crates.io `0.6.1`.

**Changes:**
- `hide_soft_input(hide_implicit_only: true)` is a no-op
- `show_soft_input(show_implicit: true)` is a no-op

winit/egui-winit call both on every IME interrupt / `allow_ime` toggle (DecorView).
With `EguiNativeActivity`'s hidden EditText as the served IME view, DecorView
`showSoftInput` is ignored ("view is not served") while the matching
`hideSoftInput(HIDE_IMPLICIT_ONLY)` still dismisses the keyboard — so the keyboard
dies until a slow recovery re-show. Skipping both implicit paths leaves show/hide
to the egui-android EditText bridge. Explicit (`*_implicit*: false`) paths are
unchanged for fallbacks.

Wired via workspace `[patch.crates-io]` in the root `Cargo.toml`.

## `ndk-build2` / `cargo-apk2`

Vendored from crates.io `1.3.11`.

**Changes (`ndk-build2`):**
- `Application.uses_native_library: Vec<NativeLibrary>` serializes
  `<uses-native-library android:name="..." android:required="..."/>`.

Needed so apps can declare vendor FastRPC (`libcdsprpc.so`) for QNN HTP unsigned
PD on targetSdk 31+. Upstream cargo-apk2/ndk-build2 had no metadata key for this.

`cargo-apk2` is an installed binary (`cargo apk2`), so the workspace patch table
does not affect packaging. Point its `ndk-build2` dep at this tree and reinstall:

```sh
cargo install --path patches/cargo-apk2 --force
```

Then in app `Cargo.toml`:

```toml
[[package.metadata.android.application.uses_native_library]]
name = "libcdsprpc.so"
required = false
```
