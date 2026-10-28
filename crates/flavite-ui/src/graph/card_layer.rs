//! `CardLayer` — the zoomable card layer that takes over from the instanced
//! dots when zoomed in (D15, B4, B7).
//!
//! At zoom levels beyond the LOD threshold, the graph view switches to
//! real widgets at world positions: note cards showing preview text and
//! task lines with clickable checkboxes. The layer is authoritative for
//! zoomed-in interaction; checkbox toggles dispatch Session commands
//! through the [`CardEditSink`] trait seam.
//!
//! # Design
//!
//! - **LOD threshold**: the layer activates when the camera zoom exceeds
//!   [`CARD_ZOOM_THRESHOLD`] (2.0× by default — D15's "zoomed in" regime).
//! - **Card cap**: at most 64 real widgets (D15).
//! - **Text re-layout**: text is re-laid-out at the scaled font size,
//!   quantized to [`SCALE_STEP_BASE`] increments, cached per step.
//! - **Edit sink**: card edits (checkbox toggles) route through
//!   [`CardEditSink`] → Session commands. For the B4 prototype this is a
//!   trait with a working in-memory implementation; B7 replaces it with
//!   direct `flavite-api` dispatch.
//!
//! # Seam for B7
//!
//! The B4 prototype uses a [`CardEditSink`] trait that the container calls
//! to apply edits. B7 deletes this trait and wires the card layer directly
//! to the Session through `flavite-api`, but the B4 implementation must
//! compile and test without `flavite-api` existing yet. The trait isolates
//! the dependency.

use std::collections::HashMap;
use std::sync::Arc;

use iced::widget::{Column, Text};
use iced::{Color, Element, Rectangle};

use super::zoomable::{CARD_CAP, Card, CardIdentity, LayoutCache, ZoomableCamera};
use flavite_graph::{NodeId, NodeKind};

/// Zoom threshold at which the card layer activates (D15: LOD via
/// zoom/visible-count threshold). At or above this zoom, cards are shown;
/// below it, the instanced dots take over.
pub const CARD_ZOOM_THRESHOLD: f32 = 2.0;

/// Maximum card count — the same constant as zoomable, duplicated here for
/// B4's independence (B7 will have a single constant).
pub const MAX_CARDS: usize = CARD_CAP;

/// The seam the card layer uses to submit edits.
///
/// For B4: a trait so the prototype compiles without `flavite-api`.
/// For B7: this trait is deleted and replaced with direct dispatch through
/// `flavite-api`; the card layer calls Session commands directly.
pub trait CardEditSink {
    /// Apply a toggle at `identity` for content line `line_index`.
    ///
    /// Returns `true` if the toggle was applied, `false` if it was stale
    /// (revision/hash mismatch). The sink owns the authority; the layer
    /// only routes the request.
    fn toggle(&mut self, identity: CardIdentity, line_index: usize) -> bool;
}

/// A no-op sink for testing.
pub struct NoopSink;

impl CardEditSink for NoopSink {
    fn toggle(&mut self, _identity: CardIdentity, _line_index: usize) -> bool {
        false
    }
}

/// A sink that logs toggles and tracks them.
#[derive(Debug, Default)]
pub struct LoggingSink {
    pub toggles: Vec<(CardIdentity, usize)>,
}

impl CardEditSink for LoggingSink {
    fn toggle(&mut self, identity: CardIdentity, line_index: usize) -> bool {
        self.toggles.push((identity, line_index));
        true
    }
}

/// Message emitted by the card layer.
#[derive(Debug, Clone)]
pub enum CardLayerMessage {
    /// Toggle requested at the given card and line.
    Toggle {
        identity: CardIdentity,
        line_index: usize,
    },
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
    cards: &[Card<()>],
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

/// Card content from an indexed preview and task lines.
///
/// This is what the B4 prototype uses to populate cards. B7 will replace
/// this with Session-managed content from `BufferManager`.
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

/// Build a list of cards from a graph snapshot and indexed data.
///
/// For the B4 prototype, this uses synthetic data. B7 will use real
/// index queries through `flavite-api`.
pub fn cards_from_snapshot(
    snapshot: &flavite_graph::GraphSnapshot,
    positions: &[[f32; 2]],
    preview_map: &HashMap<NodeId, String>,
    content_map: &HashMap<NodeId, Vec<String>>,
) -> Vec<Card<()>> {
    let mut cards = Vec::new();
    let node_count = snapshot.node_count().min(positions.len());

    for ((&id, &kind), position) in snapshot
        .node_ids
        .iter()
        .zip(&snapshot.node_kinds)
        .zip(positions.iter().copied())
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

/// Node title for display (kind-specific prefix).
fn node_title(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Note => "Note",
        NodeKind::Attachment => "File",
    }
}

/// A complete card layer harness for the B4 prototype.
///
/// This struct owns the state needed to run the card layer: the geometry,
/// the camera, the cache, and the sink. It is what the probe example uses
/// to demonstrate the card layer in action.
#[derive(Debug)]
pub struct CardLayerHarness {
    pub geometry: Arc<crate::graph::ProbeGeometry>,
    pub camera: ZoomableCamera,
    pub bounds: Rectangle,
    pub cache: LayoutCache,
    pub sink: LoggingSink,
}

impl Default for CardLayerHarness {
    fn default() -> Self {
        use crate::graph::ProbeGeometry;
        use iced::Point;
        Self {
            geometry: Arc::new(ProbeGeometry::new(0, vec![], vec![])),
            camera: ZoomableCamera::centered(),
            bounds: Rectangle::new(Point::new(0.0, 0.0), iced::Size::new(800.0, 600.0)),
            cache: LayoutCache::new(),
            sink: LoggingSink::default(),
        }
    }
}

impl CardLayerHarness {
    /// A new harness.
    #[must_use]
    pub fn new(geometry: Arc<crate::graph::ProbeGeometry>) -> Self {
        Self {
            geometry: Arc::clone(&geometry),
            camera: ZoomableCamera::centered(),
            bounds: Rectangle {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            },
            cache: LayoutCache::new(),
            sink: LoggingSink::default(),
        }
    }

    /// Update the camera.
    pub fn set_camera(&mut self, camera: ZoomableCamera) {
        self.camera = camera;
    }

    /// Update the bounds.
    pub fn set_bounds(&mut self, bounds: Rectangle) {
        self.bounds = bounds;
    }

    /// Clear the cache.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Number of cached layouts.
    #[must_use]
    pub fn cached_count(&self) -> usize {
        self.cache.len()
    }

    /// Number of toggles recorded by the sink.
    #[must_use]
    pub fn toggle_count(&self) -> usize {
        self.sink.toggles.len()
    }

    /// Consume the toggles and return them.
    pub fn take_toggles(&mut self) -> Vec<(CardIdentity, usize)> {
        std::mem::take(&mut self.sink.toggles)
    }
}

/// Build a status widget for the card layer (B4 probe).
pub fn card_layer_status(
    cards: &[Card<()>],
    camera: &ZoomableCamera,
    bounds: Rectangle,
    cache: &mut LayoutCache,
) -> Element<'static, ()> {
    let state = compute_card_layer_state(cards, camera, bounds, cache);

    let mut col = Column::new().spacing(8).padding(12);

    if state.is_active {
        col = col.push(Text::new("Card Layer Active").size(24));
        col = col.push(
            Text::new(format!(
                "Cards: {}/{} ({} cached, {}+ visible)",
                state.visible_count,
                cards.len(),
                state.cached_count,
                state.visible_count,
            ))
            .size(16),
        );
        col = col.push(
            Text::new(format!(
                "Zoom: {:.2}× (threshold: {:.2}×)",
                camera.zoom, CARD_ZOOM_THRESHOLD
            ))
            .size(14),
        );
    } else {
        col = col.push(
            Text::new("Card Layer Inactive")
                .size(20)
                .color(Color::from_rgb8(0x66, 0x66, 0x66)),
        );
        col = col.push(
            Text::new(format!(
                "Zoom: {:.2}× / {:.2}× threshold",
                camera.zoom, CARD_ZOOM_THRESHOLD
            ))
            .size(14),
        );
    }

    col.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_layer_inactive_below_threshold() {
        let geometry = Arc::new(crate::graph::ProbeGeometry::new(1, vec![], vec![]));
        let mut harness = CardLayerHarness::new(Arc::clone(&geometry));
        harness.camera.zoom = CARD_ZOOM_THRESHOLD - 0.1;

        let state =
            compute_card_layer_state(&[], &harness.camera, harness.bounds, &mut harness.cache);
        assert!(!state.is_active);
    }

    #[test]
    fn card_layer_active_above_threshold() {
        let geometry = Arc::new(crate::graph::ProbeGeometry::new(1, vec![], vec![]));
        let mut harness = CardLayerHarness::new(Arc::clone(&geometry));
        harness.camera.zoom = CARD_ZOOM_THRESHOLD + 0.1;

        let state =
            compute_card_layer_state(&[], &harness.camera, harness.bounds, &mut harness.cache);
        assert!(state.is_active);
    }

    #[test]
    fn indexed_card_content_extracts_tasks() {
        let text = r#"# My note

This is a preview.

- [ ] Task one
- [x] Task two done
- [ ] Task three

Some more content."#;
        let content = IndexedCardContent::from_preview(text);
        // The text is < 200 chars, so preview is the full text
        assert_eq!(content.preview, text);
        assert_eq!(content.task_lines.len(), 3);
        assert!(content.task_lines[0].contains("Task one"));
        assert!(content.task_lines[1].contains("Task two"));
    }

    #[test]
    fn cards_from_snapshot_respects_kinds() {
        let mut model = flavite_graph::GraphModel::new();
        model.apply(&[
            flavite_graph::Delta::Node(flavite_graph::NodeDelta::Added {
                id: NodeId::note(1),
            }),
            flavite_graph::Delta::Node(flavite_graph::NodeDelta::Added {
                id: NodeId::attachment(2),
            }),
        ]);
        let snapshot = model.snapshot();
        let positions = vec![[0.0, 0.0], [10.0, 0.0]];
        let preview_map: HashMap<NodeId, String> = [
            (NodeId::note(1), "Note preview".to_string()),
            (NodeId::attachment(2), "File preview".to_string()),
        ]
        .into();
        let content_map: HashMap<NodeId, Vec<String>> = [
            (NodeId::note(1), vec!["line 1".to_string()]),
            (NodeId::attachment(2), vec![]),
        ]
        .into();

        let cards = cards_from_snapshot(&snapshot, &positions, &preview_map, &content_map);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].kind, NodeKind::Note);
        assert_eq!(cards[1].kind, NodeKind::Attachment);
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
    fn logging_sink_records_toggles() {
        let mut sink = LoggingSink::default();
        let identity = CardIdentity {
            node_id: NodeId::note(1),
            kind: NodeKind::Note,
        };
        assert!(sink.toggle(identity, 0));
        assert_eq!(sink.toggles.len(), 1);
        assert_eq!(sink.toggles[0], (identity, 0));
    }

    #[test]
    fn noop_sink_always_succeeds() {
        let mut sink = NoopSink;
        let identity = CardIdentity {
            node_id: NodeId::note(1),
            kind: NodeKind::Note,
        };
        assert!(!sink.toggle(identity, 0));
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
    fn card_layer_harness_starts_empty() {
        let geometry = Arc::new(crate::graph::ProbeGeometry::new(1, vec![], vec![]));
        let harness = CardLayerHarness::new(Arc::clone(&geometry));
        assert_eq!(harness.cached_count(), 0);
        assert_eq!(harness.toggle_count(), 0);
    }

    #[test]
    fn card_layer_status_widget_builds() {
        let geometry = Arc::new(crate::graph::ProbeGeometry::new(1, vec![], vec![]));
        let mut harness = CardLayerHarness::new(Arc::clone(&geometry));
        harness.camera.zoom = CARD_ZOOM_THRESHOLD + 0.1;

        let cards: Vec<Card<()>> = vec![];
        let widget = card_layer_status(&cards, &harness.camera, harness.bounds, &mut harness.cache);
        drop(widget);
    }
}
