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
- **Per-model / per-LoRA defaults** — the server catalog's `recommended` numbers are scraped out of
  Civitai text and are routinely wrong for a given model. **Edit defaults** on any model row (and
  **Defaults** on any active LoRA card) opens an editor where each field is either *catalog* or
  *yours*: tick a field to pin steps, CFG, sampler, scheduler, CLIP skip or size, and for a model
  needing them, the text encoder(s), encoder type, VAE and weight dtype. **Copy current settings**
  pins everything the Create tab is showing in one tap; **Clear all** goes back to the catalog. Only
  ticked fields are stored, so untouched ones keep improving when the catalog refreshes. Overrides
  are local, keyed by filename, and ride along in the encrypted Settings backup. A model with any
  override is marked `• custom` in the picker, and its Details pane gains a **Yours** line.
- **Three loader topologies.** A checkpoint carries MODEL + CLIP + VAE in one file; a diffusion
  model under `models/diffusion_models` loads through `UNETLoader` with a separate encoder and VAE.
  The third case is a bare diffusion model (Anima) filed under `models/checkpoints`: ComfyUI lists
  it only under `CheckpointLoaderSimple.ckpt_name`, and loading it there yields a null CLIP and a
  null VAE — `CLIPTextEncode` then fails with `clip input is invalid: None`. **Checkpoint + separate
  CLIP/VAE** takes MODEL off the checkpoint loader and wires the encoder and VAE from their own
  loaders. Anima files in the checkpoints list pick it automatically (matched on whole tokens, so
  `animagine` and `animatediff` are untouched); the Loader radio in the defaults editor overrides
  that either way. When a run fails on a null clip anyway, the error dialog offers the switch as a
  one-tap fix and pins it as that model's default.
- **Prompt rewriting** — Expand / Variations / Rewrite live behind a single ☰ **Rewrite** menu in
  the prompt's own header row, beside the chips toggle and the history arrows. (Those arrows are on
  *both* prompt fields and drive one shared scrubber — a history entry is a positive/negative pair,
  so stepping from either field moves both.) ☁ marks the server engine, so a rewrite is never
  ambiguous about where it came from. A spinner sits beside the menu
  button while any of them is running, since a closed popup can't show its own:
  - **☁ Expand** streams comfy-gate's `POST /api/expand` and renders the rewrite as a live diff to
    accept or discard. The request carries a `dialect` so the text comes back in the family's own
    idiom: `wan-i2v` / `wan-t2v` for video, and for images the catalog family (Illustrious, Pony,
    Flux…) or, failing that, the loader filename for the gate to classify.
  - **✨ Rewrite** runs the local Qwen rewrite pack on the phone's CPU (feature `local-npu`), with
    the target style picked from its menu — no server needed.
  - **⭐ Variations** (comfy-gate only) asks `POST /api/variations` for alternatives instead of a
    faithful rewrite: each option changes exactly one axis (setting, pose, lighting, wardrobe,
    mood, composition) and is shown with that label as a diff against the original, so what
    changed is visible before you pick. Strength (subtle/moderate/wild), count (1–6) and a **Keep
    unchanged** list are in the window — *Again* re-rolls with the new knobs, *Keep mine* walks
    away. Keep is seeded from the applied character's injected tags, so a variation can't quietly
    restyle the character; the server also anchors anything the prompt weights, like
    `(pink hair:1.2)`. Partial results are rendered with a note about what failed.
  - **Settings → Prompt rewriting** chooses between them: *Auto* offers whichever can run (both,
    when both can), *comfy-gate* or *On device* force one and say why if it can't run. A server
    that answers 404/501/503 is remembered for the session, so Auto stops offering it. (For
    Variations, only 404/503 hide the button — a 501 means just *this* family has no variations
    prompt.)
  - Video prompts are *also* rewritten automatically by comfy-gate at queue time whichever engine
    you use, so accepting a server rewrite turns **Raw** on (a `raw:` marker) to stop it being
    rewritten twice; image prompts are never touched at queue time.
- **Create output** — finished images go to the Gallery rather than a panel on the Create tab. The
  Gallery nav icon carries a count of images you haven't opened yet, and each of those tiles gets an
  aqua dot in its top-left corner until you open it full-screen; the count and the dots read the
  same set, so they never disagree. The count is seeded from an mtime watermark, so an existing
  library doesn't light up as new the first time this runs. Create itself keeps only the live
  sampler preview and its status / warning notes, inline at the top of the pane.
- **Offline gallery** — with no server reachable, the Gallery falls back to whatever is already in
  the on-device full-image cache instead of an empty "connect to a server" screen. The cache is
  self-describing (every file has a `.key` sidecar naming its `subfolder/filename`), so the listing
  is rebuilt from disk, newest-first. Full images come from the cache, and a workflow is recovered
  from the file's **own bytes** — the PNG's embedded graph, or a video container's — so Copy
  workflow, Open workflow and Remix all still work with the server down. Editing does not: deleting
  offline would tombstone rows locally for the tombstone's whole TTL while the server still held
  every file, so it refuses rather than pretending. Reconnecting drops the cache-built listing and
  re-reads the real one.
- **LoRA library** — Create → LoRAs has an **Active / Library** sub-tab. Library is the server's
  whole installed set, served by comfy-gate's `/comfyui-android/lora/*` (see that repo's
  `HANDOFF-android-lora-manager.md`): a virtualized preview grid, search, and folder / base-model /
  tag filter chips counted over the whole library. Everything keys on the model's **sha256**, which
  is the only id that survives a move or rename server-side — no path ever crosses the wire. The
  gate transcodes video previews to JPEG with ffmpeg, so the third of a typical library whose
  preview is an `.mp4` still shows a thumbnail; a `204` latches per id so a previewless tile never
  re-asks. Tapping one opens a detail view — trigger words (tap to append to the prompt), tags,
  notes, Civitai description and link. Previews the server flagged as mature are covered until you
  opt in.
  - **Get** pulls a model straight off Civitai (`/comfyui-android/download/*`), for LoRAs,
    checkpoints or embeddings. Paste a model link, a version link or a bare id; already-installed
    versions are greyed out and early-access ones warn before they 401. The **root** picker is
    deliberately not hidden for checkpoints — they have three (`checkpoints` / `diffusion_models` /
    `unet`) and the wrong one hides the file from every loader.
  - The transfer runs **server-side**, so it survives the app closing, the screen locking or the
    connection dropping; the app persists the download id and re-attaches on launch. A download
    that never leaves `pending` is timed out client-side, because the gate cannot tell "queued, no
    bytes yet" from "no such download".
  - Downloads are **admin-only** on the gate. A non-admin gets no Get button rather than a 403.
- **App updates** — Settings → App updates checks comfy-gate for a newer build and installs it.
  The APK is streamed to app-private storage, verified against the sha256 the gate recorded at
  publish time, and handed to Android's `PackageInstaller`; the system shows its own confirm
  dialog. Android additionally needs a per-app "install unknown apps" grant, which the section
  offers a button for. Publish a build with `scripts/publish-apk.sh` from the repo root. An update
  only appears when its `versionCode` is **greater** than the installed one, and that derives from
  `version` in this crate's `Cargo.toml` — bump it before a release build.
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
  - **File → Open in Create** reads the open tab's nodes back into the Create fields — the mirror of
    Create's "Open as graph". The graph is converted to API form first (so bypassed nodes are
    spliced out and SetNode/GetNode and subgraphs are resolved), then scraped by the same reader
    Remix uses on a gallery image's embedded workflow: model, encoders/VAE, LoRAs, both prompts,
    sampler/scheduler/steps/cfg and the seed. Companions are re-pointed at this server's installed
    files. Anything Create can't hold stays in the tab, which is left untouched and still queueable
    from the Graph tab.
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
- **Global progress bar** — while anything runs, a bar with percentage slides in at the very top
  of the screen, above every tab's own header. The engine keeps its own authenticated WebSocket to `/ws` (rucomfyui's ws transport
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

Bare `cargo apk2` also works after loading the same env the wrapper injects (includes
`KOTLIN_HOME` — required by cargo-apk2, unset by default on Manjaro):

```bash
eval "$(cargo egui-mobile env -a)"
cargo apk2 run --target aarch64-linux-android --release --features tls
```

The repo's `.cargo/config.toml` exports `ANDROID_HOME` / `ANDROID_NDK_ROOT` and the NDK compilers
so cross `cargo check` resolves, but it does **not** set `KOTLIN_HOME` or fix JDK 8 on `PATH` —
use `cargo egui-mobile` or the `eval` line above.

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
cargo egui-mobile run -a --release --features tls
# or: eval "$(cargo egui-mobile env -a)" && cargo apk2 run --target aarch64-linux-android --features tls
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
