//! The part of a CI job's log worth reading: the lines that lead up to its last error.

/// At most `lines` lines of a GitHub Actions job log, ending at its last `##[error]` line (or
/// at the end when there is none), without timestamps, colors or group markers.
pub fn log_excerpt(log: &str, lines: i64) -> String {
    let raw: Vec<&str> = log.lines().collect();
    let end = raw
        .iter()
        .rposition(|line| line.contains("##[error]"))
        .map_or(raw.len(), |at| at + 1);
    let start = end.saturating_sub(usize::try_from(lines).unwrap_or(0));
    let cleaned: Vec<String> = raw[start..end].iter().copied().map(clean_line).collect();
    let mut excerpt = cleaned.join("\n");
    excerpt.push('\n');
    excerpt
}

fn clean_line(line: &str) -> String {
    // `2026-11-16T10:00:00.1234567Z text`
    let line = match line.split_once(' ') {
        Some((stamp, rest)) if stamp.len() >= 20 && stamp.ends_with('Z') && stamp.contains('T') => {
            rest
        }
        _ => line,
    };
    let line = line
        .strip_prefix("##[group]")
        .or_else(|| line.strip_prefix("##[endgroup]"))
        .unwrap_or(line);
    let line = line.replace("##[error]", "error: ");
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ parameters final-byte
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn an_excerpt_ends_at_the_last_error_without_noise() {
        let log = "2026-11-16T10:00:00.0000001Z ##[group]Run cargo test\n\
                   2026-11-16T10:00:01.0000001Z \u{1b}[1m\u{1b}[31merror[E0308]\u{1b}[0m: mismatched types\n\
                   2026-11-16T10:00:01.0000002Z  --> src/lib.rs:3:5\n\
                   2026-11-16T10:00:02.0000001Z ##[error]Process completed with exit code 101.\n\
                   2026-11-16T10:00:03.0000001Z Post job cleanup.\n";
        assert_eq!(
            log_excerpt(log, 3),
            "error[E0308]: mismatched types\n --> src/lib.rs:3:5\nerror: Process completed with exit code 101.\n"
        );
        assert_eq!(log_excerpt("a\nb\nc\n", 2), "b\nc\n");
    }
}
