# comfyui-android

A streamlined **ComfyUI client for Android**, built on `egui-mobile` and
[`rucomfyui`](https://github.com/philpax/rucomfyui). Connect to a ComfyUI server, pick a
checkpoint, write prompts, tune the core sampler params, and generate. One `impl EguiApp`,
pure Rust, no Kotlin.

## What it does

- **Connect** to a server by URL (`http://192.168.x.x:8188`, or a Tailscale `100.x` address).
  On connect it fetches `/object_info` raw and parses it with the app's own **lenient schema
  layer** (`src/schema.rs`): every node type parses independently, so nonconforming custom nodes
  (numeric enum options, bare-string input specs, missing `required` bundles, rgthree array
  outputs…) degrade gracefully instead of failing the whole catalog the way rucomfyui's typed
  `ObjectInfo` does. The parsed `SchemaSet` populates the checkpoint / sampler / scheduler menus
  and is the foundation for procedural node UIs.
- **Auto-connect**: with a saved server URL, the app reconnects on launch.
- **Logs tab**: every request, response status/size, parse count, and engine event is logged
  in-app with **Copy all** (system clipboard) and **Share** (share sheet), and mirrored to logcat.
- **API key** (optional): if the server is auth-gated, the key is sent on every HTTP request as
  both `X-Api-Key: <key>` and `Authorization: Bearer <key>`. It's stored (app-private) and persists.
- **Text → Image**: checkpoint, positive/negative prompt, steps, CFG, size, sampler, scheduler,
  seed (with a "random each run" toggle).
- **Image → Image**: reuse the current result, or fetch an input image by URL, plus a denoise
  slider. (Android's runtime has no file/gallery picker yet, so those are the two input sources —
  see *Limitations*.)
- **Live progress**: a progress bar driven by KSampler steps and streamed preview frames.
- **Save**: writes the finished PNG to `<app files>/comfyui/` and fires a notification when done.

The generation runs on a background tokio runtime; the UI thread only sends parameters and receives
progress/images over a channel, so the render loop never blocks on the network.

## Build & run

Compile-check for the device target (no NDK link needed, fast):

```bash
cargo ndk -t arm64-v8a check -p comfyui_android
```

Build + install + launch on a connected device (uses the `[package.metadata.android]` in
`Cargo.toml`; release is signed with the debug keystore for sideloads):

```bash
cargo apk2 run -p comfyui_android            # debug
cargo apk2 run -p comfyui_android --release  # signed sideload
```

Requires `ANDROID_NDK_HOME` and an SDK under `~/Android/Sdk` (see the repo README's Android setup).

## Debugging & testing

The crate splits into a **portable core** (`engine`, `schema`, `types`, `workflow`) and the
Android-only UI shell (`app.rs`), so the core compiles and tests **on the host**:

```bash
# Unit tests, including the pathological custom-node shapes:
cargo test -p comfyui_android

# Full-catalog test against a real server dump:
curl -H "X-Api-Key: $KEY" http://<server>/object_info > /tmp/object_info.json
OBJECT_INFO_JSON=/tmp/object_info.json cargo test -p comfyui_android -- --nocapture
```

On-device, everything in the Logs tab also mirrors to logcat (android_logger tags by module path):

```bash
adb logcat -s comfyui_android::logger      # live app log
adb logcat -d | grep comfyui_android       # everything, after the fact
```

Saved settings live at `files/comfyui_settings.json` (debug builds:
`adb shell run-as com.example.comfyui cat files/comfyui_settings.json`).

## Server URL and path prefix

rucomfyui calls `{server}/object_info`, `{server}/prompt`, `{server}/history/<id>`, `{server}/view`,
etc. If your server (or its auth proxy) serves the API under a prefix like `/api`, include it:
enter `https://comfy.example.com/api`, not just `https://comfy.example.com`.

## HTTP vs HTTPS

The default build is **http-only** — reqwest ships with no TLS backend, so it cross-compiles
cleanly and covers a LAN server or a Tailscale overlay (the framework's normal remote pattern).

For a server reached directly over **`https://`**, build with the `tls` feature:

```bash
cargo ndk -t arm64-v8a check -p comfyui_android --features tls
cargo apk2 run -p comfyui_android --features tls
```

This uses **rustls with a bundled webpki-roots CA set and the ring provider** — deliberately *not*
reqwest's `rustls` feature, which forces `rustls-platform-verifier` (that needs Android Java helper
classes this pure-native APK doesn't bundle, and panics uninitialized). ring cross-compiles cleanly
with the NDK; standard public CAs verify with no device trust-store integration.

## Limitations

- **img2img input** is limited to the current result or an image URL. The Android host runtime
  (`egui-android`) does not yet handle `pick_file` (no gallery/document picker), so an arbitrary
  local photo can't be selected. Adding it is an `egui-android` change (an activity-result flow).
- **Sharing images**: `host.share_file` is stubbed on Android (needs a `FileProvider`), so results
  are saved to the app's private files dir rather than shared. Saving to the system gallery
  (MediaStore) is a follow-up.
- **Cancel** aborts the local job; the server may finish the prompt it already started.
- **Progress is polled, not streamed**: the WebSocket transport is compiled out (`rucomfyui`
  default features off). rucomfyui can't attach the auth header to the ws handshake, and an
  auth-gated server rejects it, so progress comes from polling `/history`: you get
  start → (spinner) → final image, without a live per-step bar or preview frames.
