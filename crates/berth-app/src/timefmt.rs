//! Wall-clock times in the local time zone (`localtime_r`), for event
//! lists: `berth debug events` and the sidebar's hover details.

/// The local calendar time of `ms` since the epoch.
fn local_tm(ms: i64) -> Option<libc::tm> {
    let secs = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: an all-zero `tm` is valid; localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals for the call's duration.
    let ok = !unsafe { libc::localtime_r(&secs, &mut tm) }.is_null();
    ok.then_some(tm)
}

/// `YYYY-MM-DD HH:MM:SS.mmm`.
pub fn local_time(ms: i64) -> String {
    match local_tm(ms) {
        Some(tm) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            ms.rem_euclid(1000)
        ),
        None => format!("{ms} ms"),
    }
}

/// `HH:MM:SS`.
pub fn local_clock(ms: i64) -> String {
    match local_tm(ms) {
        Some(tm) => format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        None => format!("{ms} ms"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(s: &str) -> String {
        s.chars()
            .map(|c| if c.is_ascii_digit() { '9' } else { c })
            .collect()
    }

    #[test]
    fn formats_have_fixed_shapes() {
        let ms = 1_790_000_002_345;
        let full = local_time(ms);
        assert_eq!(shape(&full), "9999-99-99 99:99:99.999", "{full}");
        assert!(full.ends_with(".345"));
        let clock = local_clock(ms);
        assert_eq!(shape(&clock), "99:99:99", "{clock}");
        assert_eq!(&full[11..19], clock);
        // Before the epoch still formats (div_euclid keeps ms positive).
        assert!(local_time(-1).ends_with(".999"));
    }
}
