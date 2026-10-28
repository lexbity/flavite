//! Conservative line-based three-way merge (D7).
//!
//! Non-overlapping edits are combined. Overlapping edits are returned as
//! structured hunks for explicit resolution; this module never writes marker
//! text into a document.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeConflict {
    pub base_start_line: usize,
    pub base_end_line: usize,
    pub mine: Vec<String>,
    pub theirs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    /// Present only when all hunks could be merged without overlap.
    pub merged: Option<String>,
    pub conflicts: Vec<MergeConflict>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    start: usize,
    end: usize,
    replacement: Vec<String>,
}

pub fn three_way_merge(base: &str, mine: &str, theirs: &str) -> MergeResult {
    if mine == theirs {
        return MergeResult {
            merged: Some(mine.to_owned()),
            conflicts: vec![],
        };
    }
    if mine == base {
        return MergeResult {
            merged: Some(theirs.to_owned()),
            conflicts: vec![],
        };
    }
    if theirs == base {
        return MergeResult {
            merged: Some(mine.to_owned()),
            conflicts: vec![],
        };
    }
    let b: Vec<&str> = base.split_inclusive('\n').collect();
    let m: Vec<&str> = mine.split_inclusive('\n').collect();
    let t: Vec<&str> = theirs.split_inclusive('\n').collect();
    let mut edits = diff_hunks(&b, &m)
        .into_iter()
        .map(|h| (h, true))
        .chain(diff_hunks(&b, &t).into_iter().map(|h| (h, false)))
        .collect::<Vec<_>>();
    edits.sort_by_key(|(h, side)| (h.start, h.end, *side));

    let mut conflicts = Vec::new();
    let mut selected = Vec::<Hunk>::new();
    let mut i = 0;
    while i < edits.len() {
        let (left, left_side) = edits[i].clone();
        let mut group = vec![(left, left_side)];
        i += 1;
        while i < edits.len() && overlaps(&group[0].0, &edits[i].0) {
            group.push(edits[i].clone());
            i += 1;
        }
        if group.len() == 1 {
            selected.push(group[0].0.clone());
            continue;
        }
        let mine_hunks: Vec<_> = group.iter().filter(|(_, s)| *s).map(|(h, _)| h).collect();
        let their_hunks: Vec<_> = group.iter().filter(|(_, s)| !*s).map(|(h, _)| h).collect();
        if mine_hunks.is_empty() || their_hunks.is_empty() {
            selected.extend(group.into_iter().map(|(h, _)| h));
            continue;
        }
        let mut mine_repl = Vec::new();
        let mut their_repl = Vec::new();
        for h in &mine_hunks {
            mine_repl.extend(h.replacement.clone());
        }
        for h in &their_hunks {
            their_repl.extend(h.replacement.clone());
        }
        if mine_repl == their_repl {
            selected.push(Hunk {
                start: group.iter().map(|(h, _)| h.start).min().unwrap_or(0),
                end: group.iter().map(|(h, _)| h.end).max().unwrap_or(0),
                replacement: mine_repl,
            });
        } else {
            conflicts.push(MergeConflict {
                base_start_line: group.iter().map(|(h, _)| h.start).min().unwrap_or(0),
                base_end_line: group.iter().map(|(h, _)| h.end).max().unwrap_or(0),
                mine: mine_repl,
                theirs: their_repl,
            });
        }
    }
    if !conflicts.is_empty() {
        return MergeResult {
            merged: None,
            conflicts,
        };
    }
    selected.sort_by_key(|h| (h.start, h.end));
    let mut out = Vec::new();
    let mut cursor = 0;
    for h in selected {
        if h.start < cursor {
            return MergeResult {
                merged: None,
                conflicts: vec![MergeConflict {
                    base_start_line: h.start,
                    base_end_line: h.end,
                    mine: h.replacement,
                    theirs: vec![],
                }],
            };
        }
        out.extend(b[cursor..h.start].iter().map(|s| (*s).to_owned()));
        out.extend(h.replacement);
        cursor = h.end;
    }
    out.extend(b[cursor..].iter().map(|s| (*s).to_owned()));
    MergeResult {
        merged: Some(out.concat()),
        conflicts: vec![],
    }
}

fn overlaps(a: &Hunk, b: &Hunk) -> bool {
    if a.start == a.end && b.start == b.end {
        return a.start == b.start;
    }
    a.start < b.end && b.start < a.end
        || (a.start == b.start && (a.start == a.end || b.start == b.end))
}

/// LCS-based line diff. The caller's note-size budget bounds this O(n*m)
/// implementation; for very large inputs we return one whole-document hunk.
fn diff_hunks(base: &[&str], changed: &[&str]) -> Vec<Hunk> {
    let diff = similar::TextDiff::from_slices(base, changed);
    let mut result = Vec::new();
    let mut active: Option<Hunk> = None;
    for op in diff.ops() {
        let old = op.old_range();
        let new = op.new_range();
        if matches!(op.tag(), similar::DiffTag::Equal) {
            if let Some(hunk) = active.take() {
                result.push(hunk);
            }
            continue;
        }
        let hunk = active.get_or_insert_with(|| Hunk {
            start: old.start,
            end: old.end,
            replacement: Vec::new(),
        });
        hunk.start = hunk.start.min(old.start);
        hunk.end = hunk.end.max(old.end);
        hunk.replacement
            .extend(changed[new].iter().map(|line| (*line).to_owned()));
    }
    if let Some(h) = active.take() {
        result.push(h);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn combines_disjoint_edits() {
        let r = three_way_merge("a\nb\nc\n", "A\nb\nc\n", "a\nb\nC\n");
        assert_eq!(r.merged.as_deref(), Some("A\nb\nC\n"));
    }
    #[test]
    fn exposes_conflicts_without_markers() {
        let r = three_way_merge("a\nb\n", "a\nmine\n", "a\ntheirs\n");
        assert!(r.merged.is_none());
        assert_eq!(r.conflicts.len(), 1);
        assert!(!format!("{r:?}").contains("<<<<<<<"));
    }
    #[test]
    fn identical_hunks_coalesce() {
        assert_eq!(
            three_way_merge("a\nb", "a\nx", "a\nx").merged.as_deref(),
            Some("a\nx")
        );
    }

    #[test]
    fn disjoint_edits_merge_in_large_documents_without_whole_file_conflict() {
        let base = (0..400)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        let mut mine = base.lines().map(str::to_owned).collect::<Vec<_>>();
        let mut theirs = mine.clone();
        mine[20] = "mine".into();
        theirs[370] = "theirs".into();
        let mine = format!("{}\n", mine.join("\n"));
        let theirs = format!("{}\n", theirs.join("\n"));

        let merged = three_way_merge(&base, &mine, &theirs);
        let merged = merged.merged.expect("disjoint edits must merge");
        assert!(merged.contains("mine\n"));
        assert!(merged.contains("theirs\n"));
        assert_eq!(merged.lines().count(), 400);
    }
}
