# Android build setup (Manjaro)

What you need to build the app on a second Manjaro machine. It's **not just the SDK** — five pieces:
the Rust Android target, a JDK 17–21, the Android SDK + NDK, two cargo subcommands, and this repo's
`cargo egui-mobile` wrapper.

These mirror the working machine exactly: **NDK 28.0.12674087, build-tools 35.0.0,
platform android-35, JDK 17**.

## One-time setup

```bash
# 1. Rust + the Android target (skip the first line if rustup is already installed)
sudo pacman -S --needed rustup && rustup default stable
rustup target add aarch64-linux-android

# 2. A JDK the Android build tools accept (d8 needs 11+, rejects >21 — 17 is the safe pick).
#    Make it the system default so `java` on PATH isn't Manjaro's JDK 8, which breaks d8.
sudo pacman -S --needed jdk17-openjdk unzip
sudo archlinux-java set java-17-openjdk

# 3. Cargo subcommands: cargo-apk2 packages the APK, cargo-ndk is for fast compile-checks
cargo install cargo-apk2 cargo-ndk

# 4. Android SDK command-line tools -> ~/Android/Sdk (the layout the wrapper auto-discovers)
mkdir -p ~/Android/Sdk/cmdline-tools
cd /tmp
curl -O https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip
unzip -q commandlinetools-linux-11076708_latest.zip -d ~/Android/Sdk/cmdline-tools
mv ~/Android/Sdk/cmdline-tools/cmdline-tools ~/Android/Sdk/cmdline-tools/latest

# 5. The SDK packages, matching the working machine's versions
export ANDROID_HOME=~/Android/Sdk
SDKMGR=~/Android/Sdk/cmdline-tools/latest/bin/sdkmanager
yes | "$SDKMGR" --licenses
"$SDKMGR" "platform-tools" "build-tools;35.0.0" "platforms;android-35" "ndk;28.0.12674087"

# 6. Debug keystore for signing release sideloads. Android Studio/adb normally create this; on a
#    fresh box you must generate it, or the build fails at the signing step with
#    "debug.keystore (No such file or directory)". Params match the app's signing config exactly.
mkdir -p ~/.android
keytool -genkeypair -v -keystore ~/.android/debug.keystore \
  -storepass android -keypass android -alias androiddebugkey \
  -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Android Debug, O=Android, C=US"
```

> The command-line-tools filename (`commandlinetools-linux-11076708_latest.zip`) bumps over time.
> If that 404s, grab the current "Command line tools only" (Linux) link from
> <https://developer.android.com/studio#command-line-tools-only> and swap the URL.

## In the repo (once per checkout)

```bash
cd /path/to/EguiMobile

# Install this repo's cargo subcommand (finds the SDK/NDK/JDK/Kotlin and sets the env for you).
# Re-run after pulling CLI changes — `~/.cargo/bin/cargo-egui-mobile` is a snapshot, not a symlink.
cargo install --path crates/cargo-egui-mobile

# The `tls` feature builds ring against the NDK, whose build script wants the compiler by explicit
# path. Write a machine-local .cargo/config.toml (untracked) with your real paths:
NDK="$HOME/Android/Sdk/ndk/28.0.12674087"
CLANG="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
mkdir -p .cargo
cat > .cargo/config.toml <<EOF
[env]
ANDROID_HOME = "$HOME/Android/Sdk"
ANDROID_NDK_ROOT = "$NDK"
JAVA_HOME = "/usr/lib/jvm/java-17-openjdk"
CC_aarch64_linux_android = "$CLANG/aarch64-linux-android26-clang"
AR_aarch64_linux_android = "$CLANG/llvm-ar"
EOF
```

## Build & run

```bash
cd examples/comfyui-android

# Fast compile-check (no linking, no device)
cargo ndk -t arm64-v8a check -p comfyui_android --features tls

# Package the signed APK  (drop --features tls for the http-only build)
cargo egui-mobile build -a --release --features tls
#   -> target/release/apk/comfyui_android.apk   (at the workspace root, not this dir)

# …or build, install, and launch on a USB-connected phone (adb debugging on)
cargo egui-mobile run -a --release --features tls

# Wireless (same Wi‑Fi as the phone; see “Wireless adb” below)
cargo egui-mobile run -a --tcp 192.168.1.20 --release --features tls
```

First `--features tls` build compiles ring + rustls from source, so it's slow; later builds are
incremental. The debug keystore for signing is auto-generated at `~/.android/debug.keystore`.

## Wireless adb

`--tcp` runs `adb connect host:port` (default port **5555**), checks `adb devices`, then passes
`--device host:port` to `cargo apk2` so install/launch targets that phone even if a USB device is
also attached. Omitting `--tcp` keeps the usual USB/default adb behavior.

**One-time phone setup** (pick one):

1. **USB once, then TCP** — with USB debugging on and the phone plugged in:
   ```bash
   adb tcpip 5555          # phone stays in TCP mode until reboot
   adb connect 192.168.1.20:5555
   ```
   Unplug USB; use `cargo egui-mobile run -a --tcp 192.168.1.20` afterwards.

2. **Android 11+ Wireless debugging** — Developer options → Wireless debugging → pair with the
   pairing code once (`adb pair ip:pairing-port`), then note the **IP and port** shown (often not
   5555). Connect with that address, or:
   ```bash
   cargo egui-mobile adb-connect 192.168.1.20:37123
   cargo egui-mobile run -a --tcp 192.168.1.20:37123
   ```

`cargo egui-android` accepts the same `--tcp` / `adb-connect` flags.

## Notes / gotchas

- **JDK version matters.** Manjaro's default `java` is often 8, which makes `cargo apk2`'s dex step
  fail with a Java-8 stack trace. `archlinux-java set java-17-openjdk` fixes it for bare
  `cargo apk2`; the `cargo egui-mobile` wrapper also PATH-prepends a JDK 17–21 on its own, so either
  path works once a 17–21 JDK is installed. (JDK 21 works too; avoid 22+.)
- **The NDK version isn't strictly pinned** — the wrapper picks the newest NDK under
  `~/Android/Sdk/ndk`. Installing 28.0.12674087 just matches the other machine. If you install a
  different one, update the two paths in `.cargo/config.toml` accordingly.
- `.cargo/config.toml` holds absolute paths, so it's per-machine and **not committed** (it's in the
  repo's ignore) — that's why you regenerate it above rather than copying it over.
- No emulator or system images are needed; this only builds for and sideloads to a real device.
- **Signature mismatch across machines.** Each machine's freshly-generated `debug.keystore` is a
  different key, so a device that already has the app (signed by another machine) rejects the
  update with `INSTALL_FAILED_UPDATE_INCOMPATIBLE`. Either `adb uninstall com.example.comfyui`
  first (loses the app's saved settings), or — better — copy one `~/.android/debug.keystore` to
  every machine (`scp ~/.android/debug.keystore user@host:~/.android/`) so all builds sign
  identically and install over each other. A shared keystore also makes the `keytool` step above
  unnecessary.
