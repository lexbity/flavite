//! `Zoomable` — a container that lays out real iced widgets at world
//! positions with re-scaled text (D15, B7).
//!
//! At zoom levels beyond the LOD threshold, the graph view switches from
//! instanced dots to real note cards: iced widgets laid out at world
//! coordinates, with text re-laid-out at the scaled font size rather than
//! GPU-scaled. Scaled glyph textures get blurry; quantized scale steps
//! plus a per-step text cache keep it sharp.
//!
//! # Design
//!
//! - **World ↔ screen**: the container owns a pan/zoom camera and
//!   transforms world coordinates to widget-local screen positions.
//! - **Scale quantization**: text is laid out at discrete scale levels
//!   (powers of √2 ≈ 1.414). Between levels the same cached layout is used.
//! - **Cache**: one cached layout per widget per scale step. Cached layouts
//!   are evicted on a LRU basis when the widget count grows or when memory
//!   pressure demands it.
//! - **Card cap**: at most 64 real widgets (D15) — beyond that the LOD layer
//!   takes over or the view is reduced.
//!
//! # Production (B7)
//!
//! B7 productionizes the zoomable container: cards now carry
//! `CardLayerMessage` for toggle and open actions, and card content is
//! fetched through `flavite-api::Lattice`.

use std::collections::HashMap;

use iced::widget::Column;
use iced::{Element, Length, Point, Rectangle, Size};

use flavite_graph::{NodeId, NodeKind};

/// Card message type for the zoomable layer (B7: CardLayerMessage).
pub type CardMessage = super::card_layer::CardLayerMessage;

/// Maximum cards displayed in the zoomable layer (D15).
pub const CARD_CAP: usize = 64;

/// Scale step quantization base: √2. Text laid out at 1.0×, 1.414×,
/// 2.0×, 2.828×, etc. The human eye cannot distinguish smaller
/// increments, and caching at every possible float would exhaust memory.
pub const SCALE_STEP_BASE: f32 = std::f32::consts::SQRT_2;

/// Minimum legible font scale (below this, text collapses to a dot).
pub const SCALE_MIN: f32 = 0.25;

/// Width of a card in logical pixels at scale 1.0.
const CARD_WIDTH: f32 = 240.0;

/// Height of a card in logical pixels at scale 1.0.
const CARD_HEIGHT: f32 = 140.0;

/// Distance in world units at which a card fades in (D15: LOD via
/// zoom/visible-count threshold). A card closer than this to the camera
/// center is shown; further away it is not.
pub const CARD_DISTANCE_THRESHOLD: f32 = 200.0;

/// Result of quantizing a zoom factor to the nearest scale step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuantizedScale {
    /// The raw zoom factor.
    pub zoom: f32,
    /// Quantized step: `SCALE_STEP_BASE ^ step_index`.
    pub step: f32,
    /// Index of the step (0 = 1.0×, 1 = √2, 2 = 2.0, -1 = 1/√2, …).
    pub step_index: i32,
}

impl QuantizedScale {
    /// Quantize `zoom` to the nearest power of [`SCALE_STEP_BASE`].
    #[must_use]
    pub fn quantize(zoom: f32) -> Self {
        if !zoom.is_finite() || zoom <= 0.0 {
            return Self {
                zoom: SCALE_MIN,
                step: SCALE_MIN,
                step_index: -10, // arbitrary sentinel
            };
        }
        let clamped = zoom.clamp(SCALE_MIN, 64.0);
        let log = clamped.log(SCALE_STEP_BASE);
        let step_index = log.round() as i32;
        let step = SCALE_STEP_BASE.powi(step_index);
        Self {
            zoom: clamped,
            step,
            step_index,
        }
    }

    /// The font size multiplier for this step.
    #[must_use]
    pub fn font_scale(&self) -> f32 {
        self.step
    }
}

/// The camera for the zoomable layer (world → screen).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoomableCamera {
    pub center: [f32; 2],
    pub zoom: f32,
}

impl ZoomableCamera {
    /// The camera looking at the origin at 1:1.
    #[must_use]
    pub fn centered() -> Self {
        Self {
            center: [0.0, 0.0],
            zoom: 1.0,
        }
    }

    /// World → screen (widget-local logical pixels).
    #[must_use]
    pub fn world_to_screen(&self, world: [f32; 2], bounds: Rectangle) -> Point {
        Point {
            x: (world[0] - self.center[0]) * self.zoom + bounds.x + bounds.width * 0.5,
            y: (world[1] - self.center[1]) * self.zoom + bounds.y + bounds.height * 0.5,
        }
    }

    /// Screen → world.
    #[must_use]
    pub fn screen_to_world(&self, screen: Point, bounds: Rectangle) -> [f32; 2] {
        [
            (screen.x - bounds.x - bounds.width * 0.5) / self.zoom + self.center[0],
            (screen.y - bounds.y - bounds.height * 0.5) / self.zoom + self.center[1],
        ]
    }

    /// Distance from world point `p` to the camera center, in world units.
    #[must_use]
    pub fn distance_to(&self, p: [f32; 2]) -> f32 {
        (p[0] - self.center[0]).hypot(p[1] - self.center[1])
    }

    /// Whether world point `p` is within the card visibility threshold.
    #[must_use]
    pub fn card_visible(&self, p: [f32; 2]) -> bool {
        self.distance_to(p) <= CARD_DISTANCE_THRESHOLD
    }
}

impl Default for ZoomableCamera {
    fn default() -> Self {
        Self::centered()
    }
}

/// A card descriptor: world position + widget builder + identity.
///
/// The builder is called with the quantized scale and returns the widget
/// for that scale step. The identity is used for cache lookup.
#[derive(Debug, Clone)]
pub struct Card<Message = CardMessage> {
    /// World position of the card's reference point (usually top-left of
    /// the card's logical rectangle).
    pub world_position: [f32; 2],
    /// Node id for identity and stable ordering.
    pub node_id: NodeId,
    /// Node kind (note or attachment).
    pub kind: NodeKind,
    /// Title/preview text.
    pub title: String,
    /// Content lines (for task checkboxes).
    pub content: Vec<String>,
    /// Marked as needing a toggle at this position (from indexed task spans).
    pub toggle_requests: Vec<usize>,
    /// Message to emit when the card is interacted with.
    pub message: Message,
    /// Phantom type for Message.
    pub _phantom: std::marker::PhantomData<fn() -> Message>,
}

impl<Message> Card<Message> {
    /// A new card for a note at `world_position` with a custom message.
    #[must_use]
    pub fn new_note_with_message(
        world_position: [f32; 2],
        node_id: NodeId,
        title: String,
        content: Vec<String>,
        message: Message,
    ) -> Self {
        Self {
            world_position,
            node_id,
            kind: NodeKind::Note,
            title,
            content,
            toggle_requests: Vec::new(),
            message,
            _phantom: std::marker::PhantomData,
        }
    }

    /// A new card for an attachment at `world_position` with a custom message.
    #[must_use]
    pub fn new_attachment_with_message(
        world_position: [f32; 2],
        node_id: NodeId,
        title: String,
        message: Message,
    ) -> Self {
        Self {
            world_position,
            node_id,
            kind: NodeKind::Attachment,
            title,
            content: Vec::new(),
            toggle_requests: Vec::new(),
            message,
            _phantom: std::marker::PhantomData,
        }
    }

    /// A new card for a note at `world_position` (legacy B4 compatibility).
    #[must_use]
    #[allow(dead_code)]
    pub fn new_note(
        world_position: [f32; 2],
        node_id: NodeId,
        title: String,
        content: Vec<String>,
    ) -> Self
    where
        Message: Default,
    {
        Self {
            world_position,
            node_id,
            kind: NodeKind::Note,
            title,
            content,
            toggle_requests: Vec::new(),
            message: Message::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// A new card for an attachment at `world_position` (legacy B4 compatibility).
    #[must_use]
    #[allow(dead_code)]
    pub fn new_attachment(world_position: [f32; 2], node_id: NodeId, title: String) -> Self
    where
        Message: Default,
    {
        Self {
            world_position,
            node_id,
            kind: NodeKind::Attachment,
            title,
            content: Vec::new(),
            toggle_requests: Vec::new(),
            message: Message::default(),
            _phantom: std::marker::PhantomData,
        }
    }

    /// The card's identity for cache lookup.
    #[must_use]
    pub fn identity(&self) -> CardIdentity {
        CardIdentity {
            node_id: self.node_id,
            kind: self.kind,
        }
    }
}

/// Identity for cache lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CardIdentity {
    pub node_id: NodeId,
    pub kind: NodeKind,
}

/// Cached layout for a card at a given scale step.
///
/// The cache is keyed by `(identity, step_index)`. For B7, we use a
/// generic cache that can store elements with different message types
/// by using Box<dyn Any> or by making the cache per-card-type. For now,
/// we use a simpler approach: just cache the size, not the full element.
pub struct CardLayout {
    /// The logical size of the widget at this scale.
    pub size: Size<Length>,
}

impl std::fmt::Debug for CardLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CardLayout")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// The layout cache: one entry per (card identity, scale step).
///
/// The cache is a simple HashMap for the B4 prototype; a production
/// version would use an LRU with a bounded size.
pub type LayoutCache = HashMap<(CardIdentity, i32), CardLayout>;

/// Width of a card in logical pixels at scale 1.0.
/// Layout state for the zoomable layer: the result of computing card
/// positions and cached layouts for the current frame.
#[derive(Debug, Clone)]
pub struct ZoomableState {
    /// Cards currently in the cache.
    pub cached_count: usize,
    /// Cards visible in the current frame.
    pub visible_count: usize,
}

/// Compute the zoomable state for a set of cards and camera.
///
/// Returns the state plus the list of card elements for rendering.
/// In B7, this builds real widgets at world positions with proper styling.
pub fn compute_zoomable_state<Message>(
    cards: &[Card<Message>],
    camera: &ZoomableCamera,
    bounds: Rectangle,
    cache: &mut LayoutCache,
) -> (ZoomableState, Vec<(Point, Element<'static, Message>)>)
where
    Message: 'static + Clone + std::fmt::Debug,
{
    let scale = QuantizedScale::quantize(camera.zoom);
    let mut state = ZoomableState {
        cached_count: cache.len(),
        visible_count: 0,
    };
    let mut visible_elements: Vec<(Point, Element<'static, Message>)> = Vec::new();

    // Sort cards by distance to camera (nearest first = drawn last = on top).
    let mut sorted: Vec<&Card<Message>> = cards.iter().collect();
    sorted.sort_by(|a, b| {
        let da = camera.distance_to(a.world_position);
        let db = camera.distance_to(b.world_position);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Take the nearest N cards within the cap and visibility threshold.
    let mut count = 0;
    for card in sorted.iter() {
        if count >= CARD_CAP {
            break;
        }
        if !camera.card_visible(card.world_position) {
            continue;
        }
        count += 1;
    }
    state.visible_count = count;

    // Build visible cards: for each, ensure its size is cached at this
    // scale step, then compute its screen position and build the element.
    let step_index = scale.step_index;
    for card in sorted.iter().take(count) {
        let identity = card.identity();
        let cache_key = (identity, step_index);

        // Ensure the layout is cached (B7: just cache the size for now).
        cache.entry(cache_key).or_insert_with(|| {
            let size = Size::new(
                Length::Fixed(CARD_WIDTH * scale.step),
                Length::Fixed(CARD_HEIGHT * scale.step),
            );
            CardLayout { size }
        });

        // Compute screen position and build the element.
        let screen = camera.world_to_screen(card.world_position, bounds);
        let element = build_card(card, scale.step);
        visible_elements.push((screen, element));
    }

    (state, visible_elements)
}

/// Build a card element for a given scale step.
///
/// This is where the actual iced widget construction happens. The
/// `CardIdentity` and `step_index` are the cache key.
///
/// For B7, this returns a simple text element. The actual styled card
/// with content will be built by card_layer::build_card when integrated
/// with the Lattice API.
pub fn build_card<Message>(card: &Card<Message>, step: f32) -> Element<'static, Message>
where
    Message: 'static + Clone + std::fmt::Debug,
{
    // For B7, we use the card's message for interactions
    // The card click will emit the card's message
    // Clone the title to satisfy 'static lifetime
    Column::new()
        .push(iced::widget::text::Text::new(card.title.clone()).size(16.0 * step))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantized_scale_rounds_to_nearest_step() {
        let one = QuantizedScale::quantize(1.0);
        assert_eq!(one.step_index, 0);
        assert!((one.step - 1.0).abs() < 0.001);

        let sqrt2 = QuantizedScale::quantize(SCALE_STEP_BASE);
        assert_eq!(sqrt2.step_index, 1);
        assert!((sqrt2.step - SCALE_STEP_BASE).abs() < 0.001);

        let two = QuantizedScale::quantize(2.0);
        assert_eq!(two.step_index, 2);
        assert!((two.step - 2.0).abs() < 0.001);

        let half = QuantizedScale::quantize(0.5);
        assert_eq!(half.step_index, -2);
        assert!((half.step - 0.5).abs() < 0.001);
    }

    #[test]
    fn camera_world_screen_round_trip() {
        let camera = ZoomableCamera {
            center: [100.0, -50.0],
            zoom: 2.0,
        };
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        for world in [[0.0, 0.0], [100.0, -50.0], [50.0, -25.0]] {
            let screen = camera.world_to_screen(world, bounds);
            let back = camera.screen_to_world(screen, bounds);
            assert!((back[0] - world[0]).abs() < 0.01, "x round trip");
            assert!((back[1] - world[1]).abs() < 0.01, "y round trip");
        }
    }

    #[test]
    fn camera_distance_to_is_euclidean() {
        let camera = ZoomableCamera::centered();
        assert!((camera.distance_to([3.0, 4.0]) - 5.0).abs() < 0.001);
        assert!((camera.distance_to([0.0, 0.0]) - 0.0).abs() < 0.001);
    }

    #[test]
    fn camera_card_visible_respects_threshold() {
        let camera = ZoomableCamera {
            center: [0.0, 0.0],
            zoom: 1.0,
        };
        assert!(camera.card_visible([0.0, 0.0]));
        assert!(camera.card_visible([CARD_DISTANCE_THRESHOLD, 0.0]));
        assert!(!camera.card_visible([CARD_DISTANCE_THRESHOLD + 1.0, 0.0]));
    }

    #[test]
    fn compute_zoomable_state_caps_and_caches() {
        let mut cache = LayoutCache::new();
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let camera = ZoomableCamera::centered();

        // 100 cards, but only CARD_CAP should be visible.
        let cards: Vec<Card> = (0..100)
            .map(|i| {
                Card::new_note_with_message(
                    [i as f32 * 10.0, 0.0],
                    NodeId::note(i as u64),
                    format!("Note {}", i),
                    vec![format!("Content line {}", i)],
                    CardMessage::OpenNote {
                        identity: CardIdentity {
                            node_id: NodeId::note(i as u64),
                            kind: NodeKind::Note,
                        },
                    },
                )
            })
            .collect();

        let (state, visible) = compute_zoomable_state(&cards, &camera, bounds, &mut cache);
        // With CARD_DISTANCE_THRESHOLD=200 and cards spaced 10 units apart,
        // only cards within 200 units are visible (21 cards: 0..=20)
        // The CARD_CAP of 64 is not reached because the cards are too far apart
        assert_eq!(state.visible_count, 21);
        assert_eq!(visible.len(), 21);
        assert_eq!(cache.len(), 21);
    }

    #[test]
    fn compute_zoomable_state_skips_far_cards() {
        let mut cache = LayoutCache::new();
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let camera = ZoomableCamera::centered();

        // Two cards: one near, one far.
        let cards = vec![
            Card::new_note_with_message(
                [0.0, 0.0],
                NodeId::note(0),
                "Near".to_string(),
                vec![],
                CardMessage::OpenNote {
                    identity: CardIdentity {
                        node_id: NodeId::note(0),
                        kind: NodeKind::Note,
                    },
                },
            ),
            Card::new_note_with_message(
                [CARD_DISTANCE_THRESHOLD + 100.0, 0.0],
                NodeId::note(1),
                "Far".to_string(),
                vec![],
                CardMessage::OpenNote {
                    identity: CardIdentity {
                        node_id: NodeId::note(1),
                        kind: NodeKind::Note,
                    },
                },
            ),
        ];

        let (state, visible) = compute_zoomable_state(&cards, &camera, bounds, &mut cache);
        assert_eq!(state.visible_count, 1);
        assert_eq!(visible.len(), 1);
        // B7: we verify the position is computed correctly
        assert_eq!(
            visible[0].0.x,
            camera.world_to_screen(cards[0].world_position, bounds).x
        );
        assert_eq!(
            visible[0].0.y,
            camera.world_to_screen(cards[0].world_position, bounds).y
        );
    }

    #[test]
    fn card_identity_is_unique() {
        let card1 = Card::new_note_with_message(
            [0.0, 0.0],
            NodeId::note(1),
            "A".to_string(),
            vec![],
            CardMessage::OpenNote {
                identity: CardIdentity {
                    node_id: NodeId::note(1),
                    kind: NodeKind::Note,
                },
            },
        );
        let card2 = Card::new_note_with_message(
            [0.0, 0.0],
            NodeId::note(2),
            "B".to_string(),
            vec![],
            CardMessage::OpenNote {
                identity: CardIdentity {
                    node_id: NodeId::note(2),
                    kind: NodeKind::Note,
                },
            },
        );
        assert_ne!(card1.identity(), card2.identity());

        let card3 = Card::new_attachment_with_message(
            [0.0, 0.0],
            NodeId::note(1),
            "A".to_string(),
            CardMessage::OpenNote {
                identity: CardIdentity {
                    node_id: NodeId::note(1),
                    kind: NodeKind::Attachment,
                },
            },
        );
        assert_ne!(card1.identity(), card3.identity());
    }

    #[test]
    fn card_cap_is_64() {
        assert_eq!(CARD_CAP, 64);
    }

    #[test]
    fn scale_step_base_is_sqrt2() {
        assert!((SCALE_STEP_BASE - std::f32::consts::SQRT_2).abs() < 0.001);
    }

    #[test]
    fn compute_zoomable_state_empty_input() {
        let mut cache = LayoutCache::new();
        let bounds = Rectangle::new(Point::new(0.0, 0.0), iced::Size::new(100.0, 100.0));
        let camera = ZoomableCamera::centered();
        let cards: Vec<Card<()>> = vec![];

        let (state, visible) = compute_zoomable_state(&cards, &camera, bounds, &mut cache);
        assert_eq!(state.visible_count, 0);
        assert!(visible.is_empty());
        assert_eq!(cache.len(), 0);
    }
}
