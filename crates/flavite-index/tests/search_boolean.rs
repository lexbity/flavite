//! RFC-0006 S06 search-session evaluation (AC-8): nested mixed AST
//! membership equals an independent finite-corpus oracle; ordering is
//! stable; invalid forms fail with typed errors; NOT complements the
//! authorized universe; positive text leaves contribute their BM25 score
//! exactly once per distinct leaf.

use flavite_index::IndexActor;
use flavite_index::query::{Query, QueryMode, QueryParseError, TextLeaf};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const NOTES: &[(&str, &str)] = &[
    ("alpha.md", "# Alpha\n\nrocket science\n"),
    (
        "bravo.md",
        "---\ntags: [proj]\n---\n# Bravo\n\nrocket moon\n",
    ),
    ("charlie.md", "# Charlie\n\nquiet moon rocket\n"),
    ("delta.md", "---\ntags: [proj]\n---\n# Delta\n\nrocket\n"),
    ("echo.md", "# Echo\n\nnothing here\n"),
];

/// Apply one note honoring the D-07 admission contract: writer backpressure
/// refuses the index admission before the generation commits, so retrying
/// the cache application never repeats a durable write.
fn apply_retried(index: &mut IndexActor, path: &str, bytes: &[u8]) -> Result<(u64, u64), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match index.apply_saved_note(path.as_bytes(), path, bytes, 1, 1) {
            Ok(applied) => return Ok(applied),
            Err(error) if error.contains("queue is full") || error.contains("budget exceeded") => {
                assert!(
                    Instant::now() < deadline,
                    "search writer never drained its envelope queue: {error}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error),
        }
    }
}

fn corpus() -> (tempfile::TempDir, IndexActor) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    let mut index = IndexActor::new(&db, 1).unwrap();
    for (path, body) in NOTES {
        apply_retried(&mut index, path, body.as_bytes()).unwrap();
    }
    // Ensure every committed generation's envelope is durable and the
    // acknowledged mirror is current before membership assertions.
    index.drain_search_acks().unwrap();
    (dir, index)
}

fn text(term: &str) -> Query {
    Query::Text(TextLeaf::all_fields(term))
}

fn tag(name: &str) -> Query {
    Query::Tag {
        name: name.to_owned(),
        hierarchical: false,
    }
}

/// Independent finite-corpus oracle: membership from the fixture labels
/// (AC-8). The labels are the stems of [`NOTES`].
fn oracle(query: &Query) -> BTreeSet<&'static str> {
    match query {
        Query::Text(leaf) => match leaf.term.as_str() {
            "rocket" => BTreeSet::from(["alpha.md", "bravo.md", "charlie.md", "delta.md"]),
            "moon" => BTreeSet::from(["bravo.md", "charlie.md"]),
            "quiet" => BTreeSet::from(["charlie.md"]),
            "nothing" => BTreeSet::from(["echo.md"]),
            _ => BTreeSet::new(),
        },
        Query::Tag { name, .. } if name == "proj" => BTreeSet::from(["bravo.md", "delta.md"]),
        Query::Unresolved => BTreeSet::new(),
        Query::And(parts) => {
            let mut acc: BTreeSet<&'static str> = NOTES.iter().map(|(path, _)| *path).collect();
            for part in parts {
                acc = acc.intersection(&oracle(part)).copied().collect();
            }
            acc
        }
        Query::Or(parts) => {
            let mut acc = BTreeSet::new();
            for part in parts {
                acc.extend(oracle(part));
            }
            acc
        }
        Query::Not(inner) => NOTES
            .iter()
            .map(|(path, _)| *path)
            .collect::<BTreeSet<_>>()
            .difference(&oracle(inner))
            .copied()
            .collect(),
        other => panic!("oracle does not model {other:?}"),
    }
}

fn engine_membership(index: &mut IndexActor, query: &Query) -> BTreeSet<String> {
    let page = index.evaluate_saved_search(query, 1_000).unwrap();
    page.items.into_iter().map(|row| row.path).collect()
}

#[test]
fn mixed_ast_membership_equals_the_independent_oracle() {
    let (_dir, mut index) = corpus();
    let cases: Vec<Query> = vec![
        text("rocket"),
        text("moon"),
        Query::And(vec![text("rocket"), tag("proj")]),
        Query::Or(vec![text("moon"), tag("proj")]),
        Query::Not(Box::new(tag("proj"))),
        Query::Not(Box::new(text("rocket"))),
        Query::And(vec![
            Query::Or(vec![text("rocket"), text("nothing")]),
            Query::Not(Box::new(tag("proj"))),
        ]),
        Query::And(vec![text("rocket"), Query::Not(Box::new(text("moon")))]),
        Query::Or(vec![
            Query::And(vec![text("quiet"), text("moon")]),
            Query::Not(Box::new(text("rocket"))),
        ]),
        // Double negation normalizes to a positive leaf (§6.3).
        Query::Not(Box::new(Query::Not(Box::new(text("quiet"))))),
        Query::Unresolved,
    ];
    for query in &cases {
        let expected: BTreeSet<String> = oracle(query)
            .into_iter()
            .map(std::borrow::ToOwned::to_owned)
            .collect();
        let actual = engine_membership(&mut index, query);
        assert_eq!(actual, expected, "query {query:?}");
    }
}

#[test]
fn repeated_positive_leaf_across_or_branches_scores_once() {
    let (_dir, mut index) = corpus();
    let single = index.evaluate_saved_search(&text("rocket"), 100).unwrap();
    let single_scores: std::collections::BTreeMap<String, f32> = single
        .items
        .iter()
        .map(|row| (row.path.clone(), row.score.unwrap_or(0.0)))
        .collect();
    assert!(single.items.iter().all(|row| row.score.is_some()));
    // The same Text appears in both satisfied OR branches: the contributing
    // mask dedupes it, so every score equals the plain leaf's score (§6.3).
    let duplicated = Query::Or(vec![
        text("rocket"),
        Query::And(vec![text("rocket"), tag("proj")]),
    ]);
    let page = index.evaluate_saved_search(&duplicated, 100).unwrap();
    assert_eq!(page.items.len(), 4);
    for row in &page.items {
        let expected = single_scores[&row.path];
        assert!(
            (row.score.unwrap_or(0.0) - expected).abs() < 1e-4,
            "{} scored {} but the single leaf scores {expected}",
            row.path,
            row.score.unwrap_or(0.0)
        );
    }
    // Distinct leaves in one satisfied AND contribute additively.
    let combined = Query::And(vec![text("rocket"), text("moon")]);
    let page = index.evaluate_saved_search(&combined, 100).unwrap();
    assert_eq!(page.items.len(), 2);
    for row in &page.items {
        let rocket = single_scores[&row.path];
        let moon = 0.0f32;
        assert!(row.score.unwrap_or(0.0) > rocket + moon - 1e-4);
    }
}

#[test]
fn negative_only_queries_order_by_canonical_native_path() {
    let (_dir, mut index) = corpus();
    // Negative-only/structural matches score zero and collapse to native
    // path order (D-07).
    let page = index
        .evaluate_saved_search(&Query::Not(Box::new(tag("proj"))), 100)
        .unwrap();
    let paths: Vec<String> = page.items.iter().map(|row| row.path.clone()).collect();
    assert_eq!(paths, vec!["alpha.md", "charlie.md", "echo.md"]);
    // A negative-only tree has no positive text leaf: rows carry no
    // relevance at all (D11) and collapse to native path order.
    assert!(page.items.iter().all(|row| row.score.is_none()));

    let structural = index.evaluate_saved_search(&tag("proj"), 100).unwrap();
    let paths: Vec<String> = structural
        .items
        .iter()
        .map(|row| row.path.clone())
        .collect();
    assert_eq!(paths, vec!["bravo.md", "delta.md"]);
    // Pure structural trees carry no relevance at all (D11); mixed trees
    // keep the §6.3 zero-score rule for non-text rows.
    assert!(structural.items.iter().all(|row| row.score.is_none()));
}

#[test]
fn not_complements_the_whole_authorized_universe() {
    let (_dir, mut index) = corpus();
    let page = index
        .evaluate_saved_search(&Query::Not(Box::new(text("rocket"))), 100)
        .unwrap();
    let paths: Vec<String> = page.items.iter().map(|row| row.path.clone()).collect();
    assert_eq!(paths, vec!["echo.md"]);
}

#[test]
fn phrase_leaves_match_the_pinned_analyzer_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    let mut index = IndexActor::new(&db, 1).unwrap();
    apply_retried(&mut index, "one.md", b"the quick brown fox\n").unwrap();
    apply_retried(&mut index, "two.md", b"brown quick the fox\n").unwrap();
    apply_retried(&mut index, "three.md", b"quick brown\n").unwrap();
    apply_retried(&mut index, "ja.md", "東京の地図\n".as_bytes()).unwrap();
    apply_retried(&mut index, "reversed.md", "京東\n".as_bytes()).unwrap();
    index.drain_search_acks().unwrap();

    // A quoted phrase requires the token sequence; the reordered note does
    // not match.
    let phrase = Query::Text(TextLeaf::phrase("quick brown"));
    let page = index.evaluate_saved_search(&phrase, 100).unwrap();
    let paths: BTreeSet<String> = page.items.iter().map(|row| row.path.clone()).collect();
    assert_eq!(
        paths,
        BTreeSet::from(["one.md".to_owned(), "three.md".to_owned()])
    );
    // Score order is descending and stable (the shorter document scores
    // higher under BM25); ties fall back to native path order.
    let scores: Vec<f32> = page
        .items
        .iter()
        .map(|row| row.score.unwrap_or(0.0))
        .collect();
    assert!(scores.windows(2).all(|pair| pair[0] >= pair[1]));

    // A multi-token term input behaves as the same position-preserving
    // sequence through the pinned analyzer (CJK unigrams).
    let cjk = Query::Text(TextLeaf::all_fields("東京"));
    let page = index.evaluate_saved_search(&cjk, 100).unwrap();
    let paths: Vec<String> = page.items.iter().map(|row| row.path.clone()).collect();
    assert_eq!(paths, vec!["ja.md"]);

    // Field filters are evaluated in the pinned searcher (§6.9).
    let title_only = Query::Text(TextLeaf {
        fields: vec![flavite_index::query::TextField::Body],
        phrase: false,
        term: "fox".into(),
    });
    let page = index.evaluate_saved_search(&title_only, 100).unwrap();
    assert_eq!(page.items.len(), 2);
}

#[test]
fn invalid_forms_fail_with_typed_errors() {
    // Unknown field syntax in expression mode is a typed rejection.
    assert!(matches!(
        Query::parse_typed("title:rocket AND bogus:foo", QueryMode::Expression),
        Err(QueryParseError::UnsupportedSyntax { .. })
    ));
    // Nested legacy engine field syntax gets the concrete repair (§6.9).
    assert!(matches!(
        Query::parse_typed("text:title:rocket", QueryMode::Expression),
        Err(QueryParseError::UnsupportedLegacyTextSyntax { .. })
    ));
    // Plain mode treats punctuation as text (§6.9) — no error, literal.
    assert!(Query::parse_typed("bogus:foo", QueryMode::Plain).is_ok());
    // Depth limits (NFR-8): a 40-deep NOT chain exceeds the bound.
    let input = format!("{}rocket", "NOT ".repeat(40));
    assert!(matches!(
        Query::parse_typed(&input, QueryMode::Expression),
        Err(QueryParseError::TooComplex { .. })
    ));
    // The structural endpoint rejects text predicates explicitly.
    let (_dir, mut index) = corpus();
    let error = index.structural_page(&text("rocket"), 10).unwrap_err();
    assert!(error.contains("text predicates are not supported"));
    // Attachment predicates are not members of note search (§6.3).
    let attachment = Query::Attachment {
        kind: None,
        name: None,
    };
    let error = index.evaluate_saved_search(&attachment, 10).unwrap_err();
    assert!(error.contains("attachment"));
}

#[test]
#[allow(
    clippy::items_after_statements,
    reason = "the generated-tree strategies are local to this one property"
)]
fn property_generated_mixed_trees_match_the_oracle() {
    use proptest::prelude::*;
    use proptest::strategy::{BoxedStrategy, Just};
    let (_dir, index) = corpus();
    fn leaf_strategy() -> impl proptest::strategy::Strategy<Value = Query> {
        prop::strategy::Union::new([
            Just(text("rocket")),
            Just(text("moon")),
            Just(text("quiet")),
            Just(text("nothing")),
            Just(text("zzz-absent")),
            Just(tag("proj")),
            Just(Query::Unresolved),
        ])
    }

    fn branch(depth_leaf: BoxedStrategy<Query>) -> BoxedStrategy<Query> {
        let composite = (
            proptest::bool::ANY,
            depth_leaf.clone(),
            depth_leaf.clone(),
            depth_leaf.clone(),
        )
            .prop_map(|(is_and, a, b, c)| {
                if is_and {
                    Query::And(vec![a, b, c])
                } else {
                    Query::Or(vec![a, b, c])
                }
            });
        let negation = depth_leaf.prop_map(|leaf| Query::Not(Box::new(leaf)));
        prop::strategy::Union::new([composite.boxed(), negation.boxed()]).boxed()
    }

    fn tree_strategy() -> impl proptest::strategy::Strategy<Value = Query> {
        leaf_strategy().prop_recursive(6, 24, 3, branch)
    }

    // proptest cases replay on the shared corpus actor behind a mutex (the
    // proptest closure is `Fn`).
    let index = std::sync::Mutex::new(index);
    let tree = tree_strategy();
    proptest!(|(query in tree)| {
        let expected: BTreeSet<String> = oracle(&query)
            .into_iter()
            .map(std::borrow::ToOwned::to_owned)
            .collect();
        let actual = engine_membership(&mut index.lock().unwrap(), &query);
        prop_assert_eq!(actual, expected);
    });
}
