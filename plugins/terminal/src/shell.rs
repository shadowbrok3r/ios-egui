//! A small self-contained command interpreter — a genuinely useful pocket console: a scientific
//! calculator, base conversions, base64, sha256, and text utilities. No shell-out; everything
//! runs in the sandbox.

use base64::Engine as _;
use ratatui::style::Color;
use sha2::{Digest, Sha256};

use crate::theme;

pub struct OutLine {
    pub text: String,
    pub color: Color,
}

impl OutLine {
    fn new(text: impl Into<String>, color: Color) -> Self {
        OutLine { text: text.into(), color }
    }
}

pub enum Effect {
    None,
    Clear,
}

pub struct Response {
    pub lines: Vec<OutLine>,
    pub effect: Effect,
}

impl Response {
    fn lines(lines: Vec<OutLine>) -> Self {
        Response { lines, effect: Effect::None }
    }
    fn text(s: impl Into<String>, color: Color) -> Self {
        Response::lines(vec![OutLine::new(s, color)])
    }
    fn ok(s: impl Into<String>) -> Self {
        Response::text(s, theme::TEXT)
    }
    fn result(s: impl Into<String>) -> Self {
        Response::text(s, theme::SUCCESS)
    }
    fn err(s: impl Into<String>) -> Self {
        Response::text(format!("error: {}", s.into()), theme::ERROR)
    }
}

pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "list commands"),
    ("clear", "clear the screen (Ctrl+L)"),
    ("echo <text>", "print text"),
    ("calc <expr>", "evaluate math, e.g. calc (1+2)*sqrt(9)"),
    ("=<expr>", "shorthand for calc"),
    ("hex|dec|bin|oct <n>", "convert an integer between bases"),
    ("b64 enc|dec <text>", "base64 encode / decode"),
    ("sha256 <text>", "SHA-256 hex digest"),
    ("upper|lower|rev <text>", "transform text"),
    ("wc <text>", "count chars / words"),
    ("history", "show command history"),
    ("about", "about this terminal"),
];

pub fn run(input: &str, history: &[String]) -> Response {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Response::lines(vec![]);
    }
    // `=expr` shorthand for calc.
    if let Some(expr) = trimmed.strip_prefix('=') {
        return calc(expr);
    }
    let (cmd, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((c, r)) => (c, r.trim()),
        None => (trimmed, ""),
    };
    match cmd.to_lowercase().as_str() {
        "help" | "?" => help(),
        "clear" | "cls" => Response { lines: vec![], effect: Effect::Clear },
        "echo" => Response::ok(rest.to_string()),
        "calc" => calc(rest),
        "hex" => convert(rest, 16),
        "dec" => convert(rest, 10),
        "bin" => convert(rest, 2),
        "oct" => convert(rest, 8),
        "b64" => base64_cmd(rest),
        "sha256" | "sha" => sha256(rest),
        "upper" => Response::ok(rest.to_uppercase()),
        "lower" => Response::ok(rest.to_lowercase()),
        "rev" => Response::ok(rest.chars().rev().collect::<String>()),
        "wc" => wc(rest),
        "history" => history_cmd(history),
        "about" | "version" => about(),
        other => Response::err(format!("unknown command `{other}` — type `help`")),
    }
}

fn help() -> Response {
    let mut lines = vec![OutLine::new("commands:", theme::ACCENT)];
    for (name, desc) in COMMANDS {
        lines.push(OutLine::new(format!("  {name:22} {desc}"), theme::MUTED));
    }
    lines.push(OutLine::new("history: ↑/↓   cursor: ←/→ Home/End   Ctrl+U clear line", theme::DIM));
    Response::lines(lines)
}

fn about() -> Response {
    Response::lines(vec![
        OutLine::new("Terminal — ratatui inside a WASM plugin", theme::ACCENT),
        OutLine::new("full text input + touch scrolling, hot-reloadable on device", theme::MUTED),
        OutLine::new("the same pattern an SSH client builds on (sockets via host ops)", theme::DIM),
    ])
}

fn calc(expr: &str) -> Response {
    if expr.trim().is_empty() {
        return Response::err("usage: calc <expression>");
    }
    match crate::calc::eval(expr) {
        Ok(v) => {
            let s = if v.fract() == 0.0 && v.abs() < 1e15 {
                format!("{}", v as i64)
            } else {
                format!("{v}")
            };
            Response::result(format!("= {s}"))
        }
        Err(e) => Response::err(e),
    }
}

fn parse_int(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let v = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(h, 16)
    } else if let Some(b) = s.strip_prefix("0b").or_else(|| s.strip_prefix("0B")) {
        i64::from_str_radix(b, 2)
    } else if let Some(o) = s.strip_prefix("0o").or_else(|| s.strip_prefix("0O")) {
        i64::from_str_radix(o, 8)
    } else {
        s.parse::<i64>()
    }
    .map_err(|_| format!("`{s}` is not an integer"))?;
    Ok(if neg { -v } else { v })
}

fn convert(rest: &str, base: u32) -> Response {
    if rest.is_empty() {
        return Response::err("usage: hex|dec|bin|oct <number>");
    }
    match parse_int(rest) {
        Ok(n) => {
            let out = match base {
                16 => format!("0x{n:x}"),
                2 => format!("0b{n:b}"),
                8 => format!("0o{n:o}"),
                _ => format!("{n}"),
            };
            Response::result(out)
        }
        Err(e) => Response::err(e),
    }
}

fn base64_cmd(rest: &str) -> Response {
    let (mode, data) = match rest.split_once(char::is_whitespace) {
        Some((m, d)) => (m, d),
        None => return Response::err("usage: b64 enc|dec <text>"),
    };
    match mode.to_lowercase().as_str() {
        "enc" | "e" => Response::result(base64::engine::general_purpose::STANDARD.encode(data.as_bytes())),
        "dec" | "d" => match base64::engine::general_purpose::STANDARD.decode(data.trim().as_bytes()) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Response::result(s),
                Err(_) => Response::err("decoded bytes are not valid UTF-8"),
            },
            Err(_) => Response::err("invalid base64"),
        },
        _ => Response::err("usage: b64 enc|dec <text>"),
    }
}

fn sha256(rest: &str) -> Response {
    if rest.is_empty() {
        return Response::err("usage: sha256 <text>");
    }
    let digest = Sha256::digest(rest.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Response::result(hex)
}

fn wc(rest: &str) -> Response {
    let chars = rest.chars().count();
    let words = rest.split_whitespace().count();
    Response::result(format!("{words} words, {chars} chars"))
}

fn history_cmd(history: &[String]) -> Response {
    if history.is_empty() {
        return Response::text("(no history yet)", theme::DIM);
    }
    let lines = history
        .iter()
        .enumerate()
        .map(|(i, h)| OutLine::new(format!("{:4}  {h}", i + 1), theme::MUTED))
        .collect();
    Response::lines(lines)
}
