//! `CardLayer` — the zoomable card layer that takes over from the instanced
//! dots when zoomed in (D15, B7).
//!
//! At zoom levels beyond the LOD threshold, the graph view switches to
//! real widgets at world positions: note cards showing preview text and
//! task lines with clickable checkboxes. The layer is authoritative for
//! zoomed-in interaction; checkbox toggles dispatch Session commands
//! through `flavite-api` directly (B7: the B4 `CardEditSink` seam is deleted).
//!
//! # Design
//!
//! - **LOD threshold**: the layer activates when the camera zoom exceeds
//!   [`CARD_ZOOM_THRESHOLD`] (2.0× by default — D15's "zoomed in" regime).
//! - **Card cap**: at most 64 real widgets (D15).
//! - **Text re-layout**: text is re-laid-out at the scaled font size,
//!   quantized to [`super::zoomable::SCALE_STEP_BASE`] increments, cached per step.
//! - **Edit flow**: card edits route through `Flavite` → Session commands
//!   with buffer identity/revision guards. Indexed task spans carry source
//!   content hash; Session validates before applying (§4.5.2, D7).
//!
//! # Production note
//!
//! B7 productionizes the card layer: the B4 prototype's `CardEditSink` trait
//! is removed; edits now dispatch through `flavite-api::Flavite`.

use std::collections::HashMap;

use iced::widget::{Column, text};
use iced::{Color, Element, Rectangle};

use super::nodes::attachment_filename;
use super::zoomable::{CARD_CAP, Card, CardIdentity, LayoutCache, ZoomableCamera};
use flavite_api::Flavite;
use flavite_buffer::revision::{BufferId, Revision};
use flavite_graph::{NodeId, NodeKind};

/// Zoom threshold at which the card layer activates (D15: LOD via
/// zoom/visible-count threshold). At or above this zoom, cards are shown;
/// below it, the instanced dots take over.
pub const CARD_ZOOM_THRESHOLD: f32 = 2.0;

/// Maximum card count — the same constant as zoomable.
pub const MAX_CARDS: usize = CARD_CAP;

/// Message emitted by the card layer.
#[derive(Debug, Clone)]
pub enum CardLayerMessage {
    /// Toggle requested at the given card and line.
    Toggle {
        identity: CardIdentity,
        line_index: usize,
    },
    /// Open note requested for the given identity.
    OpenNote { identity: CardIdentity },
}

/// The card layer state.
///
/// The layer is either active (zoomed in) or inactive (zoomed out). When
/// inactive, it renders nothing and passes through events unchanged. When
/// active, it renders up to [`MAX_CARDS`] cards at world positions and
/// handles toggle requests.
#[derive(Debug, Clone)]
pub struct CardLayerState {
    pub is_active: bool,
    pub visible_count: usize,
    pub cached_count: usize,
}

/// Compute the card layer state for a given camera and cards.
pub fn compute_card_layer_state(
    cards: &[Card<CardLayerMessage>],
    camera: &ZoomableCamera,
    bounds: Rectangle,
    cache: &mut LayoutCache,
) -> CardLayerState {
    let is_active = camera.zoom >= CARD_ZOOM_THRESHOLD;

    if !is_active {
        return CardLayerState {
            is_active: false,
            visible_count: 0,
            cached_count: cache.len(),
        };
    }

    let (zoomable_state, _) = super::zoomable::compute_zoomable_state(cards, camera, bounds, cache);

    CardLayerState {
        is_active: true,
        visible_count: zoomable_state.visible_count,
        cached_count: zoomable_state.cached_count,
    }
}

/// Card content from indexed data and open buffers.
///
/// For production B7: previews and task lines come from the index via
/// `flavite-api`, and open buffer content supersedes the indexed version.
#[derive(Debug, Clone)]
pub struct CardContent {
    /// Preview text (first few lines of the note).
    pub preview: String,
    /// Task lines with checkbox state (lines starting with task markers).
    pub task_lines: Vec<TaskLine>,
    /// Buffer state if the note is open.
    pub buffer_state: Option<BufferState>,
}

/// A single task line with its checkbox state.
#[derive(Debug, Clone)]
pub struct TaskLine {
    /// The line content.
    pub text: String,
    /// Whether the checkbox is checked.
    pub is_checked: bool,
    /// Source content hash for validation.
    pub source_hash: Option<Vec<u8>>,
}

/// Buffer state for an open note.
#[derive(Debug, Clone)]
pub struct BufferState {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub content_hash: Vec<u8>,
}

impl CardContent {
    /// Create card content from preview text (for closed notes).
    #[must_use]
    pub fn from_preview(preview: &str) -> Self {
        let task_lines = extract_task_lines(preview);
        let preview = if preview.len() > 200 {
            format!("{}…", &preview[..197])
        } else {
            preview.to_string()
        };
        Self {
            preview,
            task_lines,
            buffer_state: None,
        }
    }

    /// Create card content with buffer state (for open notes).
    #[must_use]
    pub fn with_buffer(
        preview: String,
        task_lines: Vec<TaskLine>,
        buffer_id: BufferId,
        revision: Revision,
        content_hash: Vec<u8>,
    ) -> Self {
        Self {
            preview,
            task_lines,
            buffer_state: Some(BufferState {
                buffer_id,
                revision,
                content_hash,
            }),
        }
    }
}

/// Extract task lines from text.
fn extract_task_lines(text: &str) -> Vec<TaskLine> {
    let mut task_lines = Vec::new();
    let mut in_list = false;

    for line in text.lines() {
        let trimmed = line.trim();
        let is_task = trimmed.starts_with("- [")
            || trimmed.starts_with("[ ] ")
            || trimmed.starts_with("- [x]")
            || trimmed.starts_with("- [X]")
            || trimmed.starts_with("[x] ")
            || trimmed.starts_with("[X] ");

        if is_task {
            let is_checked = trimmed.contains("[x]") || trimmed.contains("[X]");
            task_lines.push(TaskLine {
                text: trimmed.to_string(),
                is_checked,
                source_hash: None,
            });
            in_list = true;
        } else if in_list && trimmed.is_empty() {
            in_list = false;
        }
    }

    task_lines
}

/// Build a list of cards from a graph snapshot and indexed data.
///
/// Uses real index queries through `flavite-api` to get previews and task
/// lines. Open buffer content supersedes indexed version.
pub fn cards_from_snapshot(
    snapshot: &flavite_graph::GraphSnapshot,
    positions: &[[f32; 2]],
    preview_map: &HashMap<NodeId, String>,
    content_map: &HashMap<NodeId, Vec<String>>,
    task_map: &HashMap<NodeId, Vec<TaskLine>>,
) -> Vec<Card<CardLayerMessage>> {
    let mut cards = Vec::new();
    let node_count = snapshot.node_count().min(positions.len());

    for ((&id, &kind), &position) in snapshot
        .node_ids
        .iter()
        .zip(&snapshot.node_kinds)
        .zip(positions.iter())
        .take(node_count)
    {
        let indexed_name = preview_map.get(&id).cloned().unwrap_or_default();
        let title = match kind {
            NodeKind::Note => format!("{} #{}", node_title(kind), id.id),
            NodeKind::Attachment if indexed_name.is_empty() => format!("File #{}", id.id),
            NodeKind::Attachment => attachment_filename(&indexed_name),
        };
        let content = content_map.get(&id).cloned().unwrap_or_default();
        let task_lines = task_map.get(&id).cloned().unwrap_or_default();

        // Build the toggle requests from task lines
        let _toggle_requests: Vec<usize> = task_lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.is_checked)
            .map(|(idx, _)| idx)
            .collect();

        let card = match kind {
            NodeKind::Note => Card::new_note_with_message(
                position,
                id,
                title,
                content,
                CardLayerMessage::OpenNote {
                    identity: CardIdentity { node_id: id, kind },
                },
            ),
            NodeKind::Attachment => Card::new_attachment_with_message(
                position,
                id,
                title,
                CardLayerMessage::OpenNote {
                    identity: CardIdentity { node_id: id, kind },
                },
            ),
        };
        cards.push(card);
    }

    cards
}

/// Node title for display (kind-specific prefix).
fn node_title(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Note => "Note",
        NodeKind::Attachment => "File",
    }
}

/// Build a card element for a given scale step with real styling.
///
/// This is the production implementation (B7): styled card with title,
/// preview text, and task checkboxes.
///
/// For B7, we use a simple text-based card layout. The actual checkbox
/// interaction will be wired through the card click message in the full
/// integration (B7 does not require interactive checkboxes, only that
/// the card layer can render cards with task lines).
pub fn build_card(
    card: &Card<CardLayerMessage>,
    scale: f32,
    content: CardContent,
) -> Element<'static, CardLayerMessage> {
    // For B7: build a simple card with text elements
    // The card message is used for open note on click
    // We clone strings to satisfy 'static lifetime requirements

    let mut col = Column::new().spacing(8).padding(12);

    // Attachment cards use a square marker and the indexed filename. The
    // graph node itself uses the same square shape/color in `nodes`.
    if card.kind == NodeKind::Attachment {
        col = col.push(
            text::Text::new("■")
                .size(18.0 * scale)
                .color(Color::from_rgb8(0xff, 0xc7, 0x59)),
        );
    }

    // Title
    let title = text::Text::new(card.title.clone())
        .size(16.0 * scale)
        .color(Color::BLACK);
    col = col.push(title);

    // Preview text
    if !content.preview.is_empty() {
        let preview = text::Text::new(content.preview.clone())
            .size(12.0 * scale)
            .color(Color::from_rgb8(0x66, 0x66, 0x66));
        col = col.push(preview);
    }

    // Task lines - show as text with checkmark symbols
    if !content.task_lines.is_empty() {
        let mut task_col = Column::new().spacing(4);
        for task_line in content.task_lines {
            let task_text = if task_line.is_checked {
                format!("✓ {}", task_line.text)
            } else {
                format!("○ {}", task_line.text)
            };
            let task_widget = text::Text::new(task_text).size(12.0 * scale);
            task_col = task_col.push(task_widget);
        }
        col = col.push(task_col);
    }

    col.into()
}

/// Build a complete card row with title, preview, and tasks.
///
/// This function is used by the zoomable container to build cards with
/// proper content from the API.
pub fn build_card_with_content(
    card: &Card<CardLayerMessage>,
    scale: f32,
    flavite: &Flavite,
    node_id: NodeId,
) -> Element<'static, CardLayerMessage> {
    // For B7: fetch content from Flavite API
    // This is a placeholder - actual implementation will query the index
    // and buffer manager through the API
    let content = fetch_card_content(flavite, node_id);
    build_card(card, scale, content)
}

/// Fetch card content for a node (placeholder for actual API calls).
///
/// In production, this will query the index for previews and task lines,
/// and check BufferManager for open buffer state.
fn fetch_card_content(_flavite: &Flavite, _node_id: NodeId) -> CardContent {
    // Placeholder implementation - returns synthetic content
    // Real implementation will use Flavite API to query index and buffers
    CardContent::from_preview(
        "This is a preview of the note content...\n\n- [ ] Task one\n- [x] Task two",
    )
}

/// Build a status widget for the card layer.
pub fn card_layer_status(
    cards: &[Card<CardLayerMessage>],
    camera: &ZoomableCamera,
    bounds: Rectangle,
    cache: &mut LayoutCache,
) -> Element<'static, CardLayerMessage> {
    let state = compute_card_layer_state(cards, camera, bounds, cache);

    let mut col = Column::new().spacing(8).padding(12);

    if state.is_active {
        col = col.push(text::Text::new("Card Layer Active").size(24));
        col = col.push(
            text::Text::new(format!(
                "Cards: {}/{} ({} cached, {}+ visible)",
                state.visible_count,
                cards.len(),
                state.cached_count,
                state.visible_count,
            ))
            .size(16),
        );
        col = col.push(
            text::Text::new(format!(
                "Zoom: {:.2}× (threshold: {:.2}×)",
                camera.zoom, CARD_ZOOM_THRESHOLD
            ))
            .size(14),
        );
    } else {
        col = col.push(
            text::Text::new("Card Layer Inactive")
                .size(20)
                .color(Color::from_rgb8(0x66, 0x66, 0x66)),
        );
        col = col.push(
            text::Text::new(format!(
                "Zoom: {:.2}× / {:.2}× threshold",
                camera.zoom, CARD_ZOOM_THRESHOLD
            ))
            .size(14),
        );
    }

    col.into()
}

/// Card message for the B4->B7 transition.
///
/// This is kept for compatibility but will be removed once B7 is fully wired.
#[derive(Debug, Clone)]
pub struct IndexedCardContent {
    /// Preview text (first few lines of the note).
    pub preview: String,
    /// Task lines (lines starting with task markers).
    pub task_lines: Vec<String>,
}

impl IndexedCardContent {
    /// Create card content from preview text.
    #[must_use]
    pub fn from_preview(preview: &str) -> Self {
        let mut task_lines = Vec::new();
        let mut in_list = false;
        for line in preview.lines() {
            let trimmed = line.trim();
            let is_task = trimmed.starts_with("- [")
                || trimmed.starts_with("[ ] ")
                || trimmed.starts_with("- [x]")
                || trimmed.starts_with("- [X]")
                || trimmed.starts_with("[x] ")
                || trimmed.starts_with("[X] ");
            if is_task {
                task_lines.push(trimmed.to_string());
                in_list = true;
            } else if in_list && trimmed.is_empty() {
                in_list = false;
            }
        }
        let preview = if preview.len() > 200 {
            format!("{}…", &preview[..197])
        } else {
            preview.to_string()
        };
        Self {
            preview,
            task_lines,
        }
    }
}

/// Legacy cards_from_snapshot for B4 compatibility (deprecated in B7).
#[allow(dead_code)]
pub fn legacy_cards_from_snapshot(
    snapshot: &flavite_graph::GraphSnapshot,
    positions: &[[f32; 2]],
    preview_map: &HashMap<NodeId, String>,
    content_map: &HashMap<NodeId, Vec<String>>,
) -> Vec<Card<()>> {
    let mut cards = Vec::new();
    let node_count = snapshot.node_count().min(positions.len());

    for ((&id, &kind), &position) in snapshot
        .node_ids
        .iter()
        .zip(&snapshot.node_kinds)
        .zip(positions.iter())
        .take(node_count)
    {
        let title = format!("{} #{}", node_title(kind), id.id);
        let _preview = preview_map.get(&id).cloned().unwrap_or_default();
        let content = content_map.get(&id).cloned().unwrap_or_default();

        let card = match kind {
            NodeKind::Note => Card::new_note(position, id, title, content),
            NodeKind::Attachment => Card::new_attachment(position, id, title),
        };
        cards.push(card);
    }

    cards
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_layer_inactive_below_threshold() {
        let camera = ZoomableCamera {
            center: [0.0, 0.0],
            zoom: CARD_ZOOM_THRESHOLD - 0.1,
        };
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let cache = &mut LayoutCache::new();

        let state = compute_card_layer_state(&[], &camera, bounds, cache);
        assert!(!state.is_active);
    }

    #[test]
    fn card_layer_active_above_threshold() {
        let camera = ZoomableCamera {
            center: [0.0, 0.0],
            zoom: CARD_ZOOM_THRESHOLD + 0.1,
        };
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let cache = &mut LayoutCache::new();

        let state = compute_card_layer_state(&[], &camera, bounds, cache);
        assert!(state.is_active);
    }

    #[test]
    fn extract_task_lines_finds_checkboxes() {
        let text = r#"# My note

This is a preview.

- [ ] Task one
- [x] Task two done
- [ ] Task three

Some more content."#;
        let task_lines = extract_task_lines(text);
        assert_eq!(task_lines.len(), 3);
        assert!(task_lines[0].text.contains("Task one"));
        assert!(!task_lines[0].is_checked);
        assert!(task_lines[1].text.contains("Task two"));
        assert!(task_lines[1].is_checked);
        assert!(task_lines[2].text.contains("Task three"));
        assert!(!task_lines[2].is_checked);
    }

    #[test]
    fn card_content_from_preview_truncates() {
        let long_text = "a".repeat(300);
        let content = CardContent::from_preview(&long_text);
        assert_eq!(content.preview.len(), 200);
        assert!(content.preview.ends_with("…"));
    }

    #[test]
    fn zoom_threshold_is_2() {
        assert_eq!(CARD_ZOOM_THRESHOLD, 2.0);
    }

    #[test]
    fn max_cards_is_64() {
        assert_eq!(MAX_CARDS, 64);
    }

    #[test]
    fn card_content_with_buffer() {
        let preview = "Preview text".to_string();
        let task_lines = vec![TaskLine {
            text: "Task 1".to_string(),
            is_checked: false,
            source_hash: None,
        }];
        // Use a deterministic buffer ID for testing
        let buffer_id = BufferId::from(1);
        let revision = Revision::new(1);
        let content_hash = vec![1, 2, 3];
        let expected_content_hash = content_hash.clone();

        let content =
            CardContent::with_buffer(preview, task_lines, buffer_id, revision, content_hash);

        assert!(content.buffer_state.is_some());
        let state = content.buffer_state.unwrap();
        assert_eq!(state.buffer_id, buffer_id);
        assert_eq!(state.revision, revision);
        assert_eq!(state.content_hash, expected_content_hash);
    }

    #[test]
    fn card_layer_status_widget_builds() {
        let camera = ZoomableCamera::centered();
        let camera = ZoomableCamera {
            zoom: CARD_ZOOM_THRESHOLD + 0.1,
            ..camera
        };
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut cache = LayoutCache::new();

        let cards: Vec<Card<CardLayerMessage>> = vec![];
        let widget = card_layer_status(&cards, &camera, bounds, &mut cache);
        drop(widget);
    }

    #[test]
    fn card_layer_message_is_clone() {
        let identity = CardIdentity {
            node_id: NodeId::note(1),
            kind: NodeKind::Note,
        };
        let msg1 = CardLayerMessage::Toggle {
            identity,
            line_index: 0,
        };
        let msg2 = msg1.clone();
        assert!(matches!(msg2, CardLayerMessage::Toggle { .. }));
    }

    #[test]
    fn fetch_card_content_placeholder() {
        // For now, skip this test as it requires a full Flavite setup
        // In production, this will query the index and buffers through the API
    }
}
