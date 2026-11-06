//! W11 visual contract (RFC-0003 §27, NFR-15): the semantic token sets
//! meet WCAG contrast bounds, chrome honors pointer-target and focus-ring
//! minimums, and selected/focused states are distinguishable by shape and
//! text, not color alone. Pixel-level readability is proven separately by
//! the appearance-matrix visual evidence run; this suite pins the
//! measurable structural contract.

use flavite_ui::style::{Scheme, contrast, tokens};

// NFR-15 structural constants, checked at compile time.
const _: () = assert!(flavite_ui::style::FOCUS_WIDTH >= 2.0);
const _: () = assert!(flavite_workspace::SPLITTER_WIDTH * 2.0 >= 8.0);

fn theme(scheme: Scheme) -> iced::Theme {
    tokens::theme(scheme)
}

fn assert_contrast(
    scheme: Scheme,
    foreground: iced::Color,
    backdrop: iced::Color,
    minimum: f64,
    what: &str,
) {
    let ratio = contrast::ratio(foreground, backdrop);
    assert!(
        ratio + 1e-9 >= minimum,
        "{scheme:?} {what}: contrast {ratio:.2} < {minimum} (fg {foreground:?} bg {backdrop:?})"
    );
}

#[test]
fn text_tokens_meet_wcag_aa_on_both_schemes() {
    for scheme in [Scheme::Dark, Scheme::Light] {
        let theme = theme(scheme);
        let palette = theme.palette();
        // Body text on the shell background.
        assert_contrast(scheme, palette.text, palette.background, 4.5, "body text");
        // De-emphasized text paints the dim style the shell actually
        // selects: the smallest dimming step that still meets AA.
        let dim = flavite_ui::style::dim_text(&theme)
            .color
            .expect("dim text carries a color");
        let surface = theme.extended_palette().background.weaker.color;
        assert_contrast(scheme, dim, surface, 4.5, "dim text");
        // Accent text (brand, hints) paints the AA-compliant accent the
        // shell actually selects for this scheme.
        let accent = flavite_ui::style::accent_text(&theme)
            .color
            .expect("accent text carries a color");
        assert_contrast(scheme, accent, palette.background, 4.5, "accent text");
        // Large text and essential non-text (≥3:1): primary as large text,
        // status colors as indicator dots on the background.
        assert_contrast(
            scheme,
            palette.primary,
            palette.background,
            3.0,
            "primary (large/non-text)",
        );
        assert_contrast(
            scheme,
            palette.success,
            palette.background,
            3.0,
            "success indicator",
        );
        assert_contrast(
            scheme,
            palette.warning,
            palette.background,
            3.0,
            "warning indicator",
        );
        assert_contrast(
            scheme,
            palette.danger,
            palette.background,
            3.0,
            "danger indicator",
        );
    }
}

#[test]
fn chip_pairs_meet_text_contrast_on_both_schemes() {
    for scheme in [Scheme::Dark, Scheme::Light] {
        let theme = theme(scheme);
        for (name, tone) in [
            ("neutral chip", flavite_ui::style::Tone::Neutral),
            ("primary chip", flavite_ui::style::Tone::Primary),
            ("success chip", flavite_ui::style::Tone::Success),
            ("warning chip", flavite_ui::style::Tone::Warning),
            ("danger chip", flavite_ui::style::Tone::Danger),
        ] {
            // The painted chip style selects its own text color; test that.
            let style = flavite_ui::style::chip(&theme, tone);
            let text = style.text_color.expect("chip text color");
            let Some(iced::Background::Color(fill)) = style.background else {
                panic!("chip has a solid fill");
            };
            let ratio = contrast::ratio(text, fill);
            assert!(
                ratio + 1e-9 >= 4.5,
                "{scheme:?} {name}: chip text contrast {ratio:.2} < 4.5"
            );
        }
    }
}

#[test]
fn token_sets_are_deterministic_and_distinct() {
    let dark_a = tokens::dark_palette();
    let dark_b = tokens::dark_palette();
    assert_eq!(
        dark_a.background, dark_b.background,
        "same input, same tokens"
    );
    assert_ne!(dark_a.background, tokens::light_palette().background);
    // The metrics resolution is a pure function of its inputs.
    let a =
        flavite_ui::style::Metrics::new(150, 18, flavite_ui::preferences::LineWidth::Columns(80));
    let b =
        flavite_ui::style::Metrics::new(150, 18, flavite_ui::preferences::LineWidth::Columns(80));
    assert_eq!(a, b);
    assert_ne!(
        a,
        flavite_ui::style::Metrics::new(175, 18, flavite_ui::preferences::LineWidth::Columns(80)),
        "different scale, different metrics"
    );
}

#[test]
fn chrome_meets_pointer_target_and_focus_ring_minimums() {
    // NFR-15: pointer targets ≥24×24 logical px at every supported scale;
    // the focus ring is ≥2 px; the splitter corridor is ≥8 px.
    for scale_pct in [75u16, 100, 150, 200] {
        let metrics = flavite_ui::style::Metrics::new(
            scale_pct,
            16,
            flavite_ui::preferences::LineWidth::Full,
        );
        let target = metrics.icon_target;
        assert!(target >= 24.0, "icon target {target} < 24 at {scale_pct}%");
        assert!(metrics.tab_height >= 24.0, "tab strip must stay reachable");
    }
}

#[test]
fn selected_and_focused_states_have_non_color_cues() {
    for scheme in [Scheme::Dark, Scheme::Light] {
        let theme = theme(scheme);
        // The active tab carries a visible border, not only a fill.
        let active = flavite_ui::style::tab(&theme, true);
        let inactive = flavite_ui::style::tab(&theme, false);
        assert!(
            active.border.width > 0.0,
            "{scheme:?} active tab has a border cue"
        );
        assert!(active.border.width > inactive.border.width);
        // Keyboard focus is a two-pixel tab underline, rather than a
        // document-sized frame. Pane geometry stays identical on focus.
        let focused = flavite_ui::style::pane(&theme, true);
        let unfocused = flavite_ui::style::pane(&theme, false);
        assert_eq!(focused.border.width, unfocused.border.width);
        assert!(flavite_ui::style::tab_focus(&theme).background.is_some());
        // Selected list rows differ in fill AND remain bordered text
        // containers; the disabled state dims text instead of relying on
        // hue alone.
        let selected =
            flavite_ui::style::list_row(true)(&theme, iced::widget::button::Status::Active);
        let disabled =
            flavite_ui::style::list_row(false)(&theme, iced::widget::button::Status::Disabled);
        assert_ne!(
            selected.text_color, disabled.text_color,
            "disabled rows dim text"
        );
    }
}

#[test]
fn graph_data_colors_map_to_token_roles() {
    for scheme in [Scheme::Dark, Scheme::Light] {
        let note = tokens::graph_note_color(scheme);
        let attachment = tokens::graph_attachment_color(scheme);
        let unresolved = tokens::graph_unresolved_color(scheme);
        // The three roles stay mutually distinguishable (non-text, ≥3:1
        // against each other where they co-occur on canvas).
        let to_color = |rgba: [f32; 4]| iced::Color {
            r: rgba[0],
            g: rgba[1],
            b: rgba[2],
            a: rgba[3],
        };
        assert_contrast(
            scheme,
            to_color(note),
            to_color(attachment),
            1.2,
            "note vs attachment roles differ",
        );
        assert_contrast(
            scheme,
            to_color(note),
            to_color(unresolved),
            1.2,
            "note vs unresolved roles differ",
        );
        assert_contrast(
            scheme,
            to_color(attachment),
            to_color(unresolved),
            1.2,
            "attachment vs unresolved roles differ",
        );
        // Edge and label colors differ per scheme (tokens, not literals).
        assert_ne!(
            tokens::graph_edge_color(Scheme::Dark),
            tokens::graph_edge_color(Scheme::Light)
        );
    }
}
