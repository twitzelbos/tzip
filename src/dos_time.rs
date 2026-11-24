//! Shared date / time helpers. Consolidates the `civil_to_unix` /
//! `civil_from_days` / `dos_to_unix` / `unix_to_dos` functions that
//! previously lived (identically) in grep.rs, find.rs, diff.rs,
//! reader.rs, cli.rs, walker.rs, bulk_walker.rs, mount/vfs.rs, and
//! sources/s3.rs. All based on Howard Hinnant's `days_from_civil`
//! algorithm (public domain).

/// Days-from-civil: convert a Gregorian `(year, month, day)` triple to
/// a Unix timestamp at UTC midnight on that date. `y` may be negative;
/// `m` is 1..=12; `d` is 1..=31.
pub fn civil_to_unix(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = m as u64;
    let d = d as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    days * 86_400
}

/// Inverse of Howard Hinnant's `days_from_civil`: convert days since
/// 1970-01-01 to `(year, month, day)`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Decode a ZIP DOS `(date, time)` tuple to Unix seconds at UTC.
/// Clamps `month` / `day` to at least 1 so a garbage CDR entry doesn't
/// panic in `civil_to_unix` — the downstream display just shows
/// 1980-01-01 in that case.
pub fn dos_to_unix((date, time): (u16, u16)) -> i64 {
    let year = 1980 + ((date >> 9) & 0x7F) as i64;
    let month = ((date >> 5) & 0x0F) as u32;
    let day = (date & 0x1F) as u32;
    let hour = ((time >> 11) & 0x1F) as i64;
    let minute = ((time >> 5) & 0x3F) as i64;
    let sec = ((time & 0x1F) << 1) as i64;
    let month = month.max(1);
    let day = day.max(1);
    civil_to_unix(year, month, day) + hour * 3600 + minute * 60 + sec
}

/// Encode a Unix timestamp as a ZIP DOS `(date, time)` tuple. Returns
/// `1980-01-01 00:00:00` for anything before the MS-DOS epoch.
pub fn unix_to_dos(unix: i64) -> (u16, u16) {
    let secs = if unix < 0 { 0i64 } else { unix };
    let days = secs / 86_400;
    let sec_of_day = (secs % 86_400) as u32;
    let (y, m, d) = civil_from_days(days);
    let year = y as i32;
    if year < 1980 {
        // DOS date encoding: (year-1980) << 9 | month << 5 | day.
        // 1980-01-01 00:00:00 = (0 << 9) | (1 << 5) | 1 = 33.
        // (The old pre-consolidation call sites had a `(1 << 9) | (1 << 5)`
        // clamp with a misleading "1980-01-01" comment — that encoded
        // 1981-01-00, which is nonsense. This is the fix.)
        return ((1 << 5) | 1, 0);
    }
    let dos_date = (((year - 1980) as u16) << 9) | ((m as u16) << 5) | (d as u16);
    let hour = sec_of_day / 3600;
    let minute = (sec_of_day / 60) % 60;
    let sec2 = (sec_of_day % 60) / 2;
    let dos_time = ((hour as u16) << 11) | ((minute as u16) << 5) | (sec2 as u16);
    (dos_date, dos_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dos_to_unix_epoch() {
        // 1980-01-01 00:00:00 UTC = 315532800
        assert_eq!(dos_to_unix((0x0021, 0)), 315_532_800);
    }

    #[test]
    fn civil_round_trip() {
        for days_since_epoch in [0i64, 1, 365, 10_000, 20_000, 30_000, -1, -365] {
            let (y, m, d) = civil_from_days(days_since_epoch);
            let secs = civil_to_unix(y, m, d);
            assert_eq!(secs / 86_400, days_since_epoch, "y={y} m={m} d={d}");
        }
    }

    #[test]
    fn dos_round_trip_known_values() {
        // 2026-11-23 15:30:42 → round-trip through DOS encoding.
        // DOS stores seconds at 2-second resolution so the low bit is lost.
        let ts = civil_to_unix(2026, 11, 23) + 15 * 3600 + 30 * 60 + 42;
        let dos = unix_to_dos(ts);
        let back = dos_to_unix(dos);
        // 42 rounds to 42 (even); check we land at a multiple of 2.
        assert_eq!(back, ts - (ts % 2));
    }

    #[test]
    fn unix_to_dos_clamps_pre_1980() {
        let (date, time) = unix_to_dos(0); // 1970-01-01
        // date encodes 1980-01-01 (year=0, month=1, day=1).
        assert_eq!(date, (1 << 5) | 1);
        assert_eq!(time, 0);
    }
}
