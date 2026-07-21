//! Process/system sampling for the perf overlay: app CPU%, hottest threads, RSS, system memory,
//! and best-effort GPU busy% (Adreno kgsl sysfs). Reads Linux procfs, so it works identically on
//! Android and the host. Parsers are pure for host tests; [`Sampler`] rate-limits to ~1 Hz.

use std::collections::HashMap;
use std::time::Instant;

/// Kernel USER_HZ: procfs stat tick length. 100 on every Android/Linux target we run on.
const TICKS_PER_SEC: f32 = 100.0;
/// Minimum interval between real samples; callers can tick every frame.
const SAMPLE_SECS: f32 = 1.0;
/// Threads shown in the snapshot, hottest first.
const TOP_THREADS: usize = 4;

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Whole-process CPU as a percentage of one core (can exceed 100 on multicore).
    pub cpu_pct: f32,
    /// Hottest thread groups as (name, cpu %), same-name threads summed.
    pub threads: Vec<(String, f32)>,
    /// Process resident set.
    pub rss_mb: f32,
    pub mem_avail_mb: f32,
    pub mem_total_mb: f32,
    /// Adreno GPU busy%, when the sysfs node is readable on this device.
    pub gpu_pct: Option<f32>,
}

struct Prev {
    at: Instant,
    proc_ticks: u64,
    /// tid -> cumulative ticks.
    threads: HashMap<u32, u64>,
}

#[derive(Default)]
pub struct Sampler {
    prev: Option<Prev>,
    last: Option<(Instant, Snapshot)>,
}

impl Sampler {
    /// The current snapshot, resampling at most once per [`SAMPLE_SECS`]. `None` until the second
    /// real sample (CPU deltas need two points).
    pub fn tick(&mut self) -> Option<Snapshot> {
        let now = Instant::now();
        if let Some((at, snap)) = &self.last
            && (now - *at).as_secs_f32() < SAMPLE_SECS
        {
            return Some(snap.clone());
        }
        let snap = self.sample(now);
        if let Some(s) = &snap {
            self.last = Some((now, s.clone()));
        }
        snap
    }

    fn sample(&mut self, now: Instant) -> Option<Snapshot> {
        // A sub-250ms delta is USER_HZ quantization junk (one 10ms tick reads as 60%+); keep the
        // anchor and wait. Over ~10s the anchor is stale (HUD was off) — reseed and skip one.
        match &self.prev {
            Some(p) if (now - p.at).as_secs_f32() < 0.25 => return None,
            Some(p) if (now - p.at).as_secs_f32() > 10.0 => self.prev = None,
            _ => {}
        }
        let proc_ticks = parse_stat_ticks(&read("/proc/self/stat")?)?.1;
        let mut threads: HashMap<u32, (String, u64)> = HashMap::new();
        if let Ok(dir) = std::fs::read_dir("/proc/self/task") {
            for e in dir.flatten() {
                let Ok(tid) = e.file_name().to_string_lossy().parse::<u32>() else { continue };
                let Some(text) = read(&format!("/proc/self/task/{tid}/stat")) else { continue };
                if let Some((name, ticks)) = parse_stat_ticks(&text) {
                    threads.insert(tid, (name, ticks));
                }
            }
        }

        let prev = self.prev.replace(Prev {
            at: now,
            proc_ticks,
            threads: threads.iter().map(|(&tid, (_, t))| (tid, *t)).collect(),
        });
        let prev = prev?;
        let dt = (now - prev.at).as_secs_f32();
        if dt <= 0.0 {
            return None;
        }
        let pct = |cur: u64, old: u64| {
            (cur.saturating_sub(old)) as f32 / TICKS_PER_SEC / dt * 100.0
        };

        // Same-name threads (tokio workers, decode pools) fold into one row.
        let mut by_name: HashMap<String, f32> = HashMap::new();
        for (tid, (name, ticks)) in &threads {
            let p = pct(*ticks, prev.threads.get(tid).copied().unwrap_or(*ticks));
            if p > 0.5 {
                *by_name.entry(name.clone()).or_default() += p;
            }
        }
        let mut top: Vec<(String, f32)> = by_name.into_iter().collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));
        top.truncate(TOP_THREADS);

        let rss_kb = read("/proc/self/status").and_then(|t| parse_status_vmrss_kb(&t)).unwrap_or(0);
        let (total_kb, avail_kb) =
            read("/proc/meminfo").and_then(|t| parse_meminfo_kb(&t)).unwrap_or((0, 0));
        let gpu_pct = read("/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage")
            .and_then(|t| parse_gpu_busy(&t));

        Some(Snapshot {
            cpu_pct: pct(proc_ticks, prev.proc_ticks),
            threads: top,
            rss_mb: rss_kb as f32 / 1024.0,
            mem_avail_mb: avail_kb as f32 / 1024.0,
            mem_total_mb: total_kb as f32 / 1024.0,
            gpu_pct,
        })
    }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// `(comm, utime+stime)` from a procfs `stat` line. comm sits in parens and may itself contain
/// parens/spaces, so fields are split after the LAST `)`.
fn parse_stat_ticks(stat: &str) -> Option<(String, u64)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    // rest[0] is state (field 3); utime/stime are fields 14/15.
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;
    Some((name, utime + stime))
}

/// VmRSS in kB from `/proc/self/status` text.
fn parse_status_vmrss_kb(text: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// `(MemTotal, MemAvailable)` in kB from `/proc/meminfo` text.
fn parse_meminfo_kb(text: &str) -> Option<(u64, u64)> {
    let field = |key: &str| -> Option<u64> {
        text.lines()
            .find(|l| l.starts_with(key))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

/// Percentage from kgsl's `gpu_busy_percentage` (formats seen: `42 %`, `42%`, `42`).
fn parse_gpu_busy(text: &str) -> Option<f32> {
    text.trim().trim_end_matches('%').trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_ticks_survive_parens_in_comm() {
        let line = "1234 (tokio-runtime-w) S 1 1234 0 0 -1 4194560 500 0 0 0 250 125 0 0 20 0 32 0 100 0 0";
        let (name, ticks) = parse_stat_ticks(line).unwrap();
        assert_eq!(name, "tokio-runtime-w");
        assert_eq!(ticks, 375);
        // A comm containing ') ' must not derail field indexing.
        let evil = "77 (a) b) S 1 77 0 0 -1 0 0 0 0 0 7 3 0 0 20 0 1 0 100 0 0";
        let (name, ticks) = parse_stat_ticks(evil).unwrap();
        assert_eq!(name, "a) b");
        assert_eq!(ticks, 10);
    }

    #[test]
    fn status_and_meminfo_parse() {
        let status = "Name:\tcomfyui\nVmPeak:\t  200000 kB\nVmRSS:\t  153600 kB\nThreads:\t12\n";
        assert_eq!(parse_status_vmrss_kb(status), Some(153600));
        let meminfo = "MemTotal:       11500000 kB\nMemFree:         900000 kB\nMemAvailable:    4200000 kB\n";
        assert_eq!(parse_meminfo_kb(meminfo), Some((11500000, 4200000)));
    }

    #[test]
    fn gpu_busy_formats() {
        assert_eq!(parse_gpu_busy("42 %\n"), Some(42.0));
        assert_eq!(parse_gpu_busy("7%"), Some(7.0));
        assert_eq!(parse_gpu_busy("13\n"), Some(13.0));
        assert_eq!(parse_gpu_busy("junk"), None);
    }

    /// On any Linux host two ticks a beat apart must produce a plausible snapshot.
    #[test]
    fn sampler_smoke() {
        let mut s = Sampler::default();
        let _ = s.tick();
        // Spin (not sleep) past the 250ms delta guard so the window holds real CPU ticks.
        let start = Instant::now();
        let mut x = 0u64;
        while start.elapsed() < std::time::Duration::from_millis(300) {
            for i in 0..10_000u64 {
                x = x.wrapping_add(i * i);
            }
        }
        std::hint::black_box(x);
        let snap = s.sample(Instant::now());
        let snap = snap.expect("second sample yields a snapshot");
        assert!(snap.rss_mb > 1.0, "rss should be readable on linux: {snap:?}");
        assert!(snap.mem_total_mb > 100.0);
        // A 300ms busy-spin spans ≥ 1 USER_HZ tick, so CPU% must be strictly positive.
        assert!(snap.cpu_pct > 0.0, "spun the whole window yet cpu_pct == 0: {snap:?}");
    }

    /// A delta shorter than the guard returns nothing and keeps the anchor.
    #[test]
    fn sampler_rejects_sub_window_delta() {
        let mut s = Sampler::default();
        assert!(s.sample(Instant::now()).is_none(), "first sample only seeds the anchor");
        assert!(s.sample(Instant::now()).is_none(), "immediate resample is quantization junk");
    }
}
