//! On-device live runtime: the desktop's session loop (component scripts, the
//! flow script, the rules engine, wiring-derived pin setup and the electrical
//! solve) running against the plugin's TCP board link — no desktop in the loop
//! once a project snapshot has been fetched.

use wirelab_core::engine::{Engine, InKind, plan_setup};
use wirelab_core::program::{Action, Program};
use wirelab_core::project::BoardTab;
use wirelab_core::script::{FLOW_ID, ScriptHost, World};
use wirelab_core::sim::{SimOutput, solve};
use wirelab_proto::{DeviceMsg, EventEdge, HostMsg};

use egui_ios_plugin_sdk::abi::{self, net};

use crate::link::{BoardLink, LinkState, Ops};
use crate::view::BoardModel;

/// Script `http_get` requests over the host's async HTTP ops: max 4 in
/// flight, 64 KiB bodies, transport errors delivered as status 0.
#[derive(Default)]
struct HttpPool {
    inflight: Vec<u64>,
}

impl HttpPool {
    fn cancel_all(&mut self, ops: &dyn Ops) {
        for id in self.inflight.drain(..) {
            let _ = ops.call(net::op::HTTP_CANCEL, &net::id_to_bytes(id));
        }
    }

    fn spawn(&mut self, ops: &dyn Ops, url: String) -> bool {
        if self.inflight.len() >= 4 {
            return false;
        }
        let req = net::HttpRequest {
            method: "GET".into(),
            url,
            headers: Vec::new(),
            body: Vec::new(),
            timeout_ms: 15_000,
        };
        match ops.call(net::op::HTTP_REQUEST, &abi::encode(&req)) {
            Ok(bytes) => match net::id_from_bytes(&bytes) {
                Some(id) => {
                    self.inflight.push(id);
                    true
                }
                None => false,
            },
            Err(_) => false,
        }
    }

    fn drain_done(&mut self, ops: &dyn Ops) -> Vec<(u16, String)> {
        let mut done = Vec::new();
        self.inflight.retain(|&id| {
            let rsp = match ops.call(net::op::HTTP_POLL, &net::id_to_bytes(id)) {
                Ok(bytes) => match abi::decode::<net::HttpPoll>(&bytes) {
                    Ok(net::HttpPoll::Pending) => return true,
                    Ok(net::HttpPoll::Done(rsp)) => rsp,
                    Ok(net::HttpPoll::Error(e)) => {
                        done.push((0, e));
                        return false;
                    }
                    Err(_) => {
                        done.push((0, "bad HttpPoll".into()));
                        return false;
                    }
                },
                Err(e) => {
                    done.push((0, e));
                    return false;
                }
            };
            // Truncate bytes before the lossy convert: String::truncate
            // panics off a char boundary.
            let cut = rsp.body.len().min(64 * 1024);
            let body = String::from_utf8_lossy(&rsp.body[..cut]).into_owned();
            done.push((rsp.status, body));
            false
        });
        done
    }
}

/// The live session the plugin hosts itself, mirroring the desktop's
/// `LiveState::tick` over the board link.
pub struct LiveRunner {
    /// Whether the runtime is on (scripts + flow live, program startable).
    pub active: bool,
    pub scripts: ScriptHost,
    pub engine: Engine,
    pub program_running: bool,
    /// Solve output for live canvas painting, refreshed every tick.
    pub live_output: Option<SimOutput>,
    /// Commanded+telemetry pin bank behind that solve, for pin-state paint.
    pub bank: Option<wirelab_core::sim::PinBank>,
    /// Content keys last synced (see [`BoardModel`]), so the 3-second
    /// snapshot refresh doesn't re-run setup or re-fire on_start.
    board_key: u64,
    scripts_key: u64,
    flow_key: u64,
    setup_key: u64,
    /// The link was Ready last tick; a drop forces a full re-setup.
    seen_ready: bool,
    /// Re-fire every script's on_start after the next setup (reconnect).
    refire_scripts: bool,
    /// UART line assembly for script `on_uart` dispatch.
    uart_buf: String,
    http: HttpPool,
    last_rgb: Option<[u8; 3]>,
    lcd_ops: Option<Vec<wirelab_core::sim::LcdOp>>,
}

impl Default for LiveRunner {
    fn default() -> Self {
        LiveRunner {
            active: false,
            scripts: ScriptHost::new(),
            engine: Engine::default(),
            program_running: false,
            live_output: None,
            bank: None,
            board_key: u64::MAX,
            scripts_key: u64::MAX,
            flow_key: u64::MAX,
            setup_key: u64::MAX,
            seen_ready: false,
            refire_scripts: false,
            uart_buf: String::new(),
            http: HttpPool::default(),
            last_rgb: None,
            lcd_ops: None,
        }
    }
}

impl LiveRunner {
    pub fn start(&mut self) {
        self.active = true;
        self.board_key = u64::MAX;
        self.scripts_key = u64::MAX;
        self.flow_key = u64::MAX;
        self.setup_key = u64::MAX;
        self.seen_ready = false;
        self.refire_scripts = false;
    }

    /// Stop everything and soft-reset the board's pin state.
    pub fn stop(&mut self, ops: &dyn Ops, link: &mut BoardLink) {
        if self.program_running {
            self.engine.stop();
            self.program_running = false;
        }
        self.scripts = ScriptHost::new();
        self.engine = Engine::default();
        self.active = false;
        self.live_output = None;
        self.bank = None;
        self.last_rgb = None;
        self.lcd_ops = None;
        self.uart_buf.clear();
        self.http.cancel_all(ops);
        link.capture_events = false;
        let _ = link.take_events();
        if link.connected() {
            link.send(ops, &HostMsg::Reset);
            // Reset zeroes the board's telemetry interval too; the Board tab
            // still wants its live GPIO grid.
            link.send(ops, &HostMsg::SetTelemetry { interval_ms: 50 });
        }
    }

    pub fn start_program(
        &mut self,
        ops: &dyn Ops,
        link: &mut BoardLink,
        program: &Program,
        model: &BoardModel,
        now_ms: u64,
    ) {
        // Reuse the engine: its output shadow, behavior slots and any script
        // continuations must survive a program (re)start.
        self.engine.program = program.clone();
        self.engine.set_bindings(model.bindings.clone());
        let msgs = self.engine.start(now_ms);
        for m in &msgs {
            link.send(ops, m);
        }
        self.program_running = true;
    }

    pub fn stop_program(&mut self) {
        self.engine.stop();
        self.program_running = false;
    }

    /// One frame of the live loop; call whenever active with a Ready link.
    pub fn tick(
        &mut self,
        ops: &dyn Ops,
        link: &mut BoardLink,
        model: &BoardModel,
        tab: &BoardTab,
        now_ms: u64,
        log: &mut Vec<String>,
    ) {
        if !self.active {
            return;
        }
        if link.state != LinkState::Ready {
            // A dropped link means the board may have rebooted: re-apply
            // setup and re-fire on_start once it's back.
            if self.seen_ready {
                self.seen_ready = false;
                self.setup_key = u64::MAX;
                self.refire_scripts = true;
                if self.program_running {
                    self.stop_program();
                    log.push("program stopped (link lost)".into());
                }
            }
            return;
        }
        self.seen_ready = true;
        link.capture_events = true;

        // Resync on real content changes, not on the periodic snapshot pull.
        let mut script_actions: Vec<Action> = Vec::new();
        if self.board_key != tab.id {
            self.board_key = tab.id;
            if self.program_running {
                self.stop_program();
                log.push("program stopped (board switched)".into());
            }
        }
        if self.scripts_key != model.scripts_key {
            self.scripts_key = model.scripts_key;
            self.scripts.set_board(
                model.board.chip.name(),
                &model.board.specs,
                model.board.features.rgb_led_gpio,
            );
            let fresh = self.scripts.sync(&tab.circuit, &model.lib);
            for c in fresh {
                script_actions.extend(self.scripts.on_start(c));
            }
        }
        if self.flow_key != model.flow_key {
            self.flow_key = model.flow_key;
            match wirelab_core::flow::compile(&tab.flow) {
                Ok(src) => {
                    let src = if tab.flow.nodes.is_empty() { None } else { Some(src) };
                    if self.scripts.set_flow_script(src.as_deref()) {
                        script_actions.extend(self.scripts.on_start(FLOW_ID));
                    }
                }
                Err(errs) => {
                    // A broken flow must not keep running its old compile.
                    self.scripts.set_flow_script(None);
                    log.push(format!("flow not live: {} error(s)", errs.len()));
                }
            }
        }

        // Wiring-derived pin setup whenever the topology changes.
        if self.setup_key != model.setup_key {
            self.setup_key = model.setup_key;
            let (msgs, _) = plan_setup(&tab.circuit, &model.board, &model.lib, &model.netlist);
            for m in &msgs {
                link.send(ops, m);
            }
            log.push(format!("pin setup applied ({} commands)", msgs.len()));
            self.engine.set_bindings(model.bindings.clone());
            if std::mem::take(&mut self.refire_scripts) {
                for c in self.scripts.scripted() {
                    script_actions.extend(self.scripts.on_start(c));
                }
            }
        }

        // Pump device -> engine + scripts -> device.
        let msgs = link.take_events();
        let mut out = Vec::new();
        if self.program_running {
            for m in &msgs {
                out.extend(self.engine.handle_device(now_ms, m));
            }
        }

        // Snapshot for script-side reads (is_on, is_pressed, millivolts...).
        let mut world = World { levels: link.levels, now_ms, ..Default::default() };
        for (comp, b) in &self.engine.bindings.outputs {
            world.outputs_on.insert(*comp, self.engine.out_high(b.gpio) == b.active_high);
        }
        for (gpio, b) in &self.engine.bindings.inputs {
            let level = link.levels & (1u64 << (*gpio).min(63)) != 0;
            world.inputs_on.insert(b.comp, level != b.active_low);
        }
        for (comp, gpio) in &self.engine.bindings.analog {
            if let Some(mv) = link.analog.get(gpio) {
                world.analog_mv.insert(*comp, *mv);
            }
        }
        world.pin_analog_mv = link.analog.iter().map(|(k, v)| (*k, *v)).collect();
        self.scripts.set_world(world);

        for m in &msgs {
            match m {
                DeviceMsg::Event { pin, edge, .. } => {
                    if let Some(b) = self.engine.bindings.inputs.get(pin).copied() {
                        let logical = (*edge == EventEdge::Rising) != b.active_low;
                        if b.kind == InKind::Button {
                            script_actions.extend(if logical {
                                self.scripts.on_press(b.comp)
                            } else {
                                self.scripts.on_release(b.comp)
                            });
                        }
                        script_actions.extend(self.scripts.on_change(b.comp, logical));
                    }
                    let high = *edge == EventEdge::Rising;
                    for comp in self.scripts.scripted() {
                        script_actions.extend(self.scripts.on_pin(comp, *pin, high));
                    }
                }
                DeviceMsg::Telemetry { analog, .. } => {
                    for s in analog.iter() {
                        let bound: Vec<_> = self
                            .engine
                            .bindings
                            .analog
                            .iter()
                            .filter(|(_, g)| **g == s.pin)
                            .map(|(c, _)| *c)
                            .collect();
                        for comp in bound {
                            script_actions.extend(self.scripts.on_reading(comp, s.millivolts));
                        }
                    }
                }
                DeviceMsg::AnalogValue { pin, millivolts } => {
                    let bound: Vec<_> = self
                        .engine
                        .bindings
                        .analog
                        .iter()
                        .filter(|(_, g)| *g == pin)
                        .map(|(c, _)| *c)
                        .collect();
                    for comp in bound {
                        script_actions.extend(self.scripts.on_reading(comp, *millivolts));
                    }
                }
                DeviceMsg::SpiData { data } => {
                    for comp in self.scripts.scripted() {
                        script_actions.extend(self.scripts.on_spi(comp, data));
                    }
                }
                DeviceMsg::I2cData { addr, data } => {
                    for comp in self.scripts.scripted() {
                        script_actions.extend(self.scripts.on_i2c(comp, *addr, data));
                    }
                }
                DeviceMsg::UartData { data } => {
                    self.uart_buf.push_str(&String::from_utf8_lossy(data));
                    while let Some(nl) = self.uart_buf.find('\n') {
                        let line: String =
                            self.uart_buf.drain(..=nl).collect::<String>().trim_end().to_string();
                        for comp in self.scripts.scripted() {
                            script_actions.extend(self.scripts.on_uart(comp, &line));
                        }
                    }
                    if self.uart_buf.len() > 4096 {
                        self.uart_buf.clear();
                    }
                }
                _ => {}
            }
        }

        // Finished http_get requests broadcast to every scripted component.
        for (status, body) in self.http.drain_done(ops) {
            for comp in self.scripts.scripted() {
                script_actions.extend(self.scripts.on_http(comp, i64::from(status), &body));
            }
        }
        script_actions.extend(self.scripts.tick(now_ms));
        if !script_actions.is_empty() {
            // HTTP runs through host ops; board mail has no peer here.
            script_actions.retain(|a| match a {
                Action::BoardMsg { to, .. } => {
                    log.push(format!("send_board('{to}') dropped — one board on this device"));
                    false
                }
                Action::HttpGet { url } => {
                    if !self.http.spawn(ops, url.clone()) {
                        log.push(format!("http_get dropped (too many in flight): {url}"));
                    }
                    false
                }
                _ => true,
            });
            out.extend(self.engine.run_script_actions(script_actions, now_ms));
        }
        out.extend(self.engine.tick(now_ms));

        // Shadow RGB/LCD commands for the canvas drawing.
        for m in &out {
            use wirelab_core::sim::{LcdOp, rgb888};
            match m {
                HostMsg::SetRgb { r, g, b, .. } => self.last_rgb = Some([*r, *g, *b]),
                HostMsg::LcdInit { .. } => self.lcd_ops = Some(vec![LcdOp::Clear([0, 0, 0])]),
                HostMsg::LcdClear { rgb565 } => {
                    if let Some(ops) = &mut self.lcd_ops {
                        ops.clear();
                        ops.push(LcdOp::Clear(rgb888(*rgb565)));
                    }
                }
                HostMsg::LcdRect { x, y, w, h, rgb565 } => {
                    if let Some(ops) = &mut self.lcd_ops {
                        ops.push(LcdOp::Rect { x: *x, y: *y, w: *w, h: *h, rgb: rgb888(*rgb565) });
                        if ops.len() > 512 {
                            ops.drain(..256);
                        }
                    }
                }
                HostMsg::LcdText { x, y, rgb565, text } => {
                    if let Some(ops) = &mut self.lcd_ops {
                        ops.push(LcdOp::Text {
                            x: *x,
                            y: *y,
                            rgb: rgb888(*rgb565),
                            text: text.to_string(),
                        });
                        if ops.len() > 512 {
                            ops.drain(..256);
                        }
                    }
                }
                _ => {}
            }
        }
        for m in &out {
            link.send(ops, m);
        }
        for line in self.engine.log.drain(..) {
            log.push(line);
        }
        for line in self.scripts.take_logs() {
            log.push(line);
        }

        // Live paint state: solve the circuit against the telemetry-backed bank.
        let bank = link.effective_bank();
        let mut sim =
            solve(&tab.circuit, &model.board, &model.lib, &model.netlist, &bank);
        sim.rgb = self.last_rgb;
        sim.lcd = self.lcd_ops.clone();
        self.live_output = Some(sim);
        self.bank = Some(bank);
    }
}
