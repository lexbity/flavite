//! Revision-independent character metadata for mounted native text views.
//! Full trees still carry revision-guarded node IDs; only immutable line
//! segmentation is reused across frames, split views, and unchanged lines.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Default)]
pub(crate) struct EditorTextCache {
    entries: BTreeMap<u64, TextProjection>,
}

pub(super) struct TextProjection {
    pub text: Arc<str>,
    pub lines: Vec<LineProjection>,
}

pub(super) struct LineProjection {
    pub start: usize,
    pub end: usize,
    pub lengths: Arc<[u8]>,
}

impl EditorTextCache {
    pub(super) fn project(&mut self, buffer: u64, text: &Arc<str>) -> &TextProjection {
        let entry = self
            .entries
            .entry(buffer)
            .or_insert_with(|| TextProjection::new(text.clone(), None));
        if !Arc::ptr_eq(&entry.text, text) {
            *entry = TextProjection::new(text.clone(), Some(entry));
        }
        entry
    }

    pub(super) fn retain(&mut self, mounted: &BTreeSet<u64>) {
        self.entries.retain(|buffer, _| mounted.contains(buffer));
    }
}

impl TextProjection {
    fn new(text: Arc<str>, previous: Option<&Self>) -> Self {
        let lines = super::tree::text_lines(&text);
        let mut prefix = 0;
        let mut suffix = 0;
        if let Some(old) = previous {
            let equal = |new: usize, old_index: usize| {
                let old_line = &old.lines[old_index];
                lines[new].1 == &old.text[old_line.start..old_line.end]
            };
            while prefix < lines.len() && prefix < old.lines.len() && equal(prefix, prefix) {
                prefix += 1;
            }
            while suffix < lines.len() - prefix
                && suffix < old.lines.len() - prefix
                && equal(lines.len() - 1 - suffix, old.lines.len() - 1 - suffix)
            {
                suffix += 1;
            }
        }
        let lines = lines
            .iter()
            .enumerate()
            .map(|(index, (start, line))| {
                let reused = previous.and_then(|old| {
                    if index < prefix {
                        Some(&old.lines[index])
                    } else if index >= lines.len() - suffix {
                        Some(&old.lines[old.lines.len() - (lines.len() - index)])
                    } else {
                        None
                    }
                });
                LineProjection {
                    start: *start,
                    end: start + line.len(),
                    lengths: reused.map_or_else(
                        || flavite_editor::viewport::accessible_character_lengths(line).into(),
                        |old| old.lengths.clone(),
                    ),
                }
            })
            .collect();
        Self { text, lines }
    }

    pub(super) fn position(&self, byte: usize) -> (usize, usize) {
        let line = self
            .lines
            .partition_point(|line| line.start <= byte)
            .saturating_sub(1);
        let local = byte.saturating_sub(self.lines[line].start);
        let mut end = 0;
        let character = self.lines[line]
            .lengths
            .iter()
            .take_while(|length| {
                end += usize::from(**length);
                end <= local
            })
            .count();
        (line, character)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insertion_reuses_unicode_lines_and_maps_shifted_selection() {
        let mut cache = EditorTextCache::default();
        let before: Arc<str> = "a🧑🏽‍💻\nold\ne\u{301}\n".into();
        let old = cache.project(7, &before);
        let first = old.lines[0].lengths.clone();
        let last = old.lines[2].lengths.clone();
        let after: Arc<str> = "a🧑🏽‍💻\nnew\ninserted\ne\u{301}\n".into();
        let new = cache.project(7, &after);
        assert!(Arc::ptr_eq(&first, &new.lines[0].lengths));
        assert!(Arc::ptr_eq(&last, &new.lines[3].lengths));
        for byte in after
            .char_indices()
            .map(|(byte, _)| byte)
            .chain([after.len()])
        {
            let (line, character) = new.position(byte);
            assert_eq!(
                character,
                super::super::tree::byte_to_char_index(
                    &after[new.lines[line].start..new.lines[line].end],
                    byte - new.lines[line].start,
                )
            );
        }
        cache.retain(&BTreeSet::new());
        assert!(cache.entries.is_empty());
    }
}
