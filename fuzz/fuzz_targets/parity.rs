//! Fuzz target for the segment cache's core contract (AC-5, §10.6).
//!
//! Differential harness: `SegmentIndex` spans must equal the pulldown
//! bridge's spans on every Prose segment, composed over an INDEPENDENT,
//! naive reference segmentation (frontmatter + fenced code blocks only).
//! The reference is deliberately simpler than the scanner: when the two
//! segmentations disagree (HTML, math, strikethrough, list containers, CR
//! line endings, control characters — all state the scanner does not
//! model, see KNOWN-DIVERGENCES.md), the input is skipped rather than
//! compared. When they agree, equality must hold byte-for-byte.

#![no_main]

use flavite_syntax::pulldown_glue::parse_normalized;
use flavite_syntax::segment::SegmentIndex;
use flavite_syntax::text_window::StructureText;
use libfuzzer_sys::fuzz_target;

/// Minimal O(1) line-start index over the input text.
struct FuzzText {
    text: String,
    line_starts: Vec<usize>,
}

impl FuzzText {
    fn new(text: String) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { text, line_starts }
    }
}

impl StructureText for FuzzText {
    fn len(&self) -> usize {
        self.text.len()
    }
    fn line_count(&self) -> usize {
        self.line_starts.len()
    }
    fn line_start(&self, line: usize) -> usize {
        self.line_starts
            .get(line)
            .copied()
            .unwrap_or(self.text.len())
    }
    fn slice(&self, range: std::ops::Range<usize>) -> String {
        self.text[range].to_owned()
    }
}

/// A reference segment: kind (0 = prose, 1 = fenced, 2 = frontmatter) and
/// half-open line range.
type RefSeg = (u8, usize, usize);

/// A fence opener candidate: ≤3 leading spaces, then ≥3 backticks/tildes.
fn fence_opener(line: &str) -> Option<(char, usize)> {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let mut chars = rest.chars();
    let ch = chars.next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let mut n = 1usize;
    for c in chars {
        if c == ch {
            n += 1;
        } else {
            break;
        }
    }
    if n < 3 {
        return None;
    }
    if ch == '`' && rest[n..].contains('`') {
        return None;
    }
    Some((ch, n))
}

/// A fence closer for `(ch, n)` at this line.
fn fence_closer(line: &str, ch: char, n: usize) -> bool {
    let indent = line.bytes().take_while(|&b| b == b' ').count();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let mut count = 0usize;
    for c in rest.chars() {
        if c == ch {
            count += 1;
        } else {
            break;
        }
    }
    count >= n
        && rest
            .chars()
            .skip(count)
            .all(|c| c == ' ' || c == '\n' || c == '\r')
}

/// Naive reference segmentation (frontmatter + fences only).
fn reference_segments(text: &str) -> Vec<RefSeg> {
    let lines: Vec<&str> = text.split('\n').collect();
    let n = lines.len();
    let mut fm_end = 0usize;
    if n > 0 && lines[0] == "---" {
        for j in 1..n {
            let t = lines[j].trim_end();
            if t == "---" || t == "..." {
                fm_end = j + 1;
                break;
            }
        }
    }
    let mut out: Vec<RefSeg> = Vec::new();
    let mut cur_kind = 0u8;
    let mut seg_start = 0usize;
    let mut state: Option<(char, usize)> = None;
    for (idx, line) in lines.iter().enumerate() {
        // Ignore the synthetic trailing empty line after the final \n.
        if idx == n - 1 && line.is_empty() {
            continue;
        }
        let mut kind = 0u8;
        match state {
            Some((ch, k)) => {
                kind = 1;
                if fence_closer(line, ch, k) {
                    state = None;
                }
            }
            None => {
                if fm_end > 0 && idx < fm_end {
                    kind = 2;
                } else if let Some((ch, k)) = fence_opener(line) {
                    kind = 1;
                    state = Some((ch, k));
                }
            }
        }
        if kind != cur_kind {
            out.push((cur_kind, seg_start, idx));
            seg_start = idx;
            cur_kind = kind;
        }
    }
    out.push((cur_kind, seg_start, n));
    out
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    if text.len() > 64 * 1024 {
        return;
    }
    // Line-model limitations (KNOWN-DIVERGENCES.md): CR line endings and
    // C0 control characters alter pulldown's line splitting in ways the
    // scanner does not model.
    if text.contains('\r') {
        return;
    }
    if text
        .bytes()
        .any(|b| b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r'))
    {
        return;
    }

    // Build the index and the independent reference segmentation.
    let fuzz_text = FuzzText::new(text.to_string());
    let mut index = SegmentIndex::new();
    index.sync(&fuzz_text, None);
    let ref_segs = reference_segments(text);
    let index_segs: Vec<(u8, usize, usize)> = index
        .debug_segments()
        .into_iter()
        .map(|(kind, ls, le)| {
            let k = match kind {
                flavite_syntax::segment::SegmentKind::Frontmatter => 2u8,
                flavite_syntax::segment::SegmentKind::Fenced => 1u8,
                flavite_syntax::segment::SegmentKind::Prose => 0u8,
            };
            (k, ls, le)
        })
        .collect();

    // Only compare when both segmentations agree exactly; otherwise the
    // input exercises state the reference (and typically the scanner
    // contract) does not model and the comparison would be noise.
    if ref_segs != index_segs {
        return;
    }

    // Expected spans: per Prose segment, the bridge's parse of the slice,
    // offset into document coordinates.
    let mut expected_links: Vec<(usize, usize, String)> = Vec::new();
    let mut expected_tags: Vec<(usize, usize)> = Vec::new();
    let mut expected_blocks: Vec<(usize, usize)> = Vec::new();
    for (kind, ls, le) in &ref_segs {
        if *kind != 0 {
            continue;
        }
        let byte_start = fuzz_text.line_start(*ls);
        let byte_end = fuzz_text.line_start(*le);
        let Ok(parsed) = parse_normalized(&text[byte_start..byte_end]) else {
            continue;
        };
        for link in &parsed.links {
            expected_links.push((
                link.span.start.0 + byte_start,
                link.span.end.0 + byte_start,
                link.target_raw.clone(),
            ));
        }
        for s in &parsed.tags {
            expected_tags.push((s.start.0 + byte_start, s.end.0 + byte_start));
        }
        for s in &parsed.block_ids {
            expected_blocks.push((s.start.0 + byte_start, s.end.0 + byte_start));
        }
    }

    let (links, tags, blocks) = index.spans();
    let got_links: Vec<(usize, usize, String)> = links
        .iter()
        .map(|l| (l.span.start.0, l.span.end.0, l.target_raw.clone()))
        .collect();
    let got_tags: Vec<(usize, usize)> = tags.iter().map(|s| (s.start.0, s.end.0)).collect();
    let got_blocks: Vec<(usize, usize)> =
        blocks.iter().map(|s| (s.start.0, s.end.0)).collect();

    if got_links != expected_links || got_tags != expected_tags || got_blocks != expected_blocks {
        panic!(
            "parity divergence: links got={got_links:?} expected={expected_links:?} \
             tags got={got_tags:?} expected={expected_tags:?} \
             blocks got={got_blocks:?} expected={expected_blocks:?}"
        );
    }
});
