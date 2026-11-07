//! Theme package codec.
//!
//! A browser theme is a code-less extension package: a zip archive whose
//! `manifest.json` carries a `theme` key (plus an optional `dark_theme`
//! variant) with `images`, `colors`, and `properties`. This crate is the
//! pure boundary for those packages: parse and validate the manifest, map
//! the browser color keys onto flavite's semantic surface (recording
//! everything that is dropped), decode theme images including APNG
//! animation, and rewrite a standard package as a `.flavite.xpi` that
//! carries the normalized keys plus a flavite conversion report.
//!
//! The crate is pure: no filesystem, no shell, no toolkit. Storage and
//! rendering live above this boundary (flavite-api owns installed-theme
//! storage; flavite-ui owns color resolution and drawing). Every decode is
//! bounded: byte, pixel, frame, and total-output budgets are hard errors,
//! never truncation.

#![forbid(unsafe_code)]

pub mod apng;
pub mod colors;
pub mod convert;
pub mod manifest;
pub mod model;

pub use apng::{
    AnimationFrame, MAX_FRAMES, MAX_THEME_IMAGE_BYTES, MAX_TOTAL_DECODED_BYTES, ThemeImage,
};
pub use colors::{Rgba, parse_color, parse_color_value};
pub use convert::{ConvertedTheme, convert_xpi, write_package};
pub use manifest::{MANIFEST_NAME, ThemeManifest, ThemeObject, parse_manifest};
pub use model::{ConversionReport, DroppedKey, FLAVITE_REPORT_KEY, SemanticTheme, map_theme};

/// Why a theme operation failed. Whole-document failures are typed errors;
/// field-level invalid values become report warnings with fallbacks, never
/// corruption.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThemeError {
    /// The bytes are not a zip archive at all.
    #[error("theme package is not a zip archive")]
    NotZip,
    /// The package does not contain a readable root manifest.json.
    #[error("theme package has no manifest.json")]
    NoManifest,
    /// The manifest is not valid JSON or not a JSON object.
    #[error("theme manifest is not valid JSON: {0}")]
    InvalidManifest(String),
    /// The manifest parsed but is not a theme (no `theme` key, or `theme`
    /// carries no colors at all).
    #[error("manifest is not a theme: {0}")]
    NotATheme(String),
    /// An image in the package is unreadable or unsupported.
    #[error("theme image {0}: {1}")]
    Image(String, String),
    /// A decode budget was exceeded (bytes, pixels, frames, or total output).
    #[error("theme exceeds decode limits: {0}")]
    TooLarge(String),
    /// The zip archive structure failed to read.
    #[error("theme package unreadable: {0}")]
    Zip(String),
    /// A write of the converted package failed.
    #[error("theme package write failed: {0}")]
    Write(String),
}
