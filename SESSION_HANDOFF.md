# Session handoff — transfer Claude Code memory to the work machine

Goal: continue this work on the **work machine** (`shadowbroker-manjaro-work`, Tailscale
`100.80.202.35`, repo at `~/Desktop/ios-egui`) with no knowledge loss, so Claude can test the new
additions on the phone instead of guessing.

## What carries the knowledge

Claude Code keeps **per-project memory** as Markdown files. `MEMORY.md` is the index loaded into
context at the start of every session; the other `*.md` files are individual facts it pulls in as
relevant. Transferring these files = transferring the durable knowledge of this project (the live
chat transcript does **not** need to move — a fresh session reads the memory).

The catch: memory is keyed by the **absolute repo path** (the directory Claude is launched from),
with `/` → `-`. So the folder name differs per machine:

| Machine | Repo path | Memory dir |
|---|---|---|
| this one | `/home/shadowbroker/Documents/Rust/Mobile/EguiMobile` | `~/.claude/projects/-home-shadowbroker-Documents-Rust-Mobile-EguiMobile/memory/` |
| work | `/home/shadowbroker/Desktop/ios-egui` | `~/.claude/projects/-home-shadowbroker-Desktop-ios-egui/memory/` |

## Already transferred

The 8 memory files (incl. `MEMORY.md` and the new `verify-on-device.md` test checklist) were copied
to the work machine's memory dir. To re-run / refresh anytime, from **this** machine:

```bash
SRC=/home/shadowbroker/.claude/projects/-home-shadowbroker-Documents-Rust-Mobile-EguiMobile/memory
DEST=".claude/projects/-home-shadowbroker-Desktop-ios-egui/memory"
ssh shadowbroker@100.80.202.35 "mkdir -p ~/$DEST"
scp "$SRC"/*.md "shadowbroker@100.80.202.35:$DEST/"
```

(Overwrites same-named files, leaves any work-machine-only memory untouched.)

## On the work machine — pick up the session

```bash
cd ~/Desktop/ios-egui           # MUST launch from the repo root, or the memory slug won't match
git pull                        # get this session's code changes
claude                          # new session auto-loads MEMORY.md for this project
```

Then tell Claude to **read `verify-on-device.md`** and work the checklist — it lists exactly what
was built compile-only this session (MediaStore save-to-gallery, LoadImage thumbnails, the img2img
fix, multi-select, gallery videos, the ws progress fix, etc.) and how to test each, so nothing is
guessed. Deploy + test with:

```bash
cd examples/comfyui-android
cargo egui-mobile run -a --release --features tls
adb logcat -s comfyui_android::logger        # app logs
adb logcat | grep -i save_to_gallery         # MediaStore JNI result
```

## Notes

- First build on the work machine needs the one-time setup in `ANDROID_SETUP.md` (SDK/NDK/JDK,
  cargo subcommands, debug keystore). If the phone rejects the install with
  `INSTALL_FAILED_UPDATE_INCOMPATIBLE`, `adb uninstall com.example.comfyui` first (different signing
  key per machine) — or share one `~/.android/debug.keystore` across both.
- `.cargo/config.toml` is machine-specific and gitignored; regenerate it per `ANDROID_SETUP.md`.
- This doc + the Tailscale IP are machine-specific; delete it if you don't want it in the repo.
