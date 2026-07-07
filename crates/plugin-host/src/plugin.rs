//! One loaded plugin instance: wasmtime store, guest exports, host imports, texture remapping.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use egui::epaint;
use egui_ios_plugin_abi as abi;
use abi::{CreateConfig, FrameInput, FrameOutput, HostCallRequest, HostCallResponse, PluginManifest, WirePlatform};
use wasmtime::{AsContextMut, Caller, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::WasiP1Ctx;

use crate::engine::{COLD_DEADLINE_TICKS, FRAME_DEADLINE_TICKS, PluginEngine};
use crate::ops::{HostOps, state_file_name};

/// Guest memory ceiling per plugin.
const MAX_GUEST_MEMORY: usize = 256 << 20;

/// Cap on retained log lines per plugin.
const MAX_LOGS: usize = 400;

/// High bit namespaces plugin textures away from host-side `TextureId::User` ids.
const PLUGIN_TEX_BIT: u64 = 1 << 63;

static NEXT_HOST_TEX: AtomicU64 = AtomicU64::new(1);
static NEXT_INSTANCE_KEY: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq)]
pub enum PluginStatus {
    Ready,
    /// Trapped or failed to load; carries the error plus recent guest logs.
    Errored(String),
}

/// Decoded result of one guest frame, ready for the viewport to paint.
pub struct FrameResult {
    /// Plugin-local coordinates; the viewport translates into host space.
    pub primitives: Vec<epaint::ClippedPrimitive>,
    /// Texture updates with ids already remapped into the host namespace.
    pub textures_set: Vec<(epaint::TextureId, epaint::ImageDelta)>,
    pub textures_free: Vec<epaint::TextureId>,
    pub platform: WirePlatform,
    pub skipped_callbacks: u32,
}

pub(crate) struct StoreData {
    wasi: WasiP1Ctx,
    manifest: Arc<PluginManifest>,
    ops: Arc<dyn HostOps>,
    state_dir: PathBuf,
    pending_response: Option<Vec<u8>>,
    logs: Arc<Mutex<VecDeque<(u8, String)>>>,
    limits: StoreLimits,
}

pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,
    pub status: PluginStatus,
    pub enabled: bool,
    /// Keys per-instance GPU resources in the paint callback resource map.
    pub instance_key: u64,
    /// Platform bits from the most recent frame (wants_keyboard etc.).
    pub last_platform: WirePlatform,
    store: Store<StoreData>,
    memory: Memory,
    f_alloc: TypedFunc<u32, u32>,
    f_dealloc: TypedFunc<u32, ()>,
    f_frame: TypedFunc<(u32, u32), u64>,
    f_event: TypedFunc<(u32, u32), ()>,
    f_save: TypedFunc<(), u64>,
    f_restore: TypedFunc<(u32, u32), ()>,
    f_destroy: TypedFunc<(), ()>,
    tex_map: HashMap<u64, epaint::TextureId>,
    /// Current full-texture extent per wire id, for partial-update bounds checks.
    tex_extent: HashMap<u64, [u32; 2]>,
    logs: Arc<Mutex<VecDeque<(u8, String)>>>,
}

impl LoadedPlugin {
    /// Load `plugin.wasm` + `manifest.toml` from `dir` and create the guest app.
    pub fn load(
        engine: &PluginEngine,
        ops: Arc<dyn HostOps>,
        dir: &Path,
        create: &CreateConfig,
    ) -> Result<Self> {
        let manifest_text = std::fs::read_to_string(dir.join("manifest.toml"))
            .with_context(|| format!("reading {}/manifest.toml", dir.display()))?;
        let manifest: PluginManifest = toml::from_str(&manifest_text).context("parsing manifest.toml")?;
        if manifest.abi_version != abi::ABI_VERSION {
            bail!(
                "plugin {} was built for ABI {}, host is ABI {}",
                manifest.id,
                manifest.abi_version,
                abi::ABI_VERSION
            );
        }
        let wasm = std::fs::read(dir.join("plugin.wasm"))
            .with_context(|| format!("reading {}/plugin.wasm", dir.display()))?;
        Self::from_parts(engine, ops, dir.to_path_buf(), manifest, &wasm, create)
    }

    fn from_parts(
        engine: &PluginEngine,
        ops: Arc<dyn HostOps>,
        dir: PathBuf,
        manifest: PluginManifest,
        wasm: &[u8],
        create: &CreateConfig,
    ) -> Result<Self> {
        let module = compile_cached(engine.inner(), &dir, wasm)?;

        let state_dir = dir.join("state");
        let logs: Arc<Mutex<VecDeque<(u8, String)>>> = Arc::new(Mutex::new(VecDeque::new()));
        let data = StoreData {
            wasi: WasiCtxBuilder::new().inherit_stdout().inherit_stderr().build_p1(),
            manifest: Arc::new(manifest.clone()),
            ops,
            state_dir,
            pending_response: None,
            logs: Arc::clone(&logs),
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_GUEST_MEMORY)
                .memories(1)
                .instances(1)
                .tables(4)
                .table_elements(1 << 20)
                .build(),
        };

        let mut store = Store::new(engine.inner(), data);
        store.limiter(|d| &mut d.limits);
        store.set_epoch_deadline(COLD_DEADLINE_TICKS);

        let mut linker: Linker<StoreData> = Linker::new(engine.inner());
        wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |d| &mut d.wasi).map_err(crate::wt_err)?;
        add_host_imports(&mut linker)?;

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(crate::wt_err)
            .context("instantiating plugin")?;

        // WASI reactor convention: run initializers before touching any export.
        if let Some(init) = instance.get_func(&mut store, "_initialize") {
            init.typed::<(), ()>(&store)
                .map_err(crate::wt_err)?
                .call(&mut store, ())
                .map_err(crate::wt_err)?;
        }

        let abi_version = instance
            .get_typed_func::<(), u32>(&mut store, "plugin_abi_version")
            .map_err(crate::wt_err)
            .context("plugin_abi_version export missing — not an egui-ios plugin?")?
            .call(&mut store, ())
            .map_err(crate::wt_err)?;
        if abi_version != abi::ABI_VERSION {
            bail!("plugin ABI {abi_version} != host ABI {}", abi::ABI_VERSION);
        }

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| anyhow!("plugin exports no memory"))?;

        let f_alloc = instance.get_typed_func(&mut store, "plugin_alloc").map_err(crate::wt_err)?;
        let f_dealloc = instance.get_typed_func(&mut store, "plugin_dealloc").map_err(crate::wt_err)?;
        let f_create = instance
            .get_typed_func::<(u32, u32), u32>(&mut store, "plugin_create")
            .map_err(crate::wt_err)?;
        let f_frame = instance.get_typed_func(&mut store, "plugin_frame").map_err(crate::wt_err)?;
        let f_event = instance.get_typed_func(&mut store, "plugin_event").map_err(crate::wt_err)?;
        let f_save = instance.get_typed_func(&mut store, "plugin_save").map_err(crate::wt_err)?;
        let f_restore = instance.get_typed_func(&mut store, "plugin_restore").map_err(crate::wt_err)?;
        let f_destroy = instance.get_typed_func(&mut store, "plugin_destroy").map_err(crate::wt_err)?;

        let mut plugin = LoadedPlugin {
            manifest,
            dir,
            status: PluginStatus::Ready,
            enabled: true,
            instance_key: NEXT_INSTANCE_KEY.fetch_add(1, Ordering::Relaxed),
            last_platform: WirePlatform::default(),
            store,
            memory,
            f_alloc,
            f_dealloc,
            f_frame,
            f_event,
            f_save,
            f_restore,
            f_destroy,
            tex_map: HashMap::new(),
            tex_extent: HashMap::new(),
            logs,
        };

        let cfg_bytes = abi::encode(create);
        let (ptr, len) = plugin.write_to_guest(&cfg_bytes)?;
        let ok = f_create
            .call(&mut plugin.store, (ptr, len))
            .map_err(|e| anyhow!("{e:?}\n{}", plugin.recent_logs()))?;
        if ok != 1 {
            bail!("plugin_create failed\n{}", plugin.recent_logs());
        }
        Ok(plugin)
    }

    /// Run one guest frame. On trap the plugin transitions to `Errored`.
    pub fn run_frame(&mut self, input: &FrameInput) -> Result<FrameResult> {
        if self.status != PluginStatus::Ready {
            bail!("plugin is not ready");
        }
        // Arm the frame deadline before ANY guest call this frame — plugin_alloc runs in the
        // guest too, so a stale deadline from a long idle gap would trap it instantly.
        self.store.set_epoch_deadline(FRAME_DEADLINE_TICKS);
        let bytes = abi::encode(input);
        let (ptr, len) = self.write_to_guest(&bytes)?;
        let packed = match self.f_frame.call(&mut self.store, (ptr, len)) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("plugin trapped: {e:?}\n{}", self.recent_logs());
                self.status = PluginStatus::Errored(msg.clone());
                bail!(msg);
            }
        };
        self.store.set_epoch_deadline(COLD_DEADLINE_TICKS);
        if packed == 0 {
            let msg = format!("plugin_frame rejected input\n{}", self.recent_logs());
            self.status = PluginStatus::Errored(msg.clone());
            bail!(msg);
        }
        let out_bytes = self.read_from_guest(packed)?;
        let out: FrameOutput = abi::decode(&out_bytes).context("decoding FrameOutput")?;
        Ok(self.remap(out))
    }

    fn remap(&mut self, out: FrameOutput) -> FrameResult {
        // Texture deltas: validate against the pixel buffer, enforce partial-update bounds,
        // and remap ids into the host's namespaced range. Anything malformed is dropped.
        let mut textures_set = Vec::with_capacity(out.textures_set.len());
        for ts in &out.textures_set {
            let Some(delta) = abi::wire_to_image_delta(ts) else {
                log::warn!("plugin {}: dropped malformed texture set (id {})", self.manifest.id, ts.id);
                continue;
            };
            if let Some(pos) = ts.pos {
                // Partial update: the sub-rect must lie inside the current texture extent.
                match self.tex_extent.get(&ts.id) {
                    Some([tw, th])
                        if pos[0] + ts.size[0] <= *tw && pos[1] + ts.size[1] <= *th => {}
                    _ => {
                        log::warn!(
                            "plugin {}: dropped out-of-bounds partial texture update (id {})",
                            self.manifest.id,
                            ts.id
                        );
                        continue;
                    }
                }
            } else {
                self.tex_extent.insert(ts.id, ts.size);
            }
            let host_id = *self.tex_map.entry(ts.id).or_insert_with(|| {
                epaint::TextureId::User(PLUGIN_TEX_BIT | NEXT_HOST_TEX.fetch_add(1, Ordering::Relaxed))
            });
            textures_set.push((host_id, delta));
        }

        // Remap primitives BEFORE applying frees: egui frees textures at end-of-frame, so a
        // primitive may legitimately reference an id also in this frame's free list. Drop any
        // invalid mesh or one referencing an unknown texture.
        let mut primitives = Vec::with_capacity(out.primitives.len());
        for wp in &out.primitives {
            let Some(host_id) = self.tex_map.get(&wp.texture_id).copied() else {
                continue;
            };
            let Some(mut cp) = abi::wire_to_primitive(wp) else {
                log::warn!("plugin {}: dropped malformed mesh", self.manifest.id);
                continue;
            };
            if let epaint::Primitive::Mesh(mesh) = &mut cp.primitive {
                mesh.texture_id = host_id;
            }
            primitives.push(cp);
        }

        let textures_free = out
            .textures_free
            .iter()
            .filter_map(|id| {
                self.tex_extent.remove(id);
                self.tex_map.remove(id)
            })
            .collect();

        self.last_platform = out.platform.clone();
        FrameResult {
            primitives,
            textures_set,
            textures_free,
            platform: out.platform,
            skipped_callbacks: out.skipped_callbacks,
        }
    }

    /// Push an app event into the plugin (`PluginApp::on_host_event`).
    pub fn send_event(&mut self, topic: &str, payload: &[u8]) -> Result<()> {
        if self.status != PluginStatus::Ready {
            bail!("plugin is not ready");
        }
        self.store.set_epoch_deadline(COLD_DEADLINE_TICKS);
        let bytes = abi::encode(&abi::PluginEvent {
            topic: topic.to_owned(),
            payload: payload.to_vec(),
        });
        let (ptr, len) = self.write_to_guest(&bytes)?;
        self.f_event
            .call(&mut self.store, (ptr, len))
            .map_err(|e| self.trap(e))?;
        Ok(())
    }

    /// Snapshot guest state for a hot reload.
    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        self.store.set_epoch_deadline(COLD_DEADLINE_TICKS);
        let packed = self.f_save.call(&mut self.store, ()).map_err(|e| self.trap(e))?;
        if packed == 0 {
            return Ok(Vec::new());
        }
        self.read_from_guest(packed)
    }

    /// Restore state captured from a previous instance.
    pub fn restore_state(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.store.set_epoch_deadline(COLD_DEADLINE_TICKS);
        let (ptr, len) = self.write_to_guest(bytes)?;
        self.f_restore
            .call(&mut self.store, (ptr, len))
            .map_err(|e| self.trap(e))?;
        Ok(())
    }

    /// Clear an error state back to `Ready`. Used after a recoverable restore trap where the
    /// freshly-created guest still holds valid default state.
    pub fn clear_error(&mut self) {
        self.status = PluginStatus::Ready;
    }

    /// Drop the guest app (best-effort; the store frees everything regardless).
    pub fn destroy(&mut self) {
        self.store.set_epoch_deadline(COLD_DEADLINE_TICKS);
        let _ = self.f_destroy.call(&mut self.store, ());
    }

    /// Recent log lines `(level, line)`, oldest first.
    pub fn logs(&self) -> Vec<(u8, String)> {
        self.logs.lock().map(|l| l.iter().cloned().collect()).unwrap_or_default()
    }

    fn recent_logs(&self) -> String {
        self.logs()
            .iter()
            .rev()
            .take(6)
            .rev()
            .map(|(_, l)| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn trap(&mut self, e: wasmtime::Error) -> anyhow::Error {
        let msg = format!("plugin trapped: {e:?}\n{}", self.recent_logs());
        self.status = PluginStatus::Errored(msg.clone());
        anyhow!(msg)
    }

    fn write_to_guest(&mut self, bytes: &[u8]) -> Result<(u32, u32)> {
        let len = u32::try_from(bytes.len()).context("payload too large")?;
        let ptr = self.f_alloc.call(&mut self.store, len).map_err(|e| self.trap(e))?;
        if let Err(e) = self.memory.write(&mut self.store, ptr as usize, bytes) {
            let _ = self.f_dealloc.call(&mut self.store, ptr);
            return Err(anyhow!("guest memory write: {e}"));
        }
        Ok((ptr, len))
    }

    fn read_from_guest(&mut self, packed: u64) -> Result<Vec<u8>> {
        let (ptr, len) = abi::unpack_ptr_len(packed);
        let data = self.memory.data(&self.store);
        data.get(ptr as usize..ptr as usize + len as usize)
            .map(|s| s.to_vec())
            .ok_or_else(|| anyhow!("guest returned out-of-bounds buffer"))
    }
}

/// Compile `wasm` to a `Module`, caching the serialized artifact next to it as `plugin.cwasm`
/// keyed by content hash. Cranelift compiling a multi-MB egui module costs seconds; on iOS
/// (Pulley) this runs on the UI thread, so a warm cache avoids a re-stall on every launch and
/// hot reload. A stale, corrupt, or version-incompatible artifact silently falls back to a
/// fresh compile. `deserialize` is unsafe but the artifact is host-produced and hash-guarded.
fn compile_cached(engine: &wasmtime::Engine, dir: &Path, wasm: &[u8]) -> Result<Module> {
    let key = format!("{:016x}", fnv1a64(wasm));
    let artifact = dir.join("plugin.cwasm");
    let key_path = dir.join("plugin.cwasm.key");

    if std::fs::read_to_string(&key_path).map(|k| k.trim() == key).unwrap_or(false)
        && let Ok(bytes) = std::fs::read(&artifact)
    {
        match unsafe { Module::deserialize(engine, &bytes) } {
            Ok(module) => return Ok(module),
            Err(e) => log::warn!("plugin cache miss ({}): {e:?}", dir.display()),
        }
    }

    let module = Module::new(engine, wasm)
        .map_err(crate::wt_err)
        .context("compiling plugin.wasm")?;
    if let Ok(bytes) = module.serialize() {
        // Best-effort cache write; a read-only dir just means no caching.
        if std::fs::write(&artifact, &bytes).is_ok() {
            let _ = std::fs::write(&key_path, &key);
        }
    }
    Ok(module)
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn push_log(logs: &Mutex<VecDeque<(u8, String)>>, level: u8, line: String) {
    if let Ok(mut logs) = logs.lock() {
        if logs.len() >= MAX_LOGS {
            logs.pop_front();
        }
        logs.push_back((level, line));
    }
}

fn read_guest_bytes(caller: &mut Caller<'_, StoreData>, ptr: u32, len: u32) -> Option<Vec<u8>> {
    let mem = caller.get_export("memory")?.into_memory()?;
    mem.data(&caller)
        .get(ptr as usize..ptr as usize + len as usize)
        .map(|s| s.to_vec())
}

fn dispatch_host_call(caller: &mut Caller<'_, StoreData>, req: HostCallRequest) -> HostCallResponse {
    let data = caller.data();
    match req.op.as_str() {
        // Built-in per-plugin persistent state, sandboxed to the plugin's install dir.
        "state.set" => {
            let Ok((key, value)) = abi::decode::<(String, Vec<u8>)>(&req.payload) else {
                return HostCallResponse::Err("state.set: bad payload".into());
            };
            let dir = data.state_dir.clone();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return HostCallResponse::Err(format!("state.set: {e}"));
            }
            match std::fs::write(dir.join(state_file_name(&key)), value) {
                Ok(()) => HostCallResponse::Ok(Vec::new()),
                Err(e) => HostCallResponse::Err(format!("state.set: {e}")),
            }
        }
        "state.get" => {
            let key = String::from_utf8_lossy(&req.payload).into_owned();
            let value = std::fs::read(data.state_dir.join(state_file_name(&key))).ok();
            HostCallResponse::Ok(abi::encode(&value))
        }
        op => {
            if !data.manifest.allows(op) {
                return HostCallResponse::Denied;
            }
            let manifest = Arc::clone(&data.manifest);
            let ops = Arc::clone(&data.ops);
            match ops.call(&manifest, op, &req.payload) {
                Ok(bytes) => HostCallResponse::Ok(bytes),
                Err(e) => HostCallResponse::Err(e),
            }
        }
    }
}

fn add_host_imports(linker: &mut Linker<StoreData>) -> Result<()> {
    add_host_imports_wt(linker).map_err(crate::wt_err)
}

fn add_host_imports_wt(linker: &mut Linker<StoreData>) -> wasmtime::Result<()> {
    linker.func_wrap(
        abi::HOST_MODULE,
        "host_log",
        |mut caller: Caller<'_, StoreData>, level: u32, ptr: u32, len: u32| {
            let msg = read_guest_bytes(&mut caller, ptr, len)
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_else(|| "<bad log pointer>".into());
            let id = caller.data().manifest.id.clone();
            let lvl = match level {
                0 => log::Level::Trace,
                1 => log::Level::Debug,
                2 => log::Level::Info,
                3 => log::Level::Warn,
                _ => log::Level::Error,
            };
            log::log!(lvl, "[plugin {id}] {msg}");
            push_log(&caller.data().logs, level.min(4) as u8, msg);
        },
    )?;

    linker.func_wrap(
        abi::HOST_MODULE,
        "host_call",
        |mut caller: Caller<'_, StoreData>, ptr: u32, len: u32| -> u32 {
            let rsp = match read_guest_bytes(&mut caller, ptr, len)
                .ok_or(())
                .and_then(|b| abi::decode::<HostCallRequest>(&b).map_err(|_| ()))
            {
                Ok(req) => dispatch_host_call(&mut caller, req),
                Err(()) => HostCallResponse::Err("host_call: bad request".into()),
            };
            // Host op time (file I/O, app ops) must not be charged against the guest's frame
            // budget, else one slow op traps the plugin. Re-arm the deadline for the guest.
            caller.as_context_mut().set_epoch_deadline(FRAME_DEADLINE_TICKS);
            let encoded = abi::encode(&rsp);
            let len = encoded.len() as u32;
            caller.data_mut().pending_response = Some(encoded);
            len
        },
    )?;

    linker.func_wrap(
        abi::HOST_MODULE,
        "host_take_response",
        |mut caller: Caller<'_, StoreData>, dst: u32, cap: u32| -> u32 {
            let Some(rsp) = caller.data_mut().pending_response.take() else {
                return 0;
            };
            let n = rsp.len().min(cap as usize);
            let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
                return 0;
            };
            if mem.write(&mut caller, dst as usize, &rsp[..n]).is_err() {
                return 0;
            }
            n as u32
        },
    )?;

    Ok(())
}
