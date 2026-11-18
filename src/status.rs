//! Measures the machine's current state. Everything is read fresh per call;
//! rates come from a single ~1 s window inside the call, so no state is
//! kept between requests.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RAPL: &str = "/sys/class/powercap/intel-rapl:0";
const WINDOW: Duration = Duration::from_secs(1);

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trim(path)?.parse().ok()
}

/// One point-in-time sample of the counters that are turned into rates.
struct Sample {
    at: Instant,
    energy_uj: Option<u64>,
    /// iface -> (rx_bytes, tx_bytes), non-lo interfaces with carrier == 1.
    net: BTreeMap<String, (u64, u64)>,
}

fn sample() -> Sample {
    Sample {
        at: Instant::now(),
        energy_uj: read_u64(format!("{RAPL}/energy_uj")),
        net: net_counters(),
    }
}

fn net_counters() -> BTreeMap<String, (u64, u64)> {
    let mut out = BTreeMap::new();
    let Ok(dir) = fs::read_dir("/sys/class/net") else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        let base = entry.path();
        // Reading carrier of a down interface fails with EINVAL.
        if read_trim(base.join("carrier")).as_deref() != Some("1") {
            continue;
        }
        let rx = read_u64(base.join("statistics/rx_bytes"));
        let tx = read_u64(base.join("statistics/tx_bytes"));
        if let (Some(rx), Some(tx)) = (rx, tx) {
            out.insert(name, (rx, tx));
        }
    }
    out
}

/// CPU package power over the window, handling counter wraparound.
fn cpu_watts(e1: Option<u64>, e2: Option<u64>, secs: f64) -> Option<f64> {
    let (e1, e2) = (e1?, e2?);
    let delta = if e2 >= e1 {
        e2 - e1
    } else {
        // The counter wraps at max_energy_range_uj.
        let max = read_u64(format!("{RAPL}/max_energy_range_uj"))?;
        max.checked_sub(e1)? + e2
    };
    if secs <= 0.0 {
        return None;
    }
    Some(round(delta as f64 / 1e6 / secs, 2))
}

fn round(x: f64, digits: i32) -> f64 {
    let m = 10f64.powi(digits);
    (x * m).round() / m
}

/// DRM cards ("card" + digits only; connector dirs like card1-DP-1 are
/// skipped), sorted by number.
fn drm_cards() -> Vec<std::path::PathBuf> {
    let mut cards: Vec<(u32, std::path::PathBuf)> = fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let n = name.strip_prefix("card")?.parse::<u32>().ok()?;
            Some((n, e.path()))
        })
        .collect();
    cards.sort();
    cards.into_iter().map(|(_, p)| p).collect()
}

/// (gpu watts, gpu asleep). The first card with a runtime_status decides
/// whether the GPU is asleep; a suspended card's hwmon is never touched,
/// since reading sensors must not wake it.
fn gpu() -> (Option<f64>, bool) {
    let cards = drm_cards();
    let status: Vec<Option<String>> = cards
        .iter()
        .map(|c| read_trim(c.join("device/power/runtime_status")))
        .collect();
    if status.iter().flatten().next().map(String::as_str) == Some("suspended") {
        return (None, true);
    }
    for (card, st) in cards.iter().zip(&status) {
        if st.as_deref() == Some("suspended") {
            continue;
        }
        let Ok(hwmons) = fs::read_dir(card.join("device/hwmon")) else {
            continue;
        };
        let mut hwmons: Vec<_> = hwmons.flatten().map(|e| e.path()).collect();
        hwmons.sort();
        for h in hwmons {
            if let Some(uw) = read_u64(h.join("power1_average")) {
                return (Some(round(uw as f64 / 1e6, 2)), false);
            }
        }
    }
    (None, false)
}

fn sshd_state() -> String {
    Command::new("systemctl")
        .args(["is-active", "sshd"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// (accepted, rejected) sshd log lines over the last 24 hours.
fn ssh24h() -> (u64, u64) {
    let Ok(out) = Command::new("journalctl")
        .args(["-u", "sshd", "--since", "-24 hours", "--no-pager", "-o", "cat"])
        .output()
    else {
        return (0, 0);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut ok = 0;
    let mut reject = 0;
    for line in text.lines() {
        if line.contains("Accepted") {
            ok += 1;
        }
        if ["Invalid user", "Failed", "not allowed"]
            .iter()
            .any(|p| line.contains(p))
        {
            reject += 1;
        }
    }
    (ok, reject)
}

fn meminfo() -> Value {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |key: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(key))?;
        let kb: u64 = line[key.len()..].trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb * 1024)
    };
    match (field("MemTotal:"), field("MemAvailable:")) {
        (Some(total), Some(avail)) => json!({
            "used": total.saturating_sub(avail),
            "total": total,
            "available": avail,
        }),
        _ => Value::Null,
    }
}

fn disk(mount: &str) -> Option<Value> {
    let c = CString::new(Path::new(mount).as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        // Skipped rather than emitted half-empty: the frontend formats
        // usedPct/avail unconditionally.
        eprintln!("statvfs {mount}: {}", std::io::Error::last_os_error());
        return None;
    }
    let frsize = st.f_frsize as u128;
    let used = (st.f_blocks as u128).saturating_sub(st.f_bfree as u128) * frsize;
    let avail = st.f_bavail as u128 * frsize;
    let total = used + avail;
    let pct = if total == 0 {
        0
    } else {
        (used * 100).div_ceil(total)
    };
    Some(json!({
        "mount": mount,
        "usedPct": pct as u64,
        "avail": avail as u64,
    }))
}

pub fn collect(disks: &[String]) -> Value {
    let (s1, s2, (sshd, (ok, reject))) = thread::scope(|scope| {
        let s1 = sample();
        // The systemctl/journalctl round trips overlap the sleep.
        let ssh = scope.spawn(|| (sshd_state(), ssh24h()));
        thread::sleep(WINDOW);
        let s2 = sample();
        let ssh = ssh
            .join()
            .unwrap_or_else(|_| ("unknown".to_owned(), (0, 0)));
        (s1, s2, ssh)
    });
    let secs = s2.at.duration_since(s1.at).as_secs_f64();

    let net: Vec<Value> = s2
        .net
        .iter()
        .map(|(iface, &(rx, tx))| {
            // Both rates or neither: a reset of either counter (or an
            // interface that just came up) makes the window meaningless.
            let rates = s1
                .net
                .get(iface)
                .filter(|&&(rx1, tx1)| rx >= rx1 && tx >= tx1 && secs > 0.0)
                .map(|&(rx1, tx1)| {
                    (
                        round((rx - rx1) as f64 / secs, 1),
                        round((tx - tx1) as f64 / secs, 1),
                    )
                });
            json!({
                "iface": iface,
                "rx": rx,
                "tx": tx,
                "rxRate": rates.map(|r| r.0),
                "txRate": rates.map(|r| r.1),
            })
        })
        .collect();

    let (gpu_w, gpu_asleep) = gpu();
    let uptime: f64 = read_trim("/proc/uptime")
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    let load: Vec<f64> = read_trim("/proc/loadavg")
        .map(|s| {
            s.split_whitespace()
                .take(3)
                .filter_map(|x| x.parse().ok())
                .collect()
        })
        .unwrap_or_default();

    json!({
        "host": read_trim("/proc/sys/kernel/hostname").unwrap_or_default(),
        "time": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "sshd": sshd,
        "ssh24h": { "ok": ok, "reject": reject },
        "uptime": uptime,
        "load": load,
        "power": {
            "cpu": cpu_watts(s1.energy_uj, s2.energy_uj, secs),
            "gpu": gpu_w,
            "gpuAsleep": gpu_asleep,
        },
        "mem": meminfo(),
        "disks": disks.iter().filter_map(|d| disk(d)).collect::<Vec<_>>(),
        "net": net,
    })
}
