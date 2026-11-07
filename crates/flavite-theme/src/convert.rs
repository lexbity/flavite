//! Standard `.xpi` → `.flavite.xpi` conversion.
//!
//! A converted package keeps every original entry verbatim (images,
//! locales, everything except the manifest) and replaces `manifest.json`
//! with a normalized copy that carries the original keys plus a reserved
//! `flavite` key holding the [`ConversionReport`]. Identity is the gecko
//! add-on id when present, else a blake3 content hash — so the same
//! package always converts to the same theme id.
//!
//! Reading is bounded: entry count, per-entry size, and total
//! uncompressed size are hard errors. Entry names must be safe relative
//! paths, because the storage layer above writes entries to disk by name.

use crate::ThemeError;
use crate::apng::{ThemeImage, decode_image};
use crate::manifest::{MANIFEST_NAME, ThemeManifest, parse_manifest};
use crate::model::{ConversionReport, FLAVITE_REPORT_KEY, MappedTheme, map_theme};
use serde_json::Value;
use std::io::{Cursor, Read, Write};

/// The flavite theme-package format version written into the report.
pub const FLAVITE_FORMAT_VERSION: u32 = 1;
/// Maximum encoded bytes for one theme package.
pub const MAX_PACKAGE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum entries in one theme package.
pub const MAX_PACKAGE_ENTRIES: usize = 512;
/// Maximum total uncompressed bytes across all entries.
pub const MAX_PACKAGE_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// A fully converted theme package.
#[derive(Debug, Clone, PartialEq)]
pub struct ConvertedTheme {
    /// The resolved theme identity.
    pub id: String,
    /// The base (light) mapping.
    pub light: MappedTheme,
    /// The `dark_theme` mapping, when the package has one.
    pub dark: Option<MappedTheme>,
    /// The report embedded in the converted manifest.
    pub report: ConversionReport,
    /// The decoded base `theme_frame`, when present and decodable.
    pub theme_frame: Option<ThemeImage>,
    /// The decoded dark-variant `theme_frame`, when present.
    pub dark_theme_frame: Option<ThemeImage>,
    /// The `.flavite.xpi` contents as `(archive path, bytes)` pairs, in
    /// archive order: the normalized manifest plus every original entry.
    pub files: Vec<(String, Vec<u8>)>,
}

/// Convert a standard theme package into a [`ConvertedTheme`].
///
/// # Errors
/// Typed failures for non-zip input, missing or non-theme manifests,
/// unreadable `theme_frame` images, budget violations, and unsafe entry
/// names.
pub fn convert_xpi(package: &[u8]) -> Result<ConvertedTheme, ThemeError> {
    if package.len() > MAX_PACKAGE_BYTES {
        return Err(ThemeError::TooLarge(format!(
            "package is {} bytes, over the {MAX_PACKAGE_BYTES} byte bound",
            package.len()
        )));
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(package))
        // An unreadable archive is, for this codec's purposes, not a zip
        // archive: the EOCD-missing case is exactly a non-zip input.
        .map_err(|_| ThemeError::NotZip)?;
    if archive.len() > MAX_PACKAGE_ENTRIES {
        return Err(ThemeError::TooLarge(format!(
            "package has {} entries, over the {MAX_PACKAGE_ENTRIES} bound",
            archive.len()
        )));
    }

    // Two passes: sizes and names first (so a hostile entry fails before
    // any image is decoded), then bytes.
    let mut total: u64 = 0;
    let mut names = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| ThemeError::Zip(error.to_string()))?;
        let name = entry
            .name()
            .map_err(|error| ThemeError::Zip(error.to_string()))?
            .to_string();
        if !is_safe_entry_name(&name) {
            return Err(ThemeError::Zip(format!(
                "entry name {name:?} is not a safe relative path"
            )));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_PACKAGE_TOTAL_BYTES {
            return Err(ThemeError::TooLarge(format!(
                "package expands past the {MAX_PACKAGE_TOTAL_BYTES} byte bound"
            )));
        }
        names.push(name);
    }
    if !names.iter().any(|name| name == MANIFEST_NAME) {
        return Err(ThemeError::NoManifest);
    }

    let mut entries = Vec::with_capacity(names.len());
    let mut manifest_text = None;
    for name in &names {
        let mut entry = archive
            .by_name(name)
            .map_err(|error| ThemeError::Zip(error.to_string()))?;
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| ThemeError::Zip(error.to_string()))?;
        if name == MANIFEST_NAME {
            manifest_text =
                Some(String::from_utf8(bytes).map_err(|_| {
                    ThemeError::InvalidManifest("manifest.json is not UTF-8".into())
                })?);
        } else {
            entries.push((name.clone(), bytes));
        }
    }
    let manifest_text = manifest_text.ok_or(ThemeError::NoManifest)?;
    let (manifest, parse_warnings) = parse_manifest(&manifest_text)?;

    // Identity is stable across reconversion: a package that already
    // carries a flavite report keeps its id; otherwise the gecko id, else
    // the content hash. Converting twice must not mint two themes.
    let id = manifest
        .id
        .clone()
        .or_else(|| existing_report_id(&manifest))
        .unwrap_or_else(|| format!("theme-{}", hex_prefix(package)));
    let light = map_theme(&manifest.theme, "light");
    let dark = manifest
        .dark_theme
        .as_ref()
        .map(|variant| map_theme(variant, "dark"));

    let theme_frame = decode_frame(&manifest, &entries, false)?;
    let dark_theme_frame = if manifest.dark_theme.is_some() {
        decode_frame(&manifest, &entries, true)?
    } else {
        None
    };

    let mut warnings = parse_warnings;
    warnings.extend(light.warnings.clone());
    if let Some(dark) = &dark {
        warnings.extend(dark.warnings.clone());
    }
    let animated_frame = theme_frame.as_ref().is_some_and(ThemeImage::is_animated);
    let report = ConversionReport {
        format_version: FLAVITE_FORMAT_VERSION,
        theme_id: id.clone(),
        name: manifest.name.clone(),
        mapped: light.mapped.clone(),
        dropped: light.dropped.clone(),
        warnings,
        animated_frame,
        supports_dark_variant: dark.is_some(),
    };

    let mut files = Vec::with_capacity(entries.len() + 1);
    files.push((
        MANIFEST_NAME.to_owned(),
        normalized_manifest(&manifest, &report)?,
    ));
    files.extend(entries);
    Ok(ConvertedTheme {
        id,
        light,
        dark,
        report,
        theme_frame,
        dark_theme_frame,
        files,
    })
}

/// Write converted package files as a deflate zip archive.
///
/// # Errors
/// [`ThemeError::Write`] when the writer fails or a file name is unsafe.
pub fn write_package<W: Write + std::io::Seek>(
    files: &[(String, Vec<u8>)],
    writer: W,
) -> Result<(), ThemeError> {
    let mut zip = zip::ZipWriter::new(writer);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        if !is_safe_entry_name(name) {
            return Err(ThemeError::Write(format!(
                "entry name {name:?} is not a safe relative path"
            )));
        }
        zip.start_file(name.as_str(), options)
            .map_err(|error| ThemeError::Write(error.to_string()))?;
        zip.write_all(bytes)
            .map_err(|error| ThemeError::Write(error.to_string()))?;
    }
    zip.finish()
        .map_err(|error| ThemeError::Write(error.to_string()))?;
    Ok(())
}

/// The original manifest with the reserved `flavite` report key added.
fn normalized_manifest(
    manifest: &ThemeManifest,
    report: &ConversionReport,
) -> Result<Vec<u8>, ThemeError> {
    let mut normalized = manifest.raw.clone();
    let report_value =
        serde_json::to_value(report).map_err(|error| ThemeError::Write(error.to_string()))?;
    match normalized.get_mut(FLAVITE_REPORT_KEY) {
        Some(Value::Object(existing)) => {
            let Value::Object(report_object) = report_value else {
                return Err(ThemeError::Write("report is not an object".into()));
            };
            for (key, value) in report_object {
                existing.insert(key, value);
            }
        }
        _ => {
            normalized.insert(FLAVITE_REPORT_KEY.to_owned(), report_value);
        }
    }
    serde_json::to_vec_pretty(&normalized).map_err(|error| ThemeError::Write(error.to_string()))
}

/// Decode the `theme_frame` of the base or dark variant. A missing frame
/// is `None`; a declared frame that is not in the package or cannot be
/// decoded is a hard error — a broken header is a broken theme.
fn decode_frame(
    manifest: &ThemeManifest,
    entries: &[(String, Vec<u8>)],
    dark: bool,
) -> Result<Option<ThemeImage>, ThemeError> {
    let variant = if dark {
        manifest.dark_theme.as_ref()
    } else {
        Some(&manifest.theme)
    };
    let Some(reference) = variant.and_then(|object| object.images.get("theme_frame")) else {
        return Ok(None);
    };
    let Some(path) = reference.path() else {
        // Gradients are already recorded as dropped by the mapping.
        return Ok(None);
    };
    let bytes = entries
        .iter()
        .find(|(name, _)| name == path)
        .map(|(_, bytes)| bytes)
        .ok_or_else(|| {
            ThemeError::Image(
                path.to_owned(),
                "the theme_frame file is missing from the package".into(),
            )
        })?;
    decode_image(bytes).map(Some)
}

/// Manifest-relative, forward-slash, no traversal, no NUL: the storage
/// layer writes entries to disk by this name.
fn is_safe_entry_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('\0')
        && !name.contains('\\')
        && !name.starts_with('/')
        && !name
            .split('/')
            .any(|component| component.is_empty() || component == ".")
        && !name.split('/').any(|component| component == "..")
}

/// The theme id of an already-converted package, when its manifest
/// carries one from a previous conversion.
fn existing_report_id(manifest: &ThemeManifest) -> Option<String> {
    manifest
        .raw
        .get(FLAVITE_REPORT_KEY)
        .and_then(|report| report.get("theme_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .filter(|id| !id.is_empty())
}

/// Lowercase hex of the first 8 digest bytes — a stable identity for
/// packages without a gecko id.
fn hex_prefix(package: &[u8]) -> String {
    blake3::hash(package).as_bytes()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const MANIFEST: &str = r##"{
        "manifest_version": 2,
        "name": "Purple Starfield",
        "version": "1.0",
        "theme": {
            "images": { "theme_frame": "header.png" },
            "colors": {
                "frame": "#1a0f38",
                "tab_background_text": "#e8e2ff",
                "toolbar": "#111729",
                "toolbar_field": "#0b0f1a",
                "icons": "#b489f0",
                "ntp_background": "#000000"
            }
        },
        "dark_theme": {
            "colors": { "frame": "#000000", "tab_background_text": "#ffffff" }
        }
    }"##;

    /// Build an in-memory zip package the way a browser would ship one.
    /// Entry insertion is fallible so hostile-name tests can observe the
    /// writer's own refusal separately from the reader's.
    fn build_package(
        manifest: &str,
        extra: &[(&str, Vec<u8>)],
    ) -> Result<Vec<u8>, zip::result::ZipError> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zip.start_file(MANIFEST_NAME, options)?;
            zip.write_all(manifest.as_bytes())?;
            for (name, bytes) in extra {
                zip.start_file(*name, options)?;
                zip.write_all(bytes)?;
            }
            zip.finish()?;
        }
        Ok(buffer.into_inner())
    }

    fn red_frame_png() -> Vec<u8> {
        let mut output = Vec::new();
        let mut encoder = png::Encoder::new(&mut output, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        writer
            .write_image_data(&[255, 0, 0, 255, 255, 0, 0, 255])
            .expect("pixels");
        drop(writer);
        output
    }

    #[test]
    fn a_standard_package_converts_with_report_and_files() {
        let package = build_package(MANIFEST, &[("header.png", red_frame_png())]).expect("package");
        let converted = convert_xpi(&package).expect("conversion");
        assert_eq!(converted.report.name, "Purple Starfield");
        assert!(converted.report.supports_dark_variant);
        assert!(!converted.report.animated_frame);
        assert!(converted.report.mapped.iter().any(|m| m.key == "frame"));
        assert!(
            converted
                .report
                .dropped
                .iter()
                .any(|d| d.key == "ntp_background")
        );
        // Identity falls back to a content hash (no gecko id here).
        assert!(converted.id.starts_with("theme-"));
        // The semantic mapping is available immediately for the UI.
        assert!(converted.light.semantic.top_bar_background.is_some());
        assert!(
            converted
                .dark
                .as_ref()
                .unwrap()
                .semantic
                .window_text
                .is_some()
        );
        // The frame decoded.
        assert_eq!(converted.theme_frame.as_ref().unwrap().width(), 2);
        // Files: normalized manifest + every original entry, manifest first.
        assert_eq!(converted.files[0].0, MANIFEST_NAME);
        assert!(converted.files.iter().any(|(name, _)| name == "header.png"));
        // The normalized manifest carries the original keys plus the report.
        let normalized: serde_json::Value =
            serde_json::from_slice(&converted.files[0].1).expect("manifest json");
        assert!(
            normalized
                .get("theme")
                .and_then(|t| t.get("images"))
                .is_some()
        );
        let report = normalized.get(FLAVITE_REPORT_KEY).expect("flavite key");
        assert_eq!(
            report.get("format_version").and_then(Value::as_u64),
            Some(u64::from(FLAVITE_FORMAT_VERSION))
        );
    }

    #[test]
    fn gecko_identity_wins_over_the_content_hash() {
        let manifest = r##"{"name":"t","browser_specific_settings":{"gecko":{"id":"star@host"}},
            "theme":{"colors":{"frame":"#000"}}}"##;
        let converted =
            convert_xpi(&build_package(manifest, &[]).expect("package")).expect("conversion");
        assert_eq!(converted.id, "star@host");
    }

    #[test]
    fn the_converted_package_round_trips_through_the_reader() {
        let package = build_package(MANIFEST, &[("header.png", red_frame_png())]).expect("package");
        let converted = convert_xpi(&package).expect("conversion");
        let mut rewritten = Cursor::new(Vec::new());
        write_package(&converted.files, &mut rewritten).expect("write");
        // The rewritten package converts again to the same id and report.
        let again = convert_xpi(rewritten.get_ref()).expect("reconversion");
        assert_eq!(again.id, converted.id);
        assert_eq!(again.report, converted.report);
    }

    #[test]
    fn missing_or_broken_packages_are_typed_errors() {
        assert_eq!(convert_xpi(b"not a zip"), Err(ThemeError::NotZip));
        // An empty object manifest is not a theme.
        let empty = build_package("{}", &[]).expect("package");
        assert!(matches!(convert_xpi(&empty), Err(ThemeError::NotATheme(_))));
        // A declared frame image that is missing is a broken theme.
        let broken = build_package(MANIFEST, &[]).expect("package");
        assert!(matches!(convert_xpi(&broken), Err(ThemeError::Image(_, _))));
    }

    #[test]
    fn hostile_entry_names_and_sizes_are_refused() {
        let traversal = build_package(
            r##"{"name":"t","theme":{"colors":{"frame":"#000"}}}"##,
            &[("../evil.txt", b"boom".to_vec())],
        );
        // Either the writer refuses the name outright or the reader must.
        match traversal {
            Err(_) => {}
            Ok(package) => {
                assert!(matches!(convert_xpi(&package), Err(ThemeError::Zip(_))));
            }
        }
        let huge = build_package(
            r##"{"name":"t","theme":{"colors":{"frame":"#000"}}}"##,
            &[("blob.bin", vec![0u8; MAX_PACKAGE_TOTAL_BYTES as usize + 1])],
        )
        .expect("package");
        assert!(matches!(convert_xpi(&huge), Err(ThemeError::TooLarge(_))));
    }
}
