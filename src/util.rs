use std::path::{Path, PathBuf};

pub fn euid() -> u32 {
    rustix::process::geteuid().as_raw()
}

pub fn is_root() -> bool {
    euid() == 0
}

/// Unix seconds now, or `$DISKEYE_NOW` when set (for reproducible screenshots).
pub fn now_secs() -> i64 {
    static FIXED: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    if let Some(t) = *FIXED.get_or_init(|| std::env::var("DISKEYE_NOW").ok().and_then(|v| v.parse().ok())) {
        return t;
    }
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Civil date from days since epoch (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// `2026-10-05 14:03` in UTC.
/// `2026-10-05 14:03` in local time.
pub fn timestamp_human(secs: i64) -> String {
    let t: libc::time_t = secs;
    // SAFETY: localtime_r only writes into the tm we pass.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        let (y, m, d) = civil(secs.div_euclid(86_400));
        let s = secs.rem_euclid(86_400);
        return format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", s / 3600, (s % 3600) / 60);
    }
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

/// `20261005T140312Z`, sortable and filename-safe.
pub fn timestamp_compact(secs: i64) -> String {
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, (s % 3600) / 60, s % 60)
}

pub fn age_human(secs: i64) -> String {
    let d = (now_secs() - secs).max(0);
    match d {
        0..=119 => format!("{d}s"),
        120..=7199 => format!("{}m", d / 60),
        7200..=172_799 => format!("{}h", d / 3600),
        172_800..=5_183_999 => format!("{}d", d / 86_400),
        _ => format!("{}mo", d / 2_592_000),
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "localhost".into())
}

pub fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default()
}

/// The (uid, gid) of the human behind this process: SUDO_UID/SUDO_GID under sudo.
pub fn invoking_ids() -> (u32, u32) {
    let parse = |k: &str| std::env::var(k).ok().and_then(|v| v.parse().ok());
    match (parse("SUDO_UID"), parse("SUDO_GID")) {
        (Some(u), Some(g)) if is_root() => (u, g),
        _ => (euid(), rustix::process::getegid().as_raw()),
    }
}

pub fn sudo_user() -> Option<String> {
    is_root().then(|| std::env::var("SUDO_USER").ok()).flatten()
}

pub fn home_of_uid(uid: u32) -> Option<PathBuf> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 6 && f[2].parse() == Ok(uid)).then(|| PathBuf::from(f[5]))
    })
}

pub fn user_name(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 3 && f[2].parse() == Ok(uid)).then(|| f[0].to_string())
    })
}

/// Home directory of the invoking user (not root's home when running under sudo).
pub fn invoking_home() -> PathBuf {
    let (uid, _) = invoking_ids();
    if uid != euid()
        && let Some(h) = home_of_uid(uid)
    {
        return h;
    }
    std::env::var_os("HOME").map(PathBuf::from).or_else(|| home_of_uid(uid)).unwrap_or_else(|| "/".into())
}

/// Real (non-system) users with a home directory: (uid, name, home).
pub fn human_users() -> Vec<(u32, String, PathBuf)> {
    let Ok(passwd) = std::fs::read_to_string("/etc/passwd") else { return vec![] };
    passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            let uid: u32 = f[2].parse().ok()?;
            let shell = f[6];
            let human = (1000..60000).contains(&uid) || uid == 0;
            (human && !shell.ends_with("nologin") && !shell.ends_with("false"))
                .then(|| (uid, f[0].to_string(), PathBuf::from(f[5])))
        })
        .collect()
}

/// When running under sudo, hand files we create back to the invoking user.
pub fn chown_to_invoker(path: &Path) {
    if !is_root() {
        return;
    }
    let (uid, gid) = invoking_ids();
    if uid == 0 {
        return;
    }
    let _ = std::os::unix::fs::chown(path, Some(uid), Some(gid));
}

pub fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Decode the octal escapes used in /proc/self/mountinfo and /proc/swaps.
pub fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            let v = (b[i + 1] - b'0') as u32 * 64 + (b[i + 2] - b'0') as u32 * 8 + (b[i + 3] - b'0') as u32;
            if let Ok(v) = u8::try_from(v) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(timestamp_compact(0), "19700101T000000Z");
        assert_eq!(timestamp_compact(1_791_208_800), "20261005T140000Z");
    }

    #[test]
    fn octal() {
        assert_eq!(unescape_octal(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(unescape_octal(r"/plain"), "/plain");
        assert_eq!(unescape_octal(r"/trail\04"), r"/trail\04");
    }
}
