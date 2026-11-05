//! Proportional prose, nested blocks and real revisioned task/link actions.
use crate::{
    app::{App, Message},
    style,
    workspace::bounds,
};
use flavite_api::{EmbedContent, ReadKind, ReadNode, ReadSnapshot};
use flavite_workspace::TabId;
use iced::widget::{
    button, column, container, image, rich_text, row, rule, scrollable, span, text,
};
use iced::{Element, Font, Length};
use std::sync::Arc;

pub fn scroll_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("read-{}", tab.raw()))
}
pub fn view<'a>(app: &'a App, tab: TabId, doc: &'a Arc<ReadSnapshot>) -> Element<'a, Message> {
    let mut content = column![].spacing(12).width(Length::Fill);
    for n in &doc.document.nodes {
        let start = n.range.start;
        let hash = doc.content_hash;
        content = content.push(bounds::observe(block(app, tab, doc, n, 0), move |rect| {
            Message::ReadBlockBounds {
                tab,
                hash,
                start,
                rect,
            }
        }));
    }
    let content = container(content)
        .max_width(880)
        .padding(20)
        .width(Length::Fill);
    scrollable(content)
        .id(scroll_id(tab))
        .on_scroll(move |viewport| Message::ReadScrolled {
            tab,
            offset: viewport.absolute_offset().y,
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
fn block<'a>(
    app: &'a App,
    tab: TabId,
    doc: &'a Arc<ReadSnapshot>,
    n: &'a ReadNode,
    depth: usize,
) -> Element<'a, Message> {
    if depth > 64 {
        return text("Structure limit — edit source").into();
    }
    match &n.kind {
        ReadKind::Heading(level) => inline(
            app,
            tab,
            doc,
            &n.children,
            32.0 - f32::from(*level) * 2.0,
            true,
        ),
        ReadKind::Paragraph => children(app, tab, doc, &n.children, depth),
        ReadKind::CodeBlock => container(
            text(flavite_api::plain_text(n))
                .font(Font::MONOSPACE)
                .size(14),
        )
        .padding(12)
        .width(Length::Fill)
        .style(style::toolbar)
        .into(),
        ReadKind::Quote => container(children(app, tab, doc, &n.children, depth))
            .padding([8, 20])
            .style(style::toolbar)
            .into(),
        ReadKind::List(start) => {
            let mut items = column![].spacing(8);
            for (index, item) in n.children.iter().enumerate() {
                let marker = start.map_or_else(
                    || "•".into(),
                    |number| format!("{}.", number.saturating_add(index as u64)),
                );
                items = items.push(
                    row![
                        text(marker).width(30),
                        block(app, tab, doc, item, depth + 1)
                    ]
                    .spacing(4),
                );
            }
            items.into()
        }
        ReadKind::Item => children(app, tab, doc, &n.children, depth),
        ReadKind::Table => container(children(app, tab, doc, &n.children, depth))
            .padding(8)
            .style(style::toolbar)
            .into(),
        ReadKind::TableHead | ReadKind::TableRow => {
            let cells = n
                .children
                .iter()
                .map(|c| {
                    container(inline(
                        app,
                        tab,
                        doc,
                        &c.children,
                        14.0,
                        n.kind == ReadKind::TableHead,
                    ))
                    .padding(8)
                    .width(Length::Fill)
                    .into()
                })
                .collect::<Vec<Element<'a, Message>>>();
            row(cells).width(Length::Fill).into()
        }
        ReadKind::TableCell => inline(app, tab, doc, &n.children, 14.0, false),
        ReadKind::Rule => rule::horizontal(1).into(),
        ReadKind::Task(checked) => crate::host::reading::observe_control(
            tab,
            doc,
            "task",
            n.range.start,
            button(text(if *checked { "☑ Done" } else { "☐ To do" }))
                .style(style::ghost())
                .on_press_maybe(task_message(app, tab, doc, n.range.start, *checked)),
        ),
        ReadKind::Link { embed: true, .. } => embed(app, tab, doc, n, depth),
        ReadKind::Frontmatter(source) => container(column![
            text("Frontmatter").size(12).style(style::dim_text),
            text(source).font(Font::MONOSPACE).size(12)
        ])
        .padding(12)
        .style(style::toolbar)
        .into(),
        ReadKind::Footnote(label) => row![
            text(format!("[{label}]")),
            children(app, tab, doc, &n.children, depth)
        ]
        .spacing(8)
        .into(),
        _ => inline(app, tab, doc, std::slice::from_ref(n), 16.0, false),
    }
}
fn children<'a>(
    app: &'a App,
    tab: TabId,
    doc: &'a Arc<ReadSnapshot>,
    nodes: &'a [ReadNode],
    depth: usize,
) -> Element<'a, Message> {
    // Tight list items contain inline text as direct children. Preserve inline
    // flow until a real block/task/embed boundary requires its own widget.
    if let Some(first) = nodes
        .first()
        .filter(|node| matches!(node.kind, ReadKind::Task(_)))
    {
        return row![
            block(app, tab, doc, first, depth + 1),
            children(app, tab, doc, &nodes[1..], depth + 1)
        ]
        .spacing(8)
        .into();
    }
    let mut out = column![].spacing(8).width(Length::Fill);
    let mut run = 0;
    for (index, n) in nodes.iter().enumerate() {
        if matches!(
            n.kind,
            ReadKind::Task(_)
                | ReadKind::Paragraph
                | ReadKind::List(_)
                | ReadKind::Quote
                | ReadKind::CodeBlock
                | ReadKind::Table
                | ReadKind::TableHead
                | ReadKind::TableRow
                | ReadKind::TableCell
                | ReadKind::Rule
                | ReadKind::Footnote(_)
                | ReadKind::Link { embed: true, .. }
        ) {
            if run < index {
                out = out.push(inline(app, tab, doc, &nodes[run..index], 16.0, false));
            }
            out = out.push(block(app, tab, doc, n, depth + 1));
            run = index + 1;
        }
    }
    if run < nodes.len() {
        out = out.push(inline(app, tab, doc, &nodes[run..], 16.0, false));
    }
    out.into()
}
fn inline<'a>(
    app: &'a App,
    tab: TabId,
    doc: &'a Arc<ReadSnapshot>,
    nodes: &'a [ReadNode],
    size: f32,
    bold: bool,
) -> Element<'a, Message> {
    let mut spans: Vec<iced::widget::text::Span<'a, Message>> = Vec::new();
    let query = app
        .note_search
        .finds
        .get(&tab)
        .map(|find| find.query.as_str())
        .filter(|query| !query.is_empty());
    if nodes.iter().any(contains_embed) {
        let mut flow = EmbedFlow {
            app,
            tab,
            doc,
            size,
            spans: Vec::new(),
            elements: Vec::new(),
        };
        flow.walk(
            nodes,
            InlineStyle {
                bold,
                italic: false,
                strike: false,
                link: None,
                query,
                scheme: app.appearance.scheme(),
            },
        );
        flow.flush();
        return column(flow.elements).spacing(8).width(Length::Fill).into();
    }
    for n in nodes {
        collect_spans(
            tab,
            doc,
            n,
            InlineStyle {
                bold,
                italic: false,
                strike: false,
                link: None,
                query,
                scheme: app.appearance.scheme(),
            },
            &mut spans,
        );
    }
    rich_text(spans)
        .size(size)
        .font(Font::DEFAULT)
        .line_height(1.5)
        .width(Length::Fill)
        .on_link_click(|message| message)
        .into()
}
#[derive(Clone, Copy)]
struct InlineStyle<'q> {
    bold: bool,
    italic: bool,
    strike: bool,
    link: Option<usize>,
    query: Option<&'q str>,
    /// The active scheme, so highlights derive from the live palette.
    scheme: crate::style::Scheme,
}
fn contains_embed(node: &ReadNode) -> bool {
    matches!(node.kind, ReadKind::Link { embed: true, .. })
        || node.children.iter().any(contains_embed)
}
/// Inline emphasis/link wrappers may contain an embed. Preserve styled runs
/// around the embedded block instead of reducing its content to a link label.
struct EmbedFlow<'a> {
    app: &'a App,
    tab: TabId,
    doc: &'a Arc<ReadSnapshot>,
    size: f32,
    spans: Vec<iced::widget::text::Span<'a, Message>>,
    elements: Vec<Element<'a, Message>>,
}
impl<'a> EmbedFlow<'a> {
    fn flush(&mut self) {
        if !self.spans.is_empty() {
            self.elements.push(
                rich_text(std::mem::take(&mut self.spans))
                    .size(self.size)
                    .line_height(1.5)
                    .width(Length::Fill)
                    .on_link_click(|message| message)
                    .into(),
            );
        }
    }
    fn walk(&mut self, nodes: &'a [ReadNode], style: InlineStyle<'_>) {
        for node in nodes {
            if matches!(node.kind, ReadKind::Link { embed: true, .. }) {
                self.flush();
                self.elements
                    .push(embed(self.app, self.tab, self.doc, node, 0));
            } else if node.children.iter().any(contains_embed) {
                self.walk(
                    &node.children,
                    InlineStyle {
                        bold: style.bold || node.kind == ReadKind::Strong,
                        italic: style.italic || node.kind == ReadKind::Emphasis,
                        strike: style.strike || node.kind == ReadKind::Strike,
                        link: if matches!(node.kind, ReadKind::Link { .. }) {
                            Some(node.range.start)
                        } else {
                            style.link
                        },
                        query: style.query,
                        scheme: self.app.appearance.scheme(),
                    },
                );
            } else {
                collect_spans(self.tab, self.doc, node, style, &mut self.spans);
            }
        }
    }
}
fn collect_spans<'a>(
    tab: TabId,
    doc: &'a Arc<ReadSnapshot>,
    n: &'a ReadNode,
    format: InlineStyle<'_>,
    spans: &mut Vec<iced::widget::text::Span<'a, Message>>,
) {
    let mut font = Font::DEFAULT;
    if format.bold {
        font.weight = iced::font::Weight::Bold;
    }
    if format.italic {
        font.style = iced::font::Style::Italic;
    }
    let content = match &n.kind {
        ReadKind::Text(t) => Some(t.clone()),
        ReadKind::Code(t) => {
            font = Font::MONOSPACE;
            Some(t.clone())
        }
        ReadKind::Break => Some("\n".into()),
        ReadKind::Excluded(t) => Some(format!("[HTML source: {t}]")),
        ReadKind::FootnoteRef(t) => Some(format!("[{t}]")),
        ReadKind::Tag(t) | ReadKind::BlockAnchor(t) => Some(t.clone()),
        _ => None,
    };
    if let Some(content) = content {
        let mut s = span(content.clone())
            .font(font)
            .strikethrough(format.strike);
        if let Some(start) = format.link {
            s = s.underline(true).link(Message::ReadActivate {
                tab,
                document: doc.clone(),
                start,
            });
        }
        if let Some(query) = format.query {
            // Find highlights use semantic palette tokens and the same dirty
            // snapshot as the editor; links keep their original action. The
            // per-node scan folds exactly like the note search host so the
            // read view highlights what the find bar counts (D-05 default).
            let mut at = 0;
            for (found, end) in crate::host::note_search::scan(&content, query, false).0 {
                if at < found {
                    let mut fragment = s.clone();
                    fragment.text = content[at..found].to_owned().into();
                    spans.push(fragment);
                }
                let mut highlight = s.clone();
                highlight.text = content[found..end].to_owned().into();
                let theme = style::theme(format.scheme);
                highlight = highlight.background(theme.extended_palette().primary.weak.color);
                spans.push(highlight);
                at = end;
            }
            if at < content.len() {
                s.text = content[at..].to_owned().into();
                spans.push(s);
            }
        } else {
            spans.push(s);
        }
    }
    for child in &n.children {
        collect_spans(
            tab,
            doc,
            child,
            InlineStyle {
                bold: format.bold || n.kind == ReadKind::Strong,
                italic: format.italic || n.kind == ReadKind::Emphasis,
                strike: format.strike || n.kind == ReadKind::Strike,
                link: if matches!(n.kind, ReadKind::Link { .. }) {
                    Some(n.range.start)
                } else {
                    format.link
                },
                query: format.query,
                scheme: format.scheme,
            },
            spans,
        );
    }
}
fn task_message(
    app: &App,
    tab: TabId,
    doc: &Arc<ReadSnapshot>,
    start: usize,
    checked: bool,
) -> Option<Message> {
    let (buffer, revision) = doc.buffer?;
    if !crate::host::reading::can_edit(app, doc) {
        return None;
    }
    Some(Message::StructureEdit {
        tab,
        document: doc.clone(),
        command: "task.toggle",
        args: serde_json::json!({"buffer_id": buffer, "revision": revision, "start": start, "checked": checked}),
    })
}
fn embed<'a>(
    app: &'a App,
    tab: TabId,
    doc: &'a Arc<ReadSnapshot>,
    n: &'a ReadNode,
    depth: usize,
) -> Element<'a, Message> {
    let mut content = column![
        text(flavite_api::plain_text(n)).size(14),
        button("Open embedded note / attachment")
            .style(style::ghost())
            .on_press(Message::ReadActivate {
                tab,
                document: doc.clone(),
                start: n.range.start
            })
    ]
    .spacing(8);
    match doc.embeds.get(&n.range.start) {
        Some(EmbedContent::Note(embedded)) => {
            content = content.push(
                text(if crate::host::reading::unsaved(app, embedded) {
                    "Embedded note · unsaved changes"
                } else {
                    "Embedded note · saved"
                })
                .size(12)
                .style(style::dim_text),
            );
            content = content.push(children(
                app,
                tab,
                embedded,
                &embedded.document.nodes,
                depth + 1,
            ))
        }
        Some(EmbedContent::Image(pixels)) => {
            if let Some(handle) = app.reading.images.get(&pixels.hash) {
                content = content.push(image(handle.clone()).width(Length::Fill));
            }
        }
        Some(EmbedContent::Unavailable(message)) => {
            content = content.push(text(message).size(12).style(style::dim_text))
        }
        None => content = content.push(text("Embed unavailable").size(12)),
    }
    container(content)
        .padding(12)
        .width(Length::Fill)
        .style(style::toolbar)
        .into()
}
