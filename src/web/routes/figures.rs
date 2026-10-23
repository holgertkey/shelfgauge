//! How figures are shown: grouped digits, file sizes and the width classes of bars.

pub(crate) use crate::render::outline::group_digits;

/// "1 book", "12 345 books": `n` with its digits grouped and the noun for it.
pub(crate) fn count(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", group_digits(n), if n == 1 { one } else { many })
}

/// A duration in seconds: "12 s", "1 min 28 s", "2 h 5 min".
pub(crate) fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min {} s", seconds / 60, seconds % 60),
        _ => format!("{} h {} min", seconds / 3600, seconds % 3600 / 60),
    }
}

/// A size in bytes with binary units: "512 B", "4.2 KB", "48 MB", "1 234 GB".
///
/// One decimal below 10 units, whole units above.
pub(crate) fn file_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    // Rounding may reach the next unit (1023.96 KB is "1.0 MB", not "1024 KB").
    if value.round() >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if (value * 10.0).round() < 100.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{} {}", group_digits(value.round() as u64), UNITS[unit])
    }
}

/// The CSS class of a bar `per_mille` / 1000 wide: `w0` … `w100` in steps of 5 (the
/// CSP allows no inline styles). A share above zero is at least `w5`, so it shows.
pub(crate) fn width_class(per_mille: u64) -> String {
    let per_mille = per_mille.min(1000);
    let mut step = (per_mille + 25) / 50 * 5;
    if per_mille > 0 && step == 0 {
        step = 5;
    }
    format!("w{step}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(500), "500");
        assert_eq!(group_digits(1234), "1\u{202f}234");
        assert_eq!(group_digits(1_234_567), "1\u{202f}234\u{202f}567");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(-1), "0 s");
        assert_eq!(duration(59), "59 s");
        assert_eq!(duration(88), "1 min 28 s");
        assert_eq!(duration(7500), "2 h 5 min");
    }

    #[test]
    fn file_sizes() {
        assert_eq!(file_size(0), "0 B");
        assert_eq!(file_size(1023), "1023 B");
        assert_eq!(file_size(1024), "1.0 KB");
        assert_eq!(file_size(1536), "1.5 KB");
        assert_eq!(file_size(48 * 1024 * 1024), "48 MB");
        assert_eq!(file_size(1024 * 1024 - 1), "1.0 MB");
        assert_eq!(file_size(4_509_715_660), "4.2 GB");
        assert_eq!(file_size((1 << 30) - 1), "1.0 GB");
        assert_eq!(file_size(10 * (1 << 30) - 1), "10 GB");
        assert_eq!(file_size(u64::MAX), "16\u{202f}384 PB");
    }

    #[test]
    fn width_classes() {
        assert_eq!(width_class(0), "w0");
        assert_eq!(width_class(1), "w5");
        assert_eq!(width_class(24), "w5");
        assert_eq!(width_class(25), "w5");
        assert_eq!(width_class(74), "w5");
        assert_eq!(width_class(75), "w10");
        assert_eq!(width_class(379), "w40");
        assert_eq!(width_class(999), "w100");
        assert_eq!(width_class(1000), "w100");
        assert_eq!(width_class(5000), "w100");
    }
}
