//! Small self-contained helpers (time, sizes, hashing, files).
//!
//! Kept dependency-free on purpose: pulling `chrono` or `humantime` for a handful of
//! conversions would dominate the binary size of this crate.

pub mod log;
pub mod pattern;

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Converts days since 1970-01-01 to a civil (year, month, day) date.
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Day of month (1–31) of a unix time, in UTC.
pub fn day_of_month(unix: u64) -> u32 {
    civil_from_days((unix as i64).div_euclid(86_400)).2
}

/// `2026-09-25T21:40:05Z`
pub fn fmt_timestamp(unix: u64) -> String {
    let secs = unix as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `2026-09-25`
pub fn fmt_date(unix: u64) -> String {
    let (y, m, d) = civil_from_days((unix as i64).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Binary units, as used by every clash client for traffic counters.
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Parses `90`, `90s`, `30m`, `12h`, `1d`, `1h30m`.
pub fn parse_duration(input: &str) -> Result<Duration> {
    let s = input.trim();
    if s.is_empty() {
        bail!("empty duration");
    }
    if let Ok(secs) = s.parse::<u64>() {
        return Ok(Duration::from_secs(secs));
    }
    let mut total = 0u64;
    let mut number = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let unit = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => bail!("invalid duration {input:?}: unknown unit {ch:?}"),
        };
        let n: u64 = number
            .parse()
            .with_context(|| format!("invalid duration {input:?}"))?;
        total += n * unit;
        number.clear();
    }
    if !number.is_empty() {
        bail!("invalid duration {input:?}: missing unit after {number}");
    }
    Ok(Duration::from_secs(total))
}

/// `1d2h`, `12h`, `5m`, `42s`
pub fn fmt_duration(d: Duration) -> String {
    let mut secs = d.as_secs();
    if secs == 0 {
        return "0s".into();
    }
    let mut out = String::new();
    for (unit, size) in [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)] {
        if secs >= size {
            out.push_str(&format!("{}{unit}", secs / size));
            secs %= size;
        }
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Cryptographically random lowercase hex string of `bytes * 2` characters.
pub fn random_hex(bytes: usize) -> Result<String> {
    use ring::rand::SecureRandom;
    let mut buf = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| anyhow::anyhow!("system RNG unavailable"))?;
    Ok(hex(&buf))
}

/// Writes a file via a temporary sibling + rename so readers never observe a
/// half-written config (mihomo may reload it at any moment).
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = write_temp(path, data)?;
    fs::rename(&tmp, path)
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        .with_context(|| format!("rename to {}", path.display()))
}

/// Like [`write_atomic`] but never replaces an existing file. Returns `false` if
/// `path` already existed (another process won the race), leaving it untouched.
pub fn write_new(path: &Path, data: &[u8]) -> Result<bool> {
    let tmp = write_temp(path, data)?;
    let linked = fs::hard_link(&tmp, path);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
    }
}

/// Writes `data` to a fresh, uniquely named file next to `path` and syncs it.
fn write_temp(path: &Path, data: &[u8]) -> Result<std::path::PathBuf> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(data)?;
            file.sync_all()
        });
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("write {}", tmp.display()));
    }
    Ok(tmp)
}

/// Replaces control characters (terminal escapes, fake log lines) in text that
/// comes from a provider before it is printed or logged.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Percent-encodes one URL path segment (mihomo proxy/group names are arbitrary
/// UTF-8, often with emoji flags and spaces).
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for &b in segment.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_timestamps() {
        assert_eq!(fmt_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(fmt_timestamp(1_790_372_405), "2026-09-25T21:40:05Z");
        assert_eq!(fmt_date(951_782_400), "2000-02-29");
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(43_200));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86_400));
        assert!(parse_duration("12").is_ok());
        assert!(parse_duration("12x").is_err());
        assert!(parse_duration("h").is_err());
        assert!(parse_duration("1h30").is_err());
    }

    #[test]
    fn formats_durations_and_bytes() {
        assert_eq!(fmt_duration(Duration::from_secs(93_784)), "1d2h3m4s");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(107_374_182_400), "100.00 GiB");
    }

    #[test]
    fn hashes_like_coreutils() {
        // Value verified against FlClashX/Koala on a real machine (docs/dev/SUBSCRIPTIONS.md).
        assert!(sha256_hex(b"0d0af05ee8fd4dc29275718f2ce4dff1").starts_with("a3b522eaa6f7dd89"));
        assert_eq!(sha256_hex(b"").len(), 64);
        assert_eq!(random_hex(16).unwrap().len(), 32);
    }

    #[test]
    fn writes_files_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        write_atomic(&path, b"1").unwrap();
        write_atomic(&path, b"2").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"2");
        assert!(!write_new(&path, b"3").unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"2");
        assert!(write_new(&dir.path().join("g"), b"4").unwrap());
        // No temporary files are left behind.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn sanitizes_control_characters() {
        assert_eq!(sanitize("ok\x1b[31mred\nINFO fake"), "ok [31mred INFO fake");
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(
            encode_path_segment("🇳🇱 NL"),
            "%F0%9F%87%B3%F0%9F%87%B1%20NL"
        );
        assert_eq!(encode_path_segment("a-b_c.d~"), "a-b_c.d~");
    }
}
