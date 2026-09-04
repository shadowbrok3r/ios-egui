# Working in this repo

## The apps moved out

Two apps that grew up as examples here now live in their own repos and consume this one as **git
dependencies** — `egui-mobile`, the `local-*` model crates, `backdrop-blur-egui`, and the vendored
forks under `patches/` through their own `[patch.crates-io]` tables:

- **comfyui-android** → `~/Documents/Rust/Mobile/comfyui-android`
  (github.com/shadowbrok3r/comfyui-android, private). Its release procedure — comfy-gate
  self-update plus the app store — moved with it; see that repo's CLAUDE.md.
- **ringdesigner-android** → `crates/ringdesigner-android` in the RingDesigner repo
  (`~/Documents/Rust/JewelryProjects/RingDesigner`).

What that means for work here:

- A change to a framework crate reaches an app only after it is **pushed** and the app runs
  `cargo update -p egui-mobile`. When a `HostExt`/`egui-android` addition is for one of those
  apps, say so in the summary — the app-side pin is a separate step, in a separate repo.
- **Java** (`crates/egui-android/java`) is copied into each app's `java/` by
  `cargo egui-mobile build`/`run` from the egui-android the app resolves, so a Java change
  propagates on the app's next build — once the app has moved its pin.
- The `[patch.crates-io]` table here is inherited by nothing outside this workspace; a new
  vendored fork needs a matching entry in each app repo.
- `cargo egui-mobile` is installed from this checkout (`cargo install --path
  crates/cargo-egui-mobile`); the apps run that binary, so reinstall after changing the wrapper.

## Releasing the in-repo apps

`examples/appstore-android`, `examples/plugins-android` and `examples/privaxy-android` publish to
the app store from CI (`.github/workflows/publish-appstore.yml`) on a push that bumps the crate
`version`; PUBLISHING.md has the manual route and the versionCode rule. The signing cert must stay
`e1ac3c3b9e0720cbce70b272c9d940a58162cb2d703967c76c31bc715c8040f1` (`~/.android/debug.keystore`):
a different key makes every update fail `INSTALL_FAILED_UPDATE_INCOMPATIBLE`, and the only fix is
uninstalling the app, which wipes its settings. Both of Logan's build machines share this keystore.

Build APKs from the crate dir, not the workspace root — cargo-apk2 fails on the virtual manifest
("virtual manifests must be configured with [workspace]"). The APK lands in the **workspace**
`target/release/apk/`.

## Testing Android changes

Every app's `mod app` is `#[cfg(target_os = "android")]`, so **host `cargo check` never compiles
the UI**. Always verify from the app's crate dir:

```bash
cd examples/<app>
ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/28.0.12674087 cargo ndk -t arm64-v8a check -p <crate>
```

Compare the warning count against the pre-change baseline rather than only looking for errors —
new dead code means something was left unwired. Host-side logic is covered by
`cargo test -p <crate>` and should stay green.

**Java changes** (`crates/egui-android/java/`) are otherwise only compiled at APK-package time.
Check them in a second rather than at the end of a build:

```bash
cd crates/egui-android/java
/usr/lib/jvm/java-17-openjdk/bin/javac -Xlint:deprecation --release 17 \
  -classpath ~/Android/Sdk/platforms/android-35/android.jar -d /tmp/javacheck com/github/egui_mobile/*.java
```

## The AI toolchain

The QNN/QAIRT SDK, conversion venv and host runtime live under `~/Documents/Ai/QNN/`
(`scripts/qnn-env.sh` defaults there), the Anima packs under `~/Documents/Ai/Anima/`, and the CLIP
and WD14 exports under `~/Documents/Ai/{clip,clip-build,wd14,wd14-build}/`. The converters
(`scripts/wd14-export`, `scripts/clip-export`, `scripts/rewriter-fetch`, `scripts/qnn-convert.sh`)
stay here beside the `local-*` crates they feed; `scripts/qnn-stage-libs.sh <app-dir>` stages the
runtime `.so` an app bundles.

## House style

- **Never run `cargo fmt`.** The repo uses a compact hand style; formatting it would produce a
  diff over the whole tree.
- Comments explain *why*, not *what* — match the density and voice of the surrounding code.
- comfy-gate is a **separate repo** (`~/Desktop/comfy-gate`) deployed to the ComfyUI box; its
  `HANDOFF-android-*.md` files are the contract comfyui-android codes against.
