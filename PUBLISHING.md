# Publishing APKs to the app store

Builds from this repo are distributed through **https://appstore.shadowbroker.app**
(source: `github.com/shadowbrok3r/app_store`). The phone's "App Store" app installs
and updates them; there is no Play Store involved.

## Bump the version FIRST — every time

Android only offers an update whose `versionCode` is **strictly greater** than the
installed one. That number is derived from the crate's `version` in `Cargo.toml`:

```
versionCode = (1<<24) | (major<<16) | (minor<<8) | patch
```

so `0.1.0` → 16777472 and `0.1.1` → 16777473. **Shipping twice at the same version
is a silent no-op** — the store accepts the upload, the phone checks, sees a code
that is not greater, and offers nothing. Nothing errors; the update just never
appears.

So the order is always: **bump `version` → build → publish.**

| App | Crate to bump | Store slug |
|---|---|---|
| ComfyUI | `examples/comfyui-android/Cargo.toml` | `comfyui-android` |
| Privaxy | `examples/privaxy-android/Cargo.toml` | `privaxy` |
| Plugins Android | `examples/plugins-android/Cargo.toml` | `plugins-android` |
| App Store | `examples/appstore-android/Cargo.toml` | `appstore-android` |
| RingDesigner | `examples/ringdesigner-android/Cargo.toml` | `ringdesigner-android` |

## A brand-new app has to be created first

`publish-appstore.sh` only uploads to an app the store already knows about. The first publish of a
new slug fails with:

```
curl: (22) The requested URL returned error: 404
error: upload failed: {"ok":false,"error":"unknown app — create it first (POST /api/apps)"}
```

**That message names the wrong route.** `POST /api/apps` is 405; the one that works is
**`POST /api/apps/create`**:

```bash
curl -sS -X POST -H "x-api-key: $AS_KEY" -H "Content-Type: application/json" \
  --data '{"slug":"my-app","name":"My App","package":"com.example.myapp","notes":"one line"}' \
  "$AS_URL/api/apps/create"
```

`GET $AS_URL/api/apps` lists what exists, which is the quickest way to confirm the slug took before
you spend a release build on it. Then publish normally.

## Build

```bash
cd examples/<app>
cargo egui-mobile build -a --release --features tls    # drop --features tls if the crate has none
```

The APK lands in the workspace-shared `target/release/apk/<crate_name>.apk`.
(comfyui-android additionally needs `scripts/qnn-stage-libs.sh` run first.)

## Publish

```bash
export AS_URL=https://appstore.shadowbroker.app
export AS_KEY=<admin API key>

scripts/publish-appstore.sh <slug> examples/<app> "what changed"
```

Passing the **crate directory** lets the script read the version out of its
`Cargo.toml` and find the APK itself. It then verifies with `aapt2` that the APK's
embedded `versionCode` matches that version, and refuses to publish if they differ
— that catches the classic mistake of bumping the version but forgetting to
rebuild, which would otherwise offer every phone an update Android then refuses to
install.

To publish an APK by path instead, state the version explicitly:

```bash
AS_VERSION=0.1.1 scripts/publish-appstore.sh privaxy target/release/apk/privaxy_android.apk "notes"
```

## Icons and changelog

The script also **extracts the launcher icon** from the APK and uploads it, so the
store and the phone app show it instead of a letter tile. Apps built without an
icon resource (plain `NativeActivity`, no `res/` mipmaps) simply keep the letter
tile — nothing to do.

The notes argument becomes a **changelog entry** in two places: recorded on the
server (`/api/apps/<slug>/changelog`, shown under "Changelog" in the store and the
phone app, and kept even after old APKs are pruned), and prepended to the repo's
`CHANGELOG.md`.

```bash
# also git-commit CHANGELOG.md (and Cargo.toml, in crate-dir mode)
scripts/publish-appstore.sh --commit <slug> <crate-dir> "what changed"

# other flags
--changelog PATH   write somewhere other than the default CHANGELOG.md
--no-changelog     server-side entry only, leave the repo file alone
--no-icon          skip icon extraction
```

## Automatic publishing (GitHub Actions)

`.github/workflows/publish-appstore.yml` builds and publishes **appstore-android**,
**plugins-android**, and **privaxy-android** on a push to `main`/`master`, or on
demand via *Actions → Publish to app store → Run workflow*.

`.github/workflows/publish-plugins.yml` does the same for everything under
`plugins/`, publishing each to the plugin store.

The version rule still applies, and CI enforces it politely: before building, the
workflow asks the store what version it already has. If the crate version has not
been bumped, the run **skips the build and succeeds** rather than failing — there
is simply nothing new to ship. So the habit is unchanged: **bump the version, then
push.**

Repository secrets used (already set): `AS_URL`, `AS_KEY`,
`ANDROID_KEYSTORE_B64` (base64 of `~/.android/debug.keystore`),
`ANDROID_KEYSTORE_PASSWORD`.

### Not built by CI

- **comfyui-android** — its QNN runtime libs are gitignored (they come from a
  proprietary QAIRT SDK), so a CI build would quietly produce an APK without NPU
  support. Publish it from a machine that has `scripts/qnn-stage-libs.sh` run.
- **AOT `.cwasm` plugin builds** — an AOT artifact only loads in the exact wasmtime
  build that produced it, so it has to be compiled by the host, not by CI. CI ships
  the portable `.wasm`; hosts fall back to it automatically.

## Notes

- The store keeps the last 5 builds per app; you can roll back to any of them from
  the AppManager desktop app.
- Uploads over ~100 MB fail with a Cloudflare `413` before reaching the server.
  Nothing here is that big today (comfyui is the largest at ~57 MB).
- `examples/android-hello` is a demo and is not in the store.

## Publishing plugins straight from the dev loop

`cargo egui-mobile plugin build --publish` and `plugin serve --publish` push to the
store themselves — no separate script step:

```bash
export AS_URL=https://appstore.shadowbroker.app
export AS_KEY=<admin API key>

cargo egui-mobile plugin build plugins/my-widget --publish     # one-shot
cargo egui-mobile plugin serve plugins --publish               # every successful rebuild
```

`serve --publish` resolves the credentials **before** binding the socket, so a missing
key fails immediately rather than minutes later on the first rebuild. A store error
during the loop is logged and skipped — devices still hot-reload from the dev server,
because the store is a mirror of the dev loop, not a dependency of it.
`--store-url` / `--store-key` override the environment.

Identity, version, and permissions come from each `manifest.toml`. Bump `version`
there so the store shows a new release; the plugin host itself hot-reloads on content
hash, so it does not care.

No `.cwasm` is uploaded by these commands: an AOT artifact is only loadable by the
exact wasmtime build that produced it, so the host compiles it on first load. Use
`scripts/publish-plugin.sh` if you want to upload one that a host already produced.

## Installing plugins from the store on device

`examples/plugins-android` has a **Plugin store** entry in its menu dropdown: enter the
store URL and an API key once (saved beside the plugins directory), Refresh, and each
row installs or updates in place — the dev-sync connection is not involved. The
dev-sync client speaks plaintext HTTP/1.0 and cannot do TLS, which is why the store
path is a separate client.
