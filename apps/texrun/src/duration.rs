//! Parsing and formatting of `--timeout` durations.

use std::time::Duration;

/// Parses `<integer>[ms|s|m|h]` (no unit: seconds), e.g. `90`, `90s`, `2m`,
/// `1500ms`. Zero is rejected.
pub fn parse_timeout(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(digits_end);
    if number.is_empty() {
        return Err(format!(
            "invalid duration `{input}`: expected a number with an optional unit (ms, s, m, h), e.g. `90s` or `2m`"
        ));
    }
    let value: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration `{input}`: number too large"))?;
    let millis_per_unit: u64 = match unit {
        "ms" => 1,
        "" | "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => {
            return Err(format!(
                "invalid duration `{input}`: unknown unit `{unit}` (use ms, s, m or h)"
            ));
        }
    };
    let millis = value
        .checked_mul(millis_per_unit)
        .ok_or_else(|| format!("invalid duration `{input}`: too large"))?;
    if millis == 0 {
        return Err("the timeout must be greater than zero".to_owned());
    }
    Ok(Duration::from_millis(millis))
}

/// Formats a duration for humans: `850ms`, `1.23s`, `90s`, `2m`, `1m30s`.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    if ms < 60_000 {
        let secs = d.as_secs_f64();
        return if ms.is_multiple_of(1_000) {
            format!("{}s", ms / 1_000)
        } else {
            format!("{secs:.2}s")
        };
    }
    let secs = d.as_secs();
    match (secs / 60, secs % 60) {
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse_timeout("90"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_timeout("1h"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_timeout("1500ms"), Ok(Duration::from_millis(1500)));
    }

    #[test]
    fn rejects_invalid_values() {
        for bad in ["", "s", "-1s", "1.5s", "10 min", "10d", "0", "0ms", "0s"] {
            assert!(parse_timeout(bad).is_err(), "{bad:?}");
        }
        assert!(parse_timeout(&format!("{}h", u64::MAX)).is_err());
        assert!(parse_timeout("99999999999999999999999").is_err());
    }

    #[test]
    fn default_matches_the_engine_default() {
        assert_eq!(
            parse_timeout(crate::cli::DEFAULT_TIMEOUT_ARG),
            Ok(texrun_texlive::DEFAULT_TIMEOUT)
        );
    }

    #[test]
    fn formats_for_humans() {
        assert_eq!(format_duration(Duration::from_millis(850)), "850ms");
        assert_eq!(format_duration(Duration::from_millis(1234)), "1.23s");
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
        assert_eq!(format_duration(Duration::from_secs(60)), "1m");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m30s");
    }
}
