//! Plain-text rendering helpers shared by the CLI reports.

use std::fmt::Display;

/// Aligned rows: the label column left-aligned, every cell column right-aligned, indented two
/// spaces.
pub fn table(rows: &[(String, Vec<String>)]) -> String {
    let label = rows
        .iter()
        .map(|(l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    let cols = rows.first().map_or(0, |(_, c)| c.len());
    let widths: Vec<usize> = (0..cols)
        .map(|i| {
            rows.iter()
                .map(|(_, c)| c[i].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|(l, cells)| {
            let cells: Vec<String> = cells
                .iter()
                .zip(&widths)
                .map(|(c, w)| format!("{c:>w$}"))
                .collect();
            format!("  {l:<label$}  {}", cells.join("  "))
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A count with thousands separators: `58210` → `58,210`.
pub fn count(n: impl Display) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// How long before `now` the Unix time `then` was, in its two largest units: `6m 31s ago`,
/// `2d 4h ago`. `0` is `never`.
pub fn ago(then: u64, now: u64) -> String {
    if then == 0 {
        return "never".into();
    }
    let secs = now.saturating_sub(then);
    if secs == 0 {
        return "just now".into();
    }
    const UNITS: [(u64, &str); 4] = [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")];
    let parts: Vec<String> = UNITS
        .iter()
        .scan(secs, |rest, &(size, unit)| {
            let n = *rest / size;
            *rest %= size;
            Some((n, unit))
        })
        .skip_while(|(n, _)| *n == 0)
        .take(2)
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect();
    format!("{} ago", parts.join(" "))
}

/// The current Unix time in seconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(58_210usize), "58,210");
        assert_eq!(count(1_234_567u64), "1,234,567");
    }

    #[test]
    fn ago_uses_the_two_largest_units() {
        assert_eq!(ago(0, 100), "never");
        assert_eq!(ago(100, 100), "just now");
        assert_eq!(ago(100, 145), "45s ago");
        assert_eq!(ago(100, 100 + 391), "6m 31s ago");
        assert_eq!(ago(100, 100 + 3_600), "1h ago");
        assert_eq!(ago(100, 100 + 3_600 + 59), "1h ago");
        assert_eq!(ago(100, 100 + 2 * 86_400 + 4 * 3_600 + 7), "2d 4h ago");
        assert_eq!(ago(200, 100), "just now");
    }

    #[test]
    fn table_aligns_labels_left_and_cells_right() {
        let rows = vec![
            (String::new(), vec!["tagged".into(), "text".into()]),
            (".rs".into(), vec!["12,345".into(), "0".into()]),
            (".py".into(), vec!["7".into(), "".into()]),
        ];
        assert_eq!(
            table(&rows),
            "       tagged  text\n  .rs  12,345     0\n  .py       7"
        );
    }
}
