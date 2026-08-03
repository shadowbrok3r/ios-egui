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

## `egui_json_tree`

Vendored from crates.io `0.16.0`.

**Changes:**
- `render_job` senses `Sense::click()` instead of `Sense::click_and_drag()` on a touch screen.

Upstream gives every key, value, bracket and separator an explicit
`Sense::click_and_drag()` (`src/render.rs`, the only `Sense` in the crate). `ScrollArea`
registers its own drag widget *before* the content, and egui breaks a hit-test tie in favour of
the last-registered widget (`hit_test.rs:430-434`), so every token outranked the scroll area and
a finger drag over a JSON body scrolled nothing at all — only the scroll bar worked.
`interaction.selectable_labels = false` does not help: an explicit `.sense()` wins outright
(`label.rs:145`).

The gate mirrors egui's own mitigation for label selection (`label.rs:159-166`), which downgrades
to `Sense::click()` on touch to prioritise scrolling. Clicks are preserved, so tapping a bracket
still toggles it, and the mouse keeps drag behaviour.

Wired as a path dependency from `examples/privaxy-android`, not `[patch.crates-io]` — nothing
else in the workspace uses it.
