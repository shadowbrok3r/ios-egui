//! Built-in reference windows: the WireLab script API + Rhai language guide and
//! the beginner wiring guide, ported from the desktop's rhai_docs/wiring_guide.
//! Glyphs are ASCII/mainstream-emoji only (the mobile hosts run egui's default
//! fonts; box-drawing, arrows and geometric shapes render as tofu there).

use egui_ios_plugin_sdk::egui;

/// (title, body) sections; code fences render monospaced via the Rhai highlighter.
const SCRIPT_SECTIONS: &[(&str, &str)] = &[
    (
        "How scripts run",
        "Every placed component can carry one script — attach it from the Script tab. \
         Scripts are live while the live session runs (Start live on the Board tab); \
         the rules program's Run/Stop does not affect them.\n\
         Apply re-compiles instantly — no reflash — and `on_start` fires again.\n\
         Callbacks WireLab invokes on the owning component:\n```\non_start()          // after connect / after Apply\non_press()          // push buttons\non_release()        // push buttons\non_change(on)       // any input: buttons, switches, digital sensors (bool)\non_reading(mv)      // analog parts, each meaningful new sample (int, millivolts)\non_tick(dt_ms)      // every frame while connected (int, elapsed ms)\non_pin(gpio, high)  // raw pin edge anywhere, e.g. the BOOT button\n```",
    ),
    (
        "Driving components",
        "Other components are addressed by their sanitized label — `Red LED!` becomes \
         `red_led` (the Script tab picker shows each part's name). \
         `me` is the component the script is attached to; `comp(\"name\")` looks one up \
         dynamically.\n```\nfn on_press() {\n    red_led.on();\n    red_led.off();\n    red_led.toggle();\n    red_led.blink(250);      // firmware-side, keeps running on its own\n    red_led.breathe(2000);   // ditto\n    red_led.dim(35);         // percent, PWM\n    servo.set_angle(120);    // degrees 0..180\n    buzzer.beep(200);        // ms\n    buzzer.tone(880, 300);   // Hz, ms\n}\n```\nVerbs map to the same engine actions the Program rules use, so anything that \
         works in a rule works from a script.",
    ),
    (
        "Reading state",
        "```\nfn on_tick(dt_ms) {\n    if btn.is_pressed() { }      // logical: true while held\n    if red_led.is_on() { }       // commanded output state\n    let mv = pot.millivolts();   // last analog sample, int\n    if pin(4).is_high() { }      // raw GPIO level from telemetry\n}\n```\nReads come from the latest telemetry snapshot (50 ms cadence), not a \
         round-trip — cheap to call every tick.",
    ),
    (
        "Raw pins, PWM & the RGB LED",
        "```\npin(2).high();\npin(2).low();\npin(2).set(true);\npin(2).toggle();\npin(8).pwm(1000, 500);       // freq Hz, duty in permille (0..1000)\npin(28).input_pullup();      // reconfigure, e.g. to watch BOOT\npin(28).is_high();\n\nrgb(255, 40, 0);             // the board's WS2812, real color via RMT\n```\nMode changes (`input_pullup`, `input_pulldown`, `input`, `output`) let a \
         script watch pins the wiring didn't configure — the BOOT button being \
         the classic case: configure it in `on_start`, react in `on_pin`.",
    ),
    (
        "Timers, time & logging",
        "```\nfn on_press() {\n    after(500, || red_led.off());     // run a closure later (ms)\n    let t = millis();                 // session clock, int ms\n    log(`held at ${t}`);              // -> the Console\n}\n```\n`after` timers belong to the component; recompiling its script cancels them. \
         Up to 64 pending timers per component. Note: `this` is not available \
         inside an `after` closure — capture what you need into a local first:\n```\nfn on_press() {\n    let n = this.count ?? 0;\n    after(300, || log(n));\n}\n```",
    ),
    (
        "Who is who: me, this, and names",
        "Three different things, easy to mix up:\n\
         • `me` — the component this script is attached to. A button script \
         reads itself with `me.is_pressed()`; an LED script drives itself with \
         `me.on()`.\n\
         • bare names — every OTHER component, addressed by its script name \
         (shown in the Script tab picker): `red_led.toggle()`.\n\
         • `this` — NOT the component. It is your script's private state map:\n```\nfn on_press() {\n    this.count = (this.count ?? 0) + 1;   // ?? gives a default when unset\n    if this.count > 3 { this.count = 0; }\n    log(`count ${this.count}, held: ${me.is_pressed()}`);\n}\n```\nState survives between events but resets when the script is re-applied. \
         One caveat: `this` is unavailable inside `after(ms, || ...)` closures — \
         capture what you need into a `let` first.",
    ),
    (
        "Board info",
        "The connected board's identity and capabilities are queryable:\n```\nfn on_start() {\n    log(chip());                     // \"ESP32-C5\"\n    if board_has(\"wifi\") { }         // matches the board's spec lines\n    if board_has(\"zigbee\") { }\n    if board_has(\"5 ghz\") { }\n}\n```\n`board_has` does a case-insensitive substring match over the board \
         profile's spec list. Radio control from scripts needs firmware-side \
         support and is not available yet — today this is for feature \
         detection so one script can adapt to different boards.",
    ),
    (
        "Network requests",
        "Scripts run app-side, so http_get uses this device's network — the \
         chip needs no Wi-Fi:\n```\nfn on_press() {\n    http_get(\"https://wttr.in/?format=3\");\n}\n\nfn on_http(status, body) {\n    if status == 200 {\n        log(body);\n    } else {\n        log(`http failed: ${status} ${body}`);\n    }\n}\n```\nReplies broadcast to every scripted component's on_http(status, body). \
         Status 0 means the request itself failed (DNS, refused, timeout) and \
         body holds the error text. Bodies are truncated to 64 KiB; up to 4 \
         requests may be in flight at once (extras are dropped with a console \
         note).",
    ),
    (
        "Flow graphs (no-code scripts)",
        "The Flow tab is a node-graph editor: wire EVENT nodes (press, \
         level, analog reading, uart line, every-N-ms) through LOGIC nodes \
         (compare, threshold, toggle, gate, delay, counter, map-range) into \
         ACTION nodes (set/toggle a component, pwm, rgb, uart, lcd, log).\n\
         Pin colors are types: orange = pulse, green = bool, blue = number, \
         purple = text.\n\
         The graph compiles to a normal Rhai script that runs exactly like \
         hand-written ones. A `script` node embeds a Rhai expression over \
         inputs a, b, c when the built-in nodes aren't enough. Flows and \
         per-component scripts run side by side.",
    ),
    (
        "Rhai: variables & types",
        "```\nlet x = 42;            // int (i64)\nlet y = 1.5;           // float (f64)\nlet s = \"text\";        // string\nlet ok = true;         // bool\nlet a = [1, 2, 3];     // array\nlet m = #{ a: 1 };     // object map\nconst LIMIT = 2000;    // constant\n```\nIntegers and floats do not mix implicitly: `1 + 1.5` is an error — write \
         `1.0 + 1.5` or `x.to_float()`. Missing map properties read as `()` \
         (unit), which is what `??` tests for.",
    ),
    (
        "Rhai: strings",
        "```\nlet name = \"world\";\nlet s = `hello ${name}, 2 + 2 = ${2 + 2}`;   // backtick interpolation\nlet n = s.len;\nlet up = s.to_upper();\nif s.contains(\"hello\") { }\n```",
    ),
    (
        "Rhai: control flow",
        "```\nif mv > 2000 {\n    // ...\n} else if mv > 1000 {\n} else {\n}\n\nlet level = if on { \"high\" } else { \"low\" };   // if is an expression\n\nswitch state {\n    0 => log(\"idle\"),\n    1 | 2 => log(\"busy\"),\n    _ => log(\"other\"),\n}\n\nfor i in 0..5 { log(i); }\nfor item in [10, 20, 30] { }\nwhile x < 10 { x += 1; }\nloop { break; }\n```",
    ),
    (
        "Rhai: functions & closures",
        "```\nfn scaled(mv, max) {\n    mv * 100 / max        // last expression is the return value\n}\n\nfn on_reading(mv) {\n    let pct = scaled(mv, 3300);\n    let f = |x| x * 2;    // closure; captures by sharing\n    log(f(pct));\n}\n```\nScript functions are pure: they see only their arguments and `this`. \
         Arguments pass by value.",
    ),
    (
        "Rhai: arrays & maps",
        "```\nlet a = [1, 2, 3];\na.push(4);\nlet n = a.len;\nlet doubled = a.map(|x| x * 2);\nlet big = a.filter(|x| x > 2);\nlet total = a.reduce(|sum, x| sum + x, 0);\n\nlet m = #{ name: \"led\", pin: 2 };\nm.pin = 4;\nif \"name\" in m { }\n```",
    ),
    (
        "Rhai: operators & errors",
        "```\nlet v = maybe ?? 0;       // default when () / missing\nlet l = obj?.len;         // safe access\nx += 1; x *= 2;           // compound assignment\n1 == 1.0;                 // false! types differ\n\ntry {\n    throw \"boom\";\n} catch (e) {\n    log(e);\n}\n```",
    ),
    (
        "Limits & safety",
        "Each callback run is capped at 200 000 operations — an accidental \
         `loop {}` aborts with an error instead of freezing the app. `eval` is \
         disabled. Compile and runtime errors show up in the Script tab header \
         and the Console (prefixed with the component name). Errors clear on \
         the next successful run.",
    ),
];

const WIRING_SECTIONS: &[(&str, &str)] = &[
    (
        "The one rule: current flows in loops",
        "Electricity only does something when it can flow in a complete loop: \
         out of a supply pin (a GPIO driven high, 3V3, or 5V), through your \
         component, and back to GND. No loop — nothing happens; that's why \
         every circuit here ends at a GND pin.\n\
         A GPIO pin is just a tiny switch the chip controls: driven high it \
         acts like a weak 3.3 V supply, driven low it acts like a connection \
         to GND. All GND pins on the board are the same wire internally — use \
         whichever is closest.",
    ),
    (
        "LEDs always need a resistor",
        "An LED is a diode: below its 'forward voltage' (~2 V for red) almost \
         no current flows; above it, current rises almost without limit — the \
         LED does NOT protect itself. Something else must limit the current, \
         and that's the series resistor's whole job.\n\
         How to size it (Ohm's law, R = V / I):\n```\nsupply        3.3 V   (a GPIO driven high)\nLED drop     -2.0 V   (its forward voltage)\nleft over     1.3 V   across the resistor\n\ntarget ~6 mA:  R = 1.3 V / 0.006 A = ~217 ohm  ->  220 ohm stock part\n```\nMore ohms = dimmer and safer; fewer = brighter and hotter. 220-330 ohm \
         is the classic range on 3.3 V boards. The resistor can sit on either \
         side of the LED — the loop current is the same everywhere.\n\
         Polarity matters: current enters the anode (+) and leaves the \
         cathode (-). Backwards = simply dark, not damaged.\n\
         Wiring: `GPIO -> resistor -> LED anode`, `LED cathode -> GND`. \
         The Checks list computes the value when you forget.",
    ),
    (
        "Buttons: why a pull-up?",
        "A push button is just two pieces of metal. Wire one side to a GPIO \
         and the other to GND:\n```\nGPIO4 -- button -- GND\n```\nPressed: the pin is connected to GND and reads LOW. Released: the pin \
         is connected to... nothing. A disconnected ('floating') pin picks up \
         electrical noise and reads randomly — that's the classic beginner trap.\n\
         The cure is a pull-up: a weak internal resistor to 3.3 V that \
         holds the pin HIGH whenever nothing stronger (the button) pulls it \
         LOW. Every ESP32 pin has one built in, and WireLab enables it \
         automatically when it sees this wiring (that's the `InputPullUp` in \
         the console).\n\
         Note the logic comes out inverted — pressed reads LOW. WireLab hides \
         that: `on_press` and `me.is_pressed()` are already the right way up.",
    ),
    (
        "Switches",
        "A toggle switch wires exactly like a button (`GPIO -> switch -> GND`, \
         pull-up on) — it just stays where you leave it.\n\
         A slide switch (SPDT) has three pins: the COMmon in the middle \
         connects to one side or the other:\n```\n3V3 -- A   COM -- GPIO   B -- GND\n```\nso the GPIO reads solid HIGH in one position and solid LOW in the \
         other — no floating, no pull-up needed.",
    ),
    (
        "Potentiometers & voltage dividers",
        "Two resistors in a row from 3.3 V to GND split the voltage at their \
         midpoint:\n```\nVout = 3.3 V x R_bottom / (R_top + R_bottom)\n```\nA potentiometer is both resistors in one part — the wiper is the \
         midpoint, so turning it sweeps 0 -> 3.3 V. Wire ends to 3V3 and GND, \
         wiper to an ADC pin (GPIO1-6 on the C5).\n\
         A photoresistor (LDR) changes resistance with light, so pair it with \
         a fixed resistor to make the divider:\n```\n3V3 -- LDR --+-- 10k -- GND\n             |\n           ADC pin\n```\nBright light -> LDR resistance drops -> the midpoint rises.",
    ),
    (
        "Buzzers, servos, relays: signal vs power",
        "Modules with V+/G/SIG pins split two jobs: the power pins carry \
         the real current (V+ -> 3V3 or 5V, G -> GND), while SIG only \
         carries information from a GPIO.\n\
         Servos want 5 V power and a PWM signal (`me.set_angle(deg)` handles \
         the pulses). Never try to power a motor or servo *from* a GPIO — \
         pins can source a few tens of mA at best; that's what the supply \
         pins are for. An active buzzer is the simple case: SIG high = noise.",
    ),
    (
        "Pins to treat with respect",
        "• Strapping pins: the chip reads them at power-on to decide how to \
         boot. Fine as outputs after boot; risky to hold high/low through a \
         reset.\n\
         • UART0 pins (GPIO11/12 on the C5): they ARE the WireLab serial \
         link — wiring them breaks the desktop connection.\n\
         • USB pins (GPIO13/14): the native USB port.\n\
         • Keep any single GPIO under ~10 mA continuous.",
    ),
    (
        "Series vs parallel (the classic LED mistake)",
        "Wiring a resistor ACROSS an LED's + and - puts it in parallel: \
         both parts see the same voltage and each draws its own current — the \
         resistor does nothing to protect the LED. Current limiting only works \
         in series, where every electron must pass through the resistor \
         first:\n```\nparallel (wrong):   GPIO --+-- LED --+-- GND\n                           +- 220 ohm -+\n\nseries (right):     GPIO -- 220 ohm -- LED -- GND\n```\nWireLab warns about both problems: the parallel arrangement gets its \
         own lint, and the live view estimates real currents — a directly \
         driven LED shows a '~52 mA (rating ~20 mA)' warning while connected.",
    ),
    (
        "Measure an unknown resistor (ohmmeter)",
        "Got a mystery resistor? Use one KNOWN resistor and the ADC:\n```\n3V3 -- unknown R --+-- known 1k -- GND\n                   |\n                 GPIO1 (ADC)\n```\nThe two resistors divide 3.3 V; measuring the midpoint gives you the \
         unknown:\n```\nR_unknown = R_known x (3300 - mv) / mv\n```\nTwo things limit the range: the ESP32 ADC pegs near ~3.1 V, and \
         it is inaccurate below ~0.1 V. So keep the reference within ~10x of \
         the unknown — a 1 k reference covers ~130 ohm to 30 kohm. (A few \
         percent off is normal — good enough to identify parts, not lab \
         metrology.)",
    ),
];

#[derive(Clone, Copy, PartialEq)]
pub enum DocKind {
    Script,
    Wiring,
}

/// Which reference is open (if any) and the shared filter text.
#[derive(Default)]
pub struct DocsState {
    pub open: Option<DocKind>,
    filter: String,
}

impl DocsState {
    /// Fullscreen-ish reference overlay; tap the title-bar close box to leave.
    pub fn show(&mut self, ctx: &egui::Context) {
        let Some(kind) = self.open else { return };
        let (title, sections) = match kind {
            DocKind::Script => ("📜 Script reference", SCRIPT_SECTIONS),
            DocKind::Wiring => ("⚡ Wiring guide", WIRING_SECTIONS),
        };
        let screen = ctx.content_rect();
        let mut still_open = true;
        egui::Window::new(title)
            .open(&mut still_open)
            .collapsible(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(screen.center())
            .default_size(screen.size() - egui::vec2(24.0, 80.0))
            .max_height(screen.height() - 90.0)
            .vscroll(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("filter");
                    ui.add(egui::TextEdit::singleline(&mut self.filter).desired_width(180.0));
                    if !self.filter.is_empty() && ui.small_button("clear").clicked() {
                        self.filter.clear();
                    }
                });
                ui.separator();
                let needle = self.filter.to_lowercase();
                let base = ui.visuals().text_color();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, (title, body)) in sections.iter().enumerate() {
                        if !needle.is_empty()
                            && !title.to_lowercase().contains(&needle)
                            && !body.to_lowercase().contains(&needle)
                        {
                            continue;
                        }
                        egui::CollapsingHeader::new(egui::RichText::new(*title).strong())
                            .default_open(i == 0 || !needle.is_empty())
                            .show(ui, |ui| {
                                for (j, chunk) in body.split("```").enumerate() {
                                    if chunk.trim().is_empty() {
                                        continue;
                                    }
                                    if j % 2 == 0 {
                                        ui.label(chunk.trim_matches('\n'));
                                    } else {
                                        let job = crate::view::highlight_rhai(
                                            chunk.trim_matches('\n'),
                                            base,
                                            &[],
                                        );
                                        ui.label(job);
                                    }
                                }
                            });
                    }
                });
            });
        if !still_open {
            self.open = None;
        }
    }
}
