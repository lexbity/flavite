//! Shared CommonMark reading tree. Coordinates always name normalized UTF-8 bytes.
//! This is a parse model, not a second link grammar or an HTML renderer.
use crate::{TextSnapshot, frontmatter, pulldown_glue};
use pulldown_cmark::{Event, Options, Parser, Tag};
use std::ops::Range;

/// Bounded semantic tree consumed by rendered reading and structure tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadDocument {
    pub nodes: Vec<ReadNode>,
    pub outline: Vec<OutlineEntry>,
    pub properties: Option<frontmatter::Frontmatter>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    pub level: u8,
    pub title: String,
    pub byte: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadNode {
    pub range: Range<usize>,
    pub kind: ReadKind,
    pub children: Vec<ReadNode>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadKind {
    Paragraph,
    Heading(u8),
    Quote,
    CodeBlock,
    HtmlBlock,
    List(Option<u64>),
    Item,
    Table,
    TableHead,
    TableRow,
    TableCell,
    Emphasis,
    Strong,
    Strike,
    Link {
        target: String,
        wiki: bool,
        embed: bool,
    },
    Text(String),
    Code(String),
    Break,
    Rule,
    Task(bool),
    Footnote(String),
    FootnoteRef(String),
    Frontmatter(String),
    /// HTML is an inert visible source fragment, never executable content.
    Excluded(String),
    Tag(String),
    BlockAnchor(String),
}
fn node(kind: ReadKind, range: Range<usize>) -> ReadNode {
    ReadNode {
        range,
        kind,
        children: Vec::new(),
    }
}
/// Parse with exactly the extension options admitted by the index parser.
pub fn parse(bytes: &[u8]) -> Result<ReadDocument, String> {
    let parsed = pulldown_glue::parse_note(bytes)?;
    from_parsed(parsed)
}
/// Parse current Session text, preserving its exact TextByte coordinate space.
pub fn parse_normalized(text: &str) -> Result<ReadDocument, String> {
    from_parsed(pulldown_glue::parse_snapshot(
        TextSnapshot::from_normalized(text),
    )?)
}
fn from_parsed(parsed: pulldown_glue::ParsedNote) -> Result<ReadDocument, String> {
    let text = parsed.snapshot.text();
    let properties = frontmatter::parse_frontmatter(&parsed.snapshot);
    let body_start = properties.as_ref().map_or(0, |fm| {
        text[fm.range.end..]
            .find('\n')
            .map_or(text.len(), |n| fm.range.end + n + 1)
    });
    let mut extensions = Vec::new();
    for link in &parsed.links {
        let target = link.anchor.as_ref().map_or_else(
            || link.target_raw.clone(),
            |anchor| format!("{}#{anchor}", link.target_raw),
        );
        let mut n = node(
            ReadKind::Link {
                target,
                wiki: true,
                embed: link.is_embed,
            },
            (if link.is_embed
                && link.span.start.0 > 0
                && text.as_bytes()[link.span.start.0 - 1] == b'!'
            {
                link.span.start.0 - 1
            } else {
                link.span.start.0
            })..link.span.end.0,
        );
        n.children.push(node(
            ReadKind::Text(
                link.alias
                    .clone()
                    .unwrap_or_else(|| link.target_raw.clone()),
            ),
            n.range.clone(),
        ));
        extensions.push(n);
    }
    for tag in &parsed.tags {
        extensions.push(node(
            ReadKind::Tag(text[tag.start.0..tag.end.0].into()),
            tag.start.0..tag.end.0,
        ));
    }
    for block in &parsed.block_ids {
        extensions.push(node(
            ReadKind::BlockAnchor(text[block.start.0..block.end.0].into()),
            block.start.0..block.end.0,
        ));
    }
    for range in crate::mask::math_ranges(text, &[]) {
        extensions.push(node(ReadKind::Code(text[range.clone()].into()), range));
    }
    extensions.sort_by_key(|n| n.range.start);
    let mut roots = Vec::new();
    if body_start > 0 {
        roots.push(node(
            ReadKind::Frontmatter(text[..body_start].into()),
            0..body_start,
        ));
    }
    let mut stack: Vec<ReadNode> = Vec::new();
    let mut outline = Vec::new();
    let opts = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut count = 0usize;
    for (event, relative) in Parser::new_ext(&text[body_start..], opts).into_offset_iter() {
        count += 1;
        if count > 200_000 || stack.len() > 64 {
            return Err("Reading structure limit exceeded; edit source".into());
        }
        let range = relative.start + body_start..relative.end + body_start;
        match event {
            Event::Start(tag) => {
                let kind = match tag {
                    Tag::Paragraph => ReadKind::Paragraph,
                    Tag::HtmlBlock => ReadKind::HtmlBlock,
                    Tag::Heading { level, .. } => ReadKind::Heading(level as u8),
                    Tag::BlockQuote(_) => ReadKind::Quote,
                    Tag::CodeBlock(_) => ReadKind::CodeBlock,
                    Tag::List(start) => ReadKind::List(start),
                    Tag::Item => ReadKind::Item,
                    Tag::Table(_) => ReadKind::Table,
                    Tag::TableHead => ReadKind::TableHead,
                    Tag::TableRow => ReadKind::TableRow,
                    Tag::TableCell => ReadKind::TableCell,
                    Tag::Emphasis => ReadKind::Emphasis,
                    Tag::Strong => ReadKind::Strong,
                    Tag::Strikethrough => ReadKind::Strike,
                    Tag::Link { dest_url, .. } => ReadKind::Link {
                        target: dest_url.into_string(),
                        wiki: false,
                        embed: false,
                    },
                    Tag::Image { dest_url, .. } => ReadKind::Link {
                        target: dest_url.into_string(),
                        wiki: false,
                        embed: true,
                    },
                    Tag::FootnoteDefinition(label) => ReadKind::Footnote(label.into_string()),
                    _ => return Err("Unsupported reading construct; edit source".into()),
                };
                stack.push(node(kind, range));
            }
            Event::End(_) => {
                let Some(completed) = stack.pop() else {
                    return Err("Invalid reading tree".into());
                };
                if let ReadKind::Heading(level) = completed.kind {
                    outline.push(OutlineEntry {
                        level,
                        title: plain_text(&completed),
                        byte: completed.range.start,
                    });
                }
                append(&mut roots, &mut stack, completed);
            }
            Event::Text(value) => {
                // Pulldown may split [[target]] across text events. Scanner spans
                // are canonical; emit a link once and consume all intersecting text.
                let mut at = range.start;
                let code_block = stack.iter().any(|n| n.kind == ReadKind::CodeBlock);
                for extension in extensions.iter().filter(|extension| {
                    extension.range.start < range.end
                        && extension.range.end > range.start
                        && !code_block
                }) {
                    if extension.range.start > at {
                        append(
                            &mut roots,
                            &mut stack,
                            node(
                                ReadKind::Text(text[at..extension.range.start].into()),
                                at..extension.range.start,
                            ),
                        );
                    }
                    if extension.range.start >= range.start {
                        append(&mut roots, &mut stack, extension.clone());
                    }
                    at = extension.range.end.max(at);
                }
                if at == range.start {
                    append(
                        &mut roots,
                        &mut stack,
                        node(ReadKind::Text(value.into_string()), range),
                    );
                } else if at < range.end {
                    append(
                        &mut roots,
                        &mut stack,
                        node(ReadKind::Text(text[at..range.end].into()), at..range.end),
                    );
                }
            }
            Event::Code(value) => append(
                &mut roots,
                &mut stack,
                node(ReadKind::Code(value.into_string()), range),
            ),
            Event::Html(value) | Event::InlineHtml(value) => append(
                &mut roots,
                &mut stack,
                node(ReadKind::Excluded(value.into_string()), range),
            ),
            Event::SoftBreak | Event::HardBreak => {
                append(&mut roots, &mut stack, node(ReadKind::Break, range))
            }
            Event::Rule => append(&mut roots, &mut stack, node(ReadKind::Rule, range)),
            Event::TaskListMarker(checked) => {
                append(&mut roots, &mut stack, node(ReadKind::Task(checked), range))
            }
            Event::FootnoteReference(label) => append(
                &mut roots,
                &mut stack,
                node(ReadKind::FootnoteRef(label.into_string()), range),
            ),
            Event::InlineMath(value) | Event::DisplayMath(value) => append(
                &mut roots,
                &mut stack,
                node(ReadKind::Code(value.into_string()), range),
            ),
        }
    }
    if !stack.is_empty() {
        return Err("Unclosed reading tree".into());
    }
    for block in &parsed.block_ids {
        outline.push(OutlineEntry {
            level: 0,
            title: text[block.start.0..block.end.0].into(),
            byte: block.start.0,
        });
    }
    outline.sort_by_key(|entry| entry.byte);
    Ok(ReadDocument {
        nodes: roots,
        outline,
        properties,
    })
}
fn append(roots: &mut Vec<ReadNode>, stack: &mut [ReadNode], n: ReadNode) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(n);
    } else {
        roots.push(n);
    }
}
/// Visible text for headings, accessible names and inline fallbacks.
pub fn plain_text(n: &ReadNode) -> String {
    match &n.kind {
        ReadKind::Text(t)
        | ReadKind::Code(t)
        | ReadKind::Excluded(t)
        | ReadKind::Tag(t)
        | ReadKind::BlockAnchor(t) => t.clone(),
        ReadKind::Break => " ".into(),
        _ => n.children.iter().map(plain_text).collect(),
    }
}
/// A task edit is valid only at a canonical parser task marker.
pub fn task_patch(
    text: &str,
    start: usize,
    expected: bool,
) -> Result<(Range<usize>, String), String> {
    let document = parse_normalized(text)?;
    fn find(nodes: &[ReadNode], start: usize, expected: bool) -> Option<Range<usize>> {
        nodes.iter().find_map(|n| {
            if n.range.start == start && n.kind == ReadKind::Task(expected) {
                Some(n.range.clone())
            } else {
                find(&n.children, start, expected)
            }
        })
    }
    let range =
        find(&document.nodes, start, expected).ok_or("Task changed; refresh before editing")?;
    let raw = text.get(range.clone()).ok_or("Invalid task span")?;
    if !matches!(raw, "[ ]" | "[x]" | "[X]") {
        return Err("Invalid task marker".into());
    }
    Ok((range, if expected { "[ ]" } else { "[x]" }.into()))
}
/// Normalize bytes consistently with editor/index coordinates.
pub fn normalized(bytes: &[u8]) -> Result<String, String> {
    Ok(TextSnapshot::new(bytes)
        .map_err(|e| e.to_string())?
        .text()
        .into())
}

/// Resolve existing heading/block-anchor syntax against current canonical text.
pub fn resolve_anchor(text: &str, anchor: &str) -> Result<Option<usize>, String> {
    let doc = parse_normalized(text)?;
    Ok(doc
        .outline
        .iter()
        .find(|h| crate::text::fold_key(&h.title) == crate::text::fold_key(anchor))
        .map(|h| h.byte))
}
