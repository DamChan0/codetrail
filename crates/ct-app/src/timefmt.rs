//! Small time/format helpers (no chrono dependency).

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `2026-10-07 17:03` (UTC; git author time is shown without tz math).
pub fn abs(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", secs / 3600, (secs % 3600) / 60)
}

pub fn abs_date(t: i64) -> String {
    let (y, m, d) = civil(t.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `just now`, `5m ago`, `3h ago`, `4d ago`, `2026-03-02`.
pub fn rel(now: i64, t: i64) -> String {
    let d = now - t;
    if d < 0 {
        return abs_date(t);
    }
    match d {
        0..=44 => "just now".into(),
        45..=3599 => format!("{}m ago", (d + 30) / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        86_400..=2_591_999 => format!("{}d ago", d / 86_400),
        _ => abs_date(t),
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 3 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(abs(0), "1970-01-01 00:00");
        assert_eq!(abs(1_700_000_000), "2023-11-14 22:13");
        assert_eq!(abs(951_782_400), "2000-02-29 00:00");
        assert_eq!(abs_date(-86_400), "1969-12-31");
    }

    #[test]
    fn relative_buckets() {
        let n = 1_000_000;
        assert_eq!(rel(n, n - 10), "just now");
        assert_eq!(rel(n, n - 300), "5m ago");
        assert_eq!(rel(n, n - 7200), "2h ago");
        assert_eq!(rel(n, n - 3 * 86_400), "3d ago");
        assert_eq!(rel(n, n - 90 * 86_400), abs_date(n - 90 * 86_400));
    }

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(10), "10 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }
}
