# Working in this repo

## Releasing the Android app (comfyui-android)

The phone app self-updates from comfy-gate, and the same APK is also listed in the app store.
**Any time you land a user-visible change to `examples/comfyui-android`, offer to cut a release** —
bump the version, build, publish to both. Logan expects the agent to drive this; he shouldn't have
to remember the steps.

```bash
# 1. Bump `version` in examples/comfyui-android/Cargo.toml  (see "the version IS the release" below)
#    …and add a CHANGELOG.md entry; publish-appstore.sh will otherwise prepend the bare one-liner.

# 2. Build. MUST run from the crate dir — from the workspace root cargo-apk2 fails with
#    "virtual manifests must be configured with [workspace]".
cd examples/comfyui-android
export ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/28.0.12674087
export ANDROID_HOME=$HOME/Android/Sdk
cargo egui-mobile build -a --release --features tls,local-npu
cd ../..

# 3. Publish to comfy-gate — the phone's Settings -> App updates route.
#    Both vars must be EXPORTED; the script has no ~/.zshrc fallback (an older local draft did,
#    and its wording survived here long after the upstream script replaced it).
#    Repeat per gate instance — there is more than one, each with its own admin key.
export CG_URL=... CG_KEY=...        # in ~/.zshrc for the main gate
scripts/publish-apk.sh "one line on what changed"

# 4. Publish to the app store. --no-changelog because step 1 already wrote the entry.
export AS_URL=https://appstore.shadowbroker.app AS_KEY=<admin key>
scripts/publish-appstore.sh --no-changelog comfyui-android examples/comfyui-android "same one line"
```

**comfyui-android is deliberately excluded from CI** (`.github/workflows/publish-appstore.yml`
builds only appstore/plugins/privaxy). Its QNN runtime libs are gitignored, so a CI build would
quietly ship an APK with no NPU support — it must be published from a machine that has run
`scripts/qnn-stage-libs.sh`. That also means `AS_KEY` lives only as a GitHub repository secret
unless someone exports it locally.

Verified 2026-08-01 on the machine's default JDK (17) with no `JAVA_HOME`/`JDK_JAVAC_OPTIONS` set.
Older notes calling for java-26 plus `JDK_JAVAC_OPTIONS=--release 17` are stale — they date from
when this box defaulted to java-8, which is no longer installed.

The phone then offers it under **Settings → App updates**. Verified working end to end on device
(2026-08-01, 0.1.0 → 0.2.0; again 2026-08-03 for 0.6.0 across both gates and the store).

### The version IS the release

`versionCode` is derived from `version` in `examples/comfyui-android/Cargo.toml` as
`(1 << 24) | (major << 16) | (minor << 8) | patch`. **Android refuses to install anything whose
code is not strictly greater than the installed build**, so shipping twice at the same version is a
silent no-op — the app fetches the manifest, correctly concludes there is nothing new, and says
nothing. There is no error to notice.

`major`/`minor`/`patch` are each a `u8`, so they cap at 255.

### Build gotchas that will cost a cycle

- **Run from `examples/comfyui-android`,** not the workspace root.
- **Match the installed build's feature list.** Building without `local-npu` silently strips
  on-device rewrite / WD14 / NPU from a phone that had them. To check what is actually installed:
  ```bash
  unzip -p target/release/apk/comfyui_android.apk lib/arm64-v8a/libcomfyui_android.so \
    | strings -a | grep -oE 'local-(sd|anima|wd14|clip|rewrite)' | sort -u
  ```
- **QNN libs must be staged** in `examples/comfyui-android/qnn-runtime-libs/arm64-v8a/` before an
  APK build (`scripts/qnn-stage-libs.sh`). They are bundled at package time regardless of features,
  so their presence in an APK does *not* prove `local-npu` was on.
- The APK lands in the **workspace** `target/release/apk/`, not the crate's.

### Verifying a build before publishing

```bash
APK=target/release/apk/comfyui_android.apk
~/Android/Sdk/build-tools/35.0.0/aapt2 dump badging $APK | head -1        # versionCode/Name
~/Android/Sdk/build-tools/35.0.0/aapt2 dump permissions $APK | grep -i install
~/Android/Sdk/build-tools/35.0.0/apksigner verify --print-certs $APK | grep -i sha-256
```

The signing cert must stay `e1ac3c3b9e0720cbce70b272c9d940a58162cb2d703967c76c31bc715c8040f1`
(`~/.android/debug.keystore`). **A different key means every update fails
`INSTALL_FAILED_UPDATE_INCOMPATIBLE` and the only fix is uninstalling the app**, which wipes its
settings. Both of Logan's build machines share this keystore.

### If a publish fails

- `413` — the gate is running a build older than `a009865`; its upload handler buffered the body
  under axum's 2 MB limit. Redeploy comfy-gate.
- `403` — `CG_KEY` is not an admin key.
- `503 no APK directory configured` — `CG_APK_DIR` is unset on the gate. It needs an
  `Environment=` line in the systemd unit plus `daemon-reload`; a plain restart won't pick it up.

## Testing changes to comfyui-android

`mod app` is `#[cfg(target_os = "android")]`, so **host `cargo check` never compiles `app.rs`** —
the file where nearly all UI work happens. Always verify with:

```bash
cd examples/comfyui-android
ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/28.0.12674087 cargo ndk -t arm64-v8a check --features tls
```

Check `tls,local-npu` too when touching anything `#[cfg(feature = "local-npu")]`. Compare the
warning count against the pre-change baseline rather than only looking for errors — new dead code
means something was left unwired.

Host-side logic (`workflow`, `types`, `schema`, `gallery`, `theme`, …) is covered by
`cargo test -p comfyui_android` and should stay green.

**Java changes** (`crates/egui-android/java/`) are otherwise only compiled at APK-package time.
Check them in a second rather than at the end of a build:

```bash
cd crates/egui-android/java
/usr/lib/jvm/java-17-openjdk/bin/javac -Xlint:deprecation --release 17 \
  -classpath ~/Android/Sdk/platforms/android-35/android.jar -d /tmp/javacheck com/github/egui_mobile/*.java
```

## House style

- **Never run `cargo fmt`.** The repo uses a compact hand style; formatting it would produce a
  diff over the whole tree.
- Comments explain *why*, not *what* — match the density and voice of the surrounding code.
- comfy-gate is a **separate repo** (`~/Desktop/comfy-gate`) deployed to the ComfyUI box. Its
  `HANDOFF-android-*.md` files are the contract the app codes against; read them rather than
  re-deriving endpoint shapes.
