use flavite_syntax::{
    properties::{self, ScalarValue},
    read::{self, ReadKind, ReadNode},
};
fn walk<'a>(nodes: &'a [ReadNode], out: &mut Vec<&'a ReadNode>) {
    for node in nodes {
        out.push(node);
        walk(&node.children, out);
    }
}
#[test]
fn canonical_reading_tree_has_headings_links_extensions_tasks_tables_and_safe_html() {
    let source = "---\ntitle: 'Test' # keep\n---\n\n# Heading [[b|B]]\n\nHello **bold** and *italic* [[b]] ![[c]] #tag ^block\n\n- [ ] task\n- [x] done\n\n| A | B |\n|---|---|\n| X | Y |\n\n`[[code]]`\n\n```rust\n[[fenced]]\n```\n\n<script>bad()</script>\n\n[^a]: Footnote\n\nSee [^a].\n";
    let doc = read::parse(source.as_bytes()).unwrap();
    assert_eq!(doc.outline.iter().filter(|h| h.level > 0).count(), 1);
    assert!(doc.outline.iter().any(|h| h.title == "^block"));
    let mut nodes = Vec::new();
    walk(&doc.nodes, &mut nodes);
    assert!(nodes.iter().any(|n| n.kind == ReadKind::Table));
    assert_eq!(
        nodes
            .iter()
            .filter(|n| matches!(n.kind, ReadKind::Link { wiki: true, .. }))
            .count(),
        3
    );
    assert!(
        nodes
            .iter()
            .any(|n| matches!(n.kind, ReadKind::Excluded(_)))
    );
    let task = nodes
        .iter()
        .find(|n| n.kind == ReadKind::Task(false))
        .unwrap();
    let (range, patch) = read::task_patch(source, task.range.start, false).unwrap();
    assert_eq!(&source[range], "[ ]");
    assert_eq!(patch, "[x]");
    assert!(read::task_patch(source, task.range.start, true).is_err());
    assert!(read::task_patch("`- [ ] fake`", 3, false).is_err());
}
#[test]
fn scalar_patches_preserve_quote_comment_key_order_and_unicode() {
    let text = "---\n# header\ntitle: 'It''s here'  # retain\ncount: 3\nready: false\n---\nBody\n";
    let (range, replacement) =
        properties::patch(text, "title", &ScalarValue::Text("Bob's 雪".into())).unwrap();
    let mut changed = text.to_owned();
    changed.replace_range(range, &replacement);
    assert_eq!(
        changed,
        "---\n# header\ntitle: 'Bob''s 雪'  # retain\ncount: 3\nready: false\n---\nBody\n"
    );
    assert!(properties::patch(text, "count", &ScalarValue::Number("nan".into())).is_err());
    for yaml in [
        "a: 1\na: 2",
        "a: &anchor value",
        "a:\n  nested: yes",
        "a: [x, y]",
        "broken",
        "a: 'unclosed",
    ] {
        assert!(
            properties::patch(
                &format!("---\n{yaml}\n---\n"),
                "a",
                &ScalarValue::Text("new".into())
            )
            .is_err(),
            "{yaml}"
        );
    }
}

#[test]
fn canonical_frontmatter_and_reading_share_quoted_scalar_and_comment_semantics() {
    let text = "---\ntitle: 'Bob''s note' # comment\ncustom: \"# value\" # comment\n---\n![[b]]\n";
    let document = read::parse(text.as_bytes()).unwrap();
    let properties = document.properties.as_ref().unwrap();
    assert_eq!(properties.title.as_deref(), Some("Bob's note"));
    assert!(
        matches!(&properties.properties["custom"][0].1, flavite_syntax::PropertyValue::Text(value) if value == "# value")
    );
    let mut nodes = Vec::new();
    walk(&document.nodes, &mut nodes);
    let embed = nodes
        .iter()
        .find(|n| matches!(n.kind, ReadKind::Link { embed: true, .. }))
        .unwrap();
    assert_eq!(&text[embed.range.clone()], "![[b]]");
    assert!(!nodes.iter().any(|n| n.kind == ReadKind::Text("!".into())));
}

#[test]
fn task_coordinates_do_not_renormalize_preserved_carriage_returns_or_bom_content() {
    let text = read::normalized(b"\r\r\n- [ ] task\n").unwrap();
    assert_eq!(text, "\r\n- [ ] task\n");
    let document = read::parse_normalized(&text).unwrap();
    let mut nodes = Vec::new();
    walk(&document.nodes, &mut nodes);
    let task = nodes
        .iter()
        .find(|node| node.kind == ReadKind::Task(false))
        .unwrap();
    assert_eq!(&text[task.range.clone()], "[ ]");
    let (range, replacement) = read::task_patch(&text, task.range.start, false).unwrap();
    let mut changed = text.clone();
    changed.replace_range(range, &replacement);
    assert_eq!(changed, "\r\n- [x] task\n");
    let snapshot = flavite_syntax::TextSnapshot::from_normalized("\u{feff}\r\ncontent");
    assert_eq!(snapshot.text(), "\u{feff}\r\ncontent");
    assert_eq!(snapshot.source(), snapshot.text().as_bytes());
}
