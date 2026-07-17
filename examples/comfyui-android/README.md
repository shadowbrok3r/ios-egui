# comfyui-android

A streamlined **ComfyUI client for Android**, built on `egui-mobile` and
[`rucomfyui`](https://github.com/philpax/rucomfyui). A bottom navigation bar of five tabs —
**Create**, **Graph** (with a Properties sub-tab), **Gallery** (with an Albums sub-tab),
**Settings**, **Logs**. One `impl EguiApp`, pure Rust, no Kotlin. Talks to a plain ComfyUI server
or one fronted by [comfy-gate](https://github.com/shadowbroker/comfy-gate) (API-key or account
sign-in, per-user gallery + albums, server-side model filtering).

## What it does

- **Connect** to a server by URL (`http://192.168.x.x:8188`, or a Tailscale `100.x` address).
  On connect it fetches `/object_info` raw and parses it with the app's own **lenient schema
  layer** (`src/schema.rs`): every node type parses independently, so nonconforming custom nodes
  (numeric enum options, bare-string input specs, missing `required` bundles, rgthree array
  outputs…) degrade gracefully instead of failing the whole catalog the way rucomfyui's typed
  `ObjectInfo` does. The parsed `SchemaSet` populates the checkpoint / sampler / scheduler menus
  and bridges into the typed `ObjectInfo` the node graph editor needs (`schema::to_object_info`).
- **Auto-connect**: with a saved server URL, the app reconnects on launch. The server URL, API
  key, and generation params **auto-save within a second of changing** (no successful connect
  required), so they survive app kills, reinstalls, and new deploys.
- **Generate tab** — Text → Image and Image → Image (current result or an image URL as input,
  denoise slider), checkpoint/sampler/scheduler menus, steps, CFG, size, seed with random toggle,
  progress + preview, Save to `<app files>/comfyui/`.
- **Graph tab** — a full node editor (`rucomfyui_node_graph`, egui-snarl) over the server's real
  node catalog:
  - **Workflows** lists the server's saved workflows (`/userdata?dir=workflows`); tap one to open
    it on the canvas. UI-format files are converted through `src/uiwf.rs`, which flattens
    **subgraphs** (recursive, with promoted/proxy widgets), resolves **Reroute** chains, KJNodes
    **SetNode/GetNode** invisible wires, mode-4 **bypass** splicing, legacy **PrimitiveNode**
    inlining, and `control_after_generate` phantom values — with per-node warnings (in Logs)
    instead of hard failures when something is off.
  - Loading a workflow **auto-arranges** it: nodes re-lay out into compact depth columns using
    their measured sizes (no overlaps), then the whole graph fits to the screen. **Arrange**
    re-compacts any time, **Fit** re-fits (double-tapping empty canvas does the same), **Start**
    pans to the workflow's first node (leftmost node with no incoming wires), and a **minimap**
    in the corner shows every node plus the current viewport — tap or drag it to jump anywhere.
  - **Find** searches the loaded workflow by node title/type and jumps to the match.
  - **Save** writes the graph back to the server as a UI-format workflow file
    (`POST /userdata/workflows%2F<name>?overwrite=true`) — keep the name to overwrite, change
    it to save a copy. Positions survive, so it round-trips with the website.
  - The **minimap** sits in the canvas's top-left corner; a **floating lock** in the top-right
    toggles view-only mode. When locked, pan/zoom/tap-to-inspect still work but nodes can't be
    moved, rewired, edited, added, or deleted — and a drag that **starts on a node** pans the
    canvas rather than doing nothing, so panning never depends on finding empty space. Tapped
    nodes get a blue focus border and can still be edited deliberately from the Properties sub-tab.
  - The canvas's controls (**File / Edit / View** menus and **Queue**) live in a bottom bar; the
    menus open upward so they never cover the navigation. **Properties** is a sub-tab beside the
    **Canvas** sub-tab, and the loaded workflow's name stays pinned at the top.
  - **Add node** searches all node types and drops the pick at the current view center;
    long-pressing the canvas opens the category menu. Drag between pins to wire, long-press a
    node to delete.
  - **Queue** converts the canvas back to API format and runs it; the executing node is
    highlighted and output images land on the nodes that produced them.
- **Properties tab** — inspects the node last tapped in the Graph tab (or picked via Find):
  type/category/description, every input (connection source, or an editable value widget), and
  outputs. **Show in graph** jumps back to the node on the canvas. Values stay editable here
  even in View only mode.
- **Global progress bar** — while anything runs, a bar with percentage sits under the tab row on
  every tab. The engine keeps its own authenticated WebSocket to `/ws` (rucomfyui's ws transport
  can't send auth headers, so we run our own alongside the polling execution), which supplies
  real per-step sampler progress, the executing-node highlight, and live previews; if the socket
  can't connect, the bar falls back to executed-node count and everything else still works.
- **Resilient execution** — all HTTP calls carry connect/read timeouts (a wedged server errors
  instead of hanging a spinner forever), and if the execution event stream drops mid-run the app
  quietly polls `/history` until the prompt finishes instead of declaring failure while the
  server keeps generating.
- **Gallery tab** — the server's output browser (`/gallery/api/*`), with an **Images** and an
  **Albums** sub-tab. Controls sit in a bottom bar: **search**, a **model** filter (from
  `/gallery/api/facets`, applied server-side), **sort** (newest/oldest/size/name), **grouping**
  (folder/model/none) into collapsible headers, and a **1–3 column** toggle. At one column tiles
  render near-full-resolution at each image's own aspect ratio (a full-width vertical scroll);
  thumbnails are fetched at a size that matches the column count and cached against a memory
  budget. Tap an image for a full-screen viewer with a scrollable **filmstrip** of the rest of the
  listing along the bottom; from there **Save**, **Use as input** (img2img), **Open workflow**, or
  add/remove it from any **album** (membership shown with a check). The **Albums** sub-tab creates,
  renames, deletes, and selects the album the Images pane filters to. Albums and model facets are
  per-account, so they follow the signed-in credential.
  - **Multi-select**: tap **Select** (or long-press any photo) to enter selection mode; then tap to
    toggle, or **long-press and drag in one swipe** to paint a selection across many tiles (scroll is
    suspended for that swipe). With a selection, **Add to album** (batch) or **Delete** them — delete
    is comfy-gate's soft delete (moved to `<user>/.trash/`, recoverable), not a hard unlink.
- **Logs tab**: every request, response status/size, parse count, and engine event is logged
  in-app with **Copy all** (system clipboard) and **Share** (share sheet), and mirrored to logcat.
- **Settings tab** — server URL, API key, and account. **Sign in** with a comfy-gate username and
  password (`POST /login`, an HTML form flow whose only success signal is a `Set-Cookie:
  cg_session=…` — both a good and a bad password return a 303, so the app disables redirects and
  reads the cookie). The session cookie is remembered and sent alongside any API key; the password
  is never persisted. An API key alone also works — both authenticate as the same user.
- **API key** (optional): if the server is auth-gated, the key is sent on every HTTP request as
  both `X-Api-Key: <key>` and `Authorization: Bearer <key>`, and only ever to the connected
  server's own origin. Every request also sends `Accept: application/json` so an expired credential
  returns a 401 instead of comfy-gate's HTML login redirect.

The engine runs on a background tokio runtime; the UI thread only sends parameters and receives
progress/images over a channel, so the render loop never blocks on the network.

## Build & run

The repo's own wrapper is the intended entry point — it locates the SDK/NDK and a JDK 17–21 and
prepends the right `java` to `PATH` (the system default is often JDK 8, which breaks cargo-apk2's
`d8`), so it works from a plain shell. Run it from this directory:

```bash
cd examples/comfyui-android
cargo egui-mobile build -a --release --features tls   # cross-compile + package the APK
cargo egui-mobile run   -a --release --features tls   # …then install + launch on a device
```

`--features` passes straight through to the app crate; drop it for the http-only build. Compile-check
for the device target (no NDK link, fast):

```bash
cargo ndk -t arm64-v8a check -p comfyui_android --features tls
```

Bare `cargo apk2 run -p comfyui_android` also works once the environment is set. The repo's
`.cargo/config.toml` exports `ANDROID_HOME` / `ANDROID_NDK_ROOT` and the NDK compilers so cross
`cargo check` resolves, but cargo-apk2's dex step still needs a JDK 17–21 ahead of the system JDK 8
on `PATH` — which is exactly what `cargo egui-mobile` handles for you.

## Debugging & testing

The crate splits into a **portable core** (`engine`, `schema`, `uiwf`, `types`, `workflow`) and
the Android-only UI shell (`app.rs`), so the core compiles and tests **on the host**:

```bash
# Unit tests, including the pathological custom-node shapes:
cargo test -p comfyui_android

# Full-catalog + real-workflow tests against a live server:
curl -H "X-Api-Key: $KEY" http://<server>/object_info > /tmp/object_info.json
curl -H "X-Api-Key: $KEY" "http://<server>/userdata/workflows%2FMy.json" > /tmp/wf.json
OBJECT_INFO_JSON=/tmp/object_info.json WORKFLOW_UI_JSON=/tmp/wf.json \
  cargo test -p comfyui_android -- --nocapture   # WORKFLOW_UI_JSON is colon-separated
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
- **Cancel** aborts the local job; the server may keep finishing the prompt it already started.
- **Execution results come from polling** (`rucomfyui` ws feature off — it can't attach auth
  headers to its handshake). Live progress/previews come from the app's own authenticated `/ws`
  listener instead; if that socket is unreachable, runs still complete via polling, just without
  the live extras.
- **Wireless-link extensions**: "Anything Everywhere"-style nodes broadcast values without wires
  in the web frontend; the converter keeps the nodes but can't reconstruct the invisible feeds, so
  consumers relying on them fail server-side validation with a "missing input" error.
- **Gallery videos** show their thumbnails but don't play (no video decoder in the app).
