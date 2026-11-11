//! UTC timestamps as milliseconds since the Unix epoch, for transcripts' ISO 8601 strings.

/// Parses `2026-11-11T09:42:02.123Z`; the fraction is optional and the offset is `Z`, `±hh:mm` or
/// absent (UTC).
pub fn parse(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b' ') {
        return None;
    }
    let num = |from: usize, to: usize| s.get(from..to)?.parse::<i64>().ok();
    let days = days_from_civil(num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let mut ms = ((days * 24 + num(11, 13)?) * 60 + num(14, 16)?) * 60_000 + num(17, 19)? * 1000;
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        let millis = &frac[..digits.min(3)];
        ms += format!("{millis:0<3}").parse::<i64>().ok()?;
        rest = &frac[digits..];
    }
    if !rest.is_empty() && rest != "Z" {
        let sign = match rest.as_bytes()[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        let minutes =
            rest.get(1..3)?.parse::<i64>().ok()? * 60 + rest.get(4..6)?.parse::<i64>().ok()?;
        ms -= sign * minutes * 60_000;
    }
    Some(ms)
}

/// Parses a `YYYY-MM-DD` date as the start of that day, UTC.
pub fn parse_date(s: &str) -> Option<i64> {
    (s.len() == 10).then(|| parse(&format!("{s}T00:00:00Z")))?
}

/// `YYYY-MM-DD HH:MM`, UTC.
pub fn format(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3_600_000,
        rem / 60_000 % 60
    )
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_utc() {
        let t = parse("2026-11-11T09:42:02.123Z").unwrap();
        assert_eq!(t % 1000, 123);
        assert_eq!(format(t), "2026-11-11 09:42");
        assert_eq!(parse("2026-11-11T10:42:02.123+01:00"), Some(t));
        assert_eq!(parse("2026-11-11T09:42:02.1234567Z"), Some(t));
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(
            format(parse_date("2024-02-29").unwrap()),
            "2024-02-29 00:00"
        );
        assert_eq!(parse("yesterday"), None);
    }
}
