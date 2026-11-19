use std::time::Duration;

pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.2} {}", UNITS[i])
    }
}

pub fn count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Unix seconds as a local "yyyy-mm-dd hh:mm".
pub fn date(unix: u32) -> String {
    if unix == 0 {
        return String::new();
    }
    let ft = crate::win::to_local_filetime((unix as u64 + 11_644_473_600) * 10_000_000);
    let secs = (ft / 10_000_000) as i64 - 11_644_473_600;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", t / 3600, t % 3600 / 60)
}

/// "just now", "5 min ago", "3 h ago", "yesterday", "4 days ago", or the date.
pub fn ago(unix: u32) -> String {
    if unix == 0 {
        return String::new();
    }
    let secs = crate::index::now().saturating_sub(unix);
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        86_400..172_800 => "yesterday".into(),
        172_800..2_592_000 => format!("{} days ago", secs / 86_400),
        _ => date(unix).get(..10).unwrap_or_default().to_string(),
    }
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
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

pub fn duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if d < Duration::from_millis(1) {
        "<1 ms".into()
    } else if s < 1.0 {
        format!("{} ms", d.as_millis())
    } else if s < 60.0 {
        format!("{s:.1} s")
    } else {
        format!("{} min {} s", d.as_secs() / 60, d.as_secs() % 60)
    }
}
