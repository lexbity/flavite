#![no_main]
use libfuzzer_sys::fuzz_target;

use flavite_text_search::{MatchLimits, MatchMode, MatchOptions, Matcher, Span, TextByte};

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 || data.len() > 64 * 1024 { return; }
    let mode = if data[0] & 1 == 0 { MatchMode::Literal } else { MatchMode::Regex };
    let case_sensitive = data[0] & 2 != 0;
    let whole_word = data[0] & 4 != 0;
    let pattern_len = usize::from(u16::from_le_bytes([data[1], data[2]])).min(8192);
    if 3 + pattern_len > data.len() { return; }
    let (Ok(pattern), Ok(source)) = (
        std::str::from_utf8(&data[3..3 + pattern_len]),
        std::str::from_utf8(&data[3 + pattern_len..]),
    ) else { return; };
    if source.len() > 32 * 1024 * 1024 { return; }
    let Ok(matcher) = Matcher::new(pattern, MatchOptions { mode, case_sensitive, whole_word }, MatchLimits { max_matches: 4096 }) else { return; };
    let mut previous_end = 0;
    let mut count = 0;
    for item in matcher.iter_matches(source, None).expect("valid full scope") {
        let Ok(hit) = item else { break; };
        let Span { start: TextByte(start), end: TextByte(end) } = hit.span;
        assert!(start <= end && end <= source.len());
        assert!(source.is_char_boundary(start) && source.is_char_boundary(end));
        assert!(start >= previous_end, "matches must be ascending and non-overlapping");
        previous_end = end.max(start);
        count += 1;
        assert!(count <= 4097, "match budget must terminate the iterator");
    }
});
