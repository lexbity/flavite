//! Bounded source excerpts over the frozen text used by the matcher.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchContext {
    pub start: u64,
    pub end: u64,
    pub line: usize,
    pub column: usize,
    pub before: String,
    pub matched: String,
    pub after: String,
}

/// A single character/line index serves every hit on a page; long lines do
/// not cause repeated scans from the beginning of the source.
pub fn excerpts(text: &str, spans: &[(u64, u64)]) -> Result<Vec<MatchContext>, String> {
    let chars: Vec<_> = text
        .char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(text.len()))
        .collect();
    let lines: Vec<_> = std::iter::once(0)
        .chain(
            text.bytes()
                .enumerate()
                .filter_map(|(at, byte)| (byte == b'\n').then_some(at + 1)),
        )
        .collect();
    spans
        .iter()
        .map(|&(start, end)| {
            let from = usize::try_from(start).map_err(|_| "Invalid match offset")?;
            let to = usize::try_from(end).map_err(|_| "Invalid match offset")?;
            if from > to
                || to > text.len()
                || !text.is_char_boundary(from)
                || !text.is_char_boundary(to)
            {
                return Err("Invalid match span".into());
            }
            let first = chars.partition_point(|&at| at < from);
            let last = chars.partition_point(|&at| at < to);
            let line = lines.partition_point(|&at| at <= from).saturating_sub(1);
            let line_char = chars.partition_point(|&at| at < lines[line]);
            let begin = chars[first.saturating_sub(64).max(line_char)];
            let suffix_line = lines.partition_point(|&at| at <= to).saturating_sub(1);
            let line_end = lines
                .get(suffix_line + 1)
                .map_or(text.len(), |&at| at.saturating_sub(1));
            let finish = chars[(last + 64).min(chars.len() - 1)]
                .min(line_end)
                .max(to);
            let match_end = chars[(first + 96).min(last)];
            Ok(MatchContext {
                start,
                end,
                line: line + 1,
                column: first - line_char + 1,
                before: format!(
                    "{}{}",
                    if begin > lines[line] { "…" } else { "" },
                    &text[begin..from]
                ),
                matched: format!(
                    "{}{}",
                    &text[from..match_end],
                    if match_end < to { "…" } else { "" }
                ),
                after: format!(
                    "{}{}",
                    &text[to..finish],
                    if finish < line_end { "…" } else { "" }
                ),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacementDiff {
    pub before: String,
    pub after: String,
    pub first_line: usize,
    pub truncated: bool,
}

/// Readable context around the first actual difference, bounded per side.
pub fn diff(before: &str, after: &str) -> ReplacementDiff {
    let mut prefix = before
        .bytes()
        .zip(after.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !before.is_char_boundary(prefix) || !after.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let line_start = before[..prefix].rfind('\n').map_or(0, |at| at + 1);
    let start = before[..prefix]
        .char_indices()
        .rev()
        .nth(256)
        .map_or(line_start, |(at, _)| at.max(line_start));
    let take = |text: &str| {
        let remaining = &text[start.min(text.len())..];
        let end = remaining
            .char_indices()
            .nth(4096)
            .map_or(remaining.len(), |(at, _)| at);
        (remaining[..end].to_owned(), end < remaining.len())
    };
    let first_line = 1 + before[..start]
        .bytes()
        .filter(|&byte| byte == b'\n')
        .count();
    let (before, clipped_before) = take(before);
    let (after, clipped_after) = take(after);
    ReplacementDiff {
        first_line,
        before,
        after,
        truncated: clipped_before || clipped_after || start > line_start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_keep_unicode_multiline_and_eof_offsets() {
        let text = "α\n🙂x\n終";
        let values = excerpts(text, &[(3, 9), (12, 12)]).unwrap();
        assert_eq!((values[0].line, values[0].column), (2, 1));
        assert_eq!(values[0].matched, "🙂x\n");
        assert_eq!(values[0].after, "終");
        assert_eq!((values[1].line, values[1].column), (3, 2));
        assert_eq!(values[1].matched, "");
        assert!(excerpts(text, &[(4, 7)]).is_err());
        assert!(excerpts(text, &[(9, 3)]).is_err());
        assert!(excerpts(text, &[(0, 13)]).is_err());
    }

    #[test]
    fn long_excerpts_and_diffs_are_bounded_around_the_change() {
        let source = "α".repeat(10_000);
        let context = excerpts(&source, &[(10_000, 11_000)]).unwrap().remove(0);
        assert_eq!(context.before.chars().count(), 65);
        assert_eq!(context.matched.chars().count(), 97);
        assert_eq!(context.after.chars().count(), 65);
        let after = format!("{}NEW{}", &source[..10_000], &source[10_002..]);
        let shown = diff(&source, &after);
        assert!(shown.after.contains("NEW"));
        assert!(shown.truncated);
        assert!(shown.before.chars().count() <= 4096);
        assert!(shown.after.chars().count() <= 4096);
    }
}
