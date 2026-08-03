//! Getting-started snippets, one tap from a working template — the desktop's
//! IDE side panel list, driven by the board profile (specs + features) so a
//! C3 shows a different list than a C5.

use wirelab_core::board::BoardProfile;

pub struct Snippet {
    pub title: &'static str,
    pub blurb: &'static str,
    pub code: &'static str,
}

/// What this board can do, as insertable templates.
pub fn snippets_for(board: &BoardProfile) -> Vec<Snippet> {
    let has = |what: &str| board.specs.iter().any(|s| s.to_lowercase().contains(what));
    let mut out = vec![
        Snippet {
            title: "GPIO in / out",
            blurb: "drive and read raw pins",
            code: "fn on_start() {\n    pin(2).output();\n    pin(2).high();\n    pin(28).input_pullup();      // e.g. the BOOT button\n}\n\nfn on_pin(gpio, high) {\n    if gpio == 28 && !high { log(\"BOOT pressed\"); }\n}\n",
        },
        Snippet {
            title: "PWM dimming",
            blurb: "frequency + duty on any pin",
            code: "fn on_start() {\n    // 1 kHz, 35% duty (duty is 0..1000 permille)\n    pin(2).pwm(1000, 350);\n}\n",
        },
        Snippet {
            title: "Analog watch (ADC)",
            blurb: "stream millivolts from a pin",
            code: "fn on_start() {\n    pin(3).watch_analog(100);    // sample every 100 ms\n}\n\nfn on_reading(mv) {\n    log(`analog: ${mv} mV`);\n}\n",
        },
        Snippet {
            title: "Timers & delays",
            blurb: "after() closures + on_tick clocks",
            code: "fn on_start() {\n    this.deadline = 0;\n    after(1000, || log(\"one second later\"));\n}\n\nfn on_tick(dt_ms) {\n    // countdowns that may touch `this` live here, not in after()\n    if this.deadline != 0 && millis() >= this.deadline {\n        this.deadline = 0;\n        log(\"timed out\");\n    }\n}\n",
        },
        Snippet {
            title: "State machine",
            blurb: "the idle/armed/running skeleton",
            code: "fn on_start() {\n    this.state = \"idle\";\n}\n\nfn on_press() {\n    if this.state == \"idle\" {\n        this.state = \"running\";\n        log(\"started\");\n    } else {\n        this.state = \"idle\";\n        log(\"stopped\");\n    }\n}\n",
        },
        Snippet {
            title: "UART serial",
            blurb: "TX/RX on any pins, line-based RX",
            code: "fn on_start() {\n    uart(4, 5, 115200);          // tx, rx, baud\n    uart_send(\"hello\\r\\n\");\n}\n\nfn on_uart(line) {\n    log(`rx: ${line}`);\n}\n",
        },
        Snippet {
            title: "I2C sensor",
            blurb: "read registers; sim emulates a BME280 @0x76",
            code: "fn on_start() {\n    i2c_setup(0, 1, 100);        // sda, scl, kHz\n    i2c_read(0x76, 0xD0, 1);     // who-am-I\n}\n\nfn on_i2c(addr, data) {\n    log(`i2c ${addr}: ${data}`);\n}\n",
        },
        Snippet {
            title: "SPI transfer",
            blurb: "full-duplex with chip select",
            code: "fn on_start() {\n    spi_setup(6, 7, 2, 1000);    // sck, mosi, miso, kHz\n    spi_xfer(8, [0x9F, 0, 0]);   // cs pin, bytes out\n}\n\nfn on_spi(data) {\n    log(`spi: ${data}`);\n}\n",
        },
        Snippet {
            title: "ST7735 LCD",
            blurb: "init, clear, rectangles, text",
            code: "fn on_start() {\n    lcd_init(6, 7, 8, 9, 23);    // sck, mosi, cs, dc, rst\n    lcd_clear(0, 0, 40);\n    lcd_rect(0, 0, 128, 14, 20, 20, 30);\n    lcd_text(6, 3, \"hello wirelab\", 255, 255, 255);\n}\n",
        },
        Snippet {
            title: "HTTP request",
            blurb: "fetch a URL over this device's network",
            code: "fn on_press() {\n    http_get(\"https://wttr.in/?format=3\");\n}\n\nfn on_http(status, body) {\n    if status == 200 {\n        log(body);\n    } else {\n        log(`http ${status}: ${body}`);   // status 0 = request failed\n    }\n}\n",
        },
    ];

    if board.features.rgb_led_gpio.is_some() {
        out.insert(
            0,
            Snippet {
                title: "On-board RGB LED",
                blurb: "the WS2812, real color via RMT",
                code: "fn on_start() {\n    rgb(80, 0, 120);\n    after(1000, || rgb(0, 0, 0));\n}\n",
            },
        );
    }
    if has("wi-fi") || has("wifi") {
        out.push(Snippet {
            title: "Wi-Fi",
            blurb: "feature-detect; the link itself rides Wi-Fi",
            code: "fn on_start() {\n    // Scripts run app-side, so networking APIs live in WireLab, not on\n    // the chip. Today scripts can feature-detect and adapt:\n    if board_has(\"wifi\") {\n        log(`${chip()} has Wi-Fi - link can go cable-free`);\n    }\n}\n",
        });
    }
    if has("bluetooth") || has("ble") {
        out.push(Snippet {
            title: "Bluetooth LE",
            blurb: "feature-detect (script control is on the roadmap)",
            code: "fn on_start() {\n    // BLE advertising/scanning from scripts needs firmware support that is\n    // still on the roadmap. Feature-detect today so the script adapts:\n    if board_has(\"bluetooth\") {\n        log(\"BLE-capable board\");\n    }\n}\n",
        });
    }
    if has("802.15.4") || has("zigbee") || has("thread") {
        out.push(Snippet {
            title: "802.15.4 (Zigbee/Thread)",
            blurb: "feature-detect (script control is on the roadmap)",
            code: "fn on_start() {\n    if board_has(\"802.15.4\") {\n        log(\"Zigbee/Thread-capable board\");\n    }\n}\n",
        });
    }
    out
}
