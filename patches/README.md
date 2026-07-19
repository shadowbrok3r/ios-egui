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