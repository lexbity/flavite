//! Current-buffer/saved reading snapshots, resolved links and bounded embeds.
use crate::{ReadDocument, ReadImage, ReadKind, ReadNode};
use flavite_vault::session::Session;
use flavite_vault::sync::MutexExt;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
const READ_CACHE_BUDGET: usize = 128 * 1024 * 1024;
#[derive(Debug, Default)]
pub(crate) struct ReadResources {
    used: AtomicUsize,
}
impl ReadResources {
    pub(crate) fn reserved(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}
#[derive(Debug)]
struct ReadReservation {
    resources: Arc<ReadResources>,
    charged: Mutex<usize>,
}
// A reservation is lifetime bookkeeping, not document content.
impl PartialEq for ReadReservation {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for ReadReservation {}
impl ReadReservation {
    fn reserve(&self, bytes: usize) -> Result<(), String> {
        self.resources
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|n| *n <= READ_CACHE_BUDGET)
            })
            .map_err(|_| "Reading memory limit; close a Read/Preview view and retry")?;
        *self.charged.lock_recovering() += bytes;
        Ok(())
    }
    fn release(&self, bytes: usize) {
        *self.charged.lock_recovering() -= bytes;
        self.resources.used.fetch_sub(bytes, Ordering::AcqRel);
    }
}
impl Drop for ReadReservation {
    fn drop(&mut self) {
        self.resources
            .used
            .fetch_sub(*self.charged.lock_recovering(), Ordering::AcqRel);
    }
}
pub const MAX_READ_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_EMBEDS: usize = 32;
pub const MAX_EMBED_DEPTH: usize = 4;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadSnapshot {
    pub path: PathBuf,
    pub root_path: PathBuf,
    pub text: Arc<str>,
    pub buffer: Option<(u64, u64)>,
    pub unsaved: bool,
    pub content_hash: [u8; 32],
    pub catalog_version: (u64, u64),
    pub document: ReadDocument,
    /// Source-byte start -> canonical destination; no display path reconstruction.
    pub links: BTreeMap<usize, ReadLink>,
    pub embeds: BTreeMap<usize, EmbedContent>,
    _reservation: Arc<ReadReservation>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadLink {
    Note {
        path: PathBuf,
        anchor: Option<String>,
    },
    Attachment(PathBuf),
    External(String),
    Anchor(String),
    Unavailable(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedContent {
    Note(Arc<ReadSnapshot>),
    Image(ReadImage),
    Unavailable(String),
}
struct ReadBudget {
    bytes: usize,
    embeds: usize,
    images: usize,
}
/// Only these schemes may be handed to an explicit OS-open action.
pub fn allowed_external_url(url: &str) -> bool {
    !url.chars().any(|c| c.is_control() || c.is_whitespace())
        && url.split_once(':').is_some_and(|(scheme, rest)| {
            !rest.is_empty()
                && matches!(
                    scheme.to_ascii_lowercase().as_str(),
                    "http" | "https" | "mailto"
                )
        })
}
pub(crate) fn load(
    session: &Arc<Session>,
    path: &Path,
    expected: Option<(u64, u64)>,
    resources: &Arc<ReadResources>,
) -> Result<Arc<ReadSnapshot>, String> {
    let reservation = Arc::new(ReadReservation {
        resources: resources.clone(),
        charged: Mutex::new(0),
    });
    let mut budget = ReadBudget {
        bytes: 0,
        embeds: 0,
        images: 0,
    };
    let mut visited = Vec::new();
    let catalog = session.resolver_catalog().map_err(str::to_owned)?;
    let reader = Reader {
        session,
        catalog: &catalog,
        reservation: &reservation,
        root_path: path,
    };
    let result = load_inner(&reader, path, expected, 0, &mut visited, &mut budget)?;
    let now = session.resolver_catalog().map_err(str::to_owned)?;
    if (now.epoch, now.generation) != (catalog.epoch, catalog.generation) {
        return Err("Knowledge catalog changed; retry reading".into());
    }
    Ok(result)
}
struct Reader<'a> {
    session: &'a Arc<Session>,
    catalog: &'a flavite_index::catalog::ResolverCatalog,
    reservation: &'a Arc<ReadReservation>,
    root_path: &'a Path,
}
fn load_inner(
    reader: &Reader<'_>,
    path: &Path,
    expected: Option<(u64, u64)>,
    depth: usize,
    visited: &mut Vec<PathBuf>,
    budget: &mut ReadBudget,
) -> Result<Arc<ReadSnapshot>, String> {
    let session = reader.session;
    let catalog = reader.catalog;
    let reservation = reader.reservation;
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("Invalid vault path".into());
    }
    if visited.iter().any(|p| p == path) {
        return Err("Embed cycle — open embedded note".into());
    }
    let open = session
        .graph_buffer_stamp()
        .into_iter()
        .find(|(_, _, candidate, ..)| {
            candidate.strip_prefix(session.root()).unwrap_or(candidate) == path
        });
    let (text, buffer, unsaved) = if let Some((id, revision, ..)) = open {
        if expected.is_some_and(|stamp| stamp != (id.as_u64(), revision.as_u64())) {
            return Err("Source revision changed; refresh reading".into());
        }
        let view = session
            .shared_buffer_snapshot(id, Some(revision))
            .map_err(|_| "Source changed; retry reading")?;
        (
            view.text,
            Some((id.as_u64(), revision.as_u64())),
            view.state != flavite_buffer::state::BufferState::Clean,
        )
    } else {
        if expected.is_some() {
            return Err("Source buffer closed".into());
        }
        let bytes = session
            .read_content_bounded(path, MAX_READ_BYTES as u64)?
            .ok_or("Missing note")?;
        (
            Arc::from(flavite_syntax::read::normalized(&bytes)?),
            None,
            false,
        )
    };
    budget.bytes = budget
        .bytes
        .checked_add(text.len())
        .ok_or("Reading memory limit")?;
    if budget.bytes > MAX_READ_BYTES {
        return Err("Reading/embedded text exceeds 16 MiB; open separately".into());
    }
    // Reserve conservative parse/tree headroom before allocating the tree.
    reservation.reserve(text.len().checked_mul(16).ok_or("Reading size overflow")?)?;
    let document = flavite_syntax::read::parse_normalized(&text)?;
    let mut links = BTreeMap::new();
    let mut embeds = BTreeMap::new();
    visited.push(path.into());
    let result = (|| {
        let mut nodes: Vec<&ReadNode> = document.nodes.iter().collect();
        while let Some(node) = nodes.pop() {
            nodes.extend(&node.children);
            let ReadKind::Link {
                target,
                wiki,
                embed,
            } = &node.kind
            else {
                continue;
            };
            let link = if !*wiki && allowed_external_url(target) {
                ReadLink::External(target.clone())
            } else if !*wiki && target.contains(':') {
                ReadLink::Unavailable("Unsupported link scheme".into())
            } else if let Some(anchor) = target.strip_prefix('#') {
                ReadLink::Anchor(anchor.into())
            } else {
                let (raw, anchor) = target
                    .split_once('#')
                    .map_or((target.as_str(), None), |(raw, anchor)| {
                        (raw, Some(anchor.to_owned()))
                    });
                let resolved = catalog.resolve_at_path(
                    &flavite_syntax::text::fold_key(raw),
                    raw,
                    *embed,
                    Some(path),
                );
                if resolved.is_ambiguous {
                    ReadLink::Unavailable("Ambiguous link; choose its target in source".into())
                } else if let Some(note) = resolved.note {
                    let paths = session.decode_graph_paths(vec![note.path_native])?;
                    ReadLink::Note {
                        path: paths.into_iter().next().ok_or("Invalid native path")?,
                        anchor,
                    }
                } else if let Some(att) = resolved.attachment {
                    let paths = session.decode_graph_paths(vec![att.path_native])?;
                    ReadLink::Attachment(paths.into_iter().next().ok_or("Invalid native path")?)
                } else {
                    ReadLink::Unavailable("Missing target — locate or create from source".into())
                }
            };
            if *embed {
                let content = if budget.embeds >= MAX_EMBEDS || depth >= MAX_EMBED_DEPTH {
                    EmbedContent::Unavailable("Embed limit — open embedded note".into())
                } else {
                    budget.embeds += 1;
                    match &link {
                        ReadLink::Note { path, .. } => {
                            match load_inner(reader, path, None, depth + 1, visited, budget) {
                                Ok(doc) => EmbedContent::Note(doc),
                                Err(error) => EmbedContent::Unavailable(error),
                            }
                        }
                        ReadLink::Attachment(path) => {
                            if budget.images >= 64 * 1024 * 1024 {
                                EmbedContent::Unavailable(
                                    "Image cache budget reached; open attachment".into(),
                                )
                            } else {
                                const IMAGE_RESERVATION: usize = 80 * 1024 * 1024;
                                reservation.reserve(IMAGE_RESERVATION)?;
                                let decoded = session
                                    .read_content_bounded(
                                        path,
                                        crate::attachments::MAX_ATTACHMENT_BYTES,
                                    )
                                    .and_then(|bytes| bytes.ok_or("Attachment unavailable".into()))
                                    .and_then(|bytes| {
                                        crate::attachments::decode_image(&bytes)
                                            .map_err(str::to_owned)
                                    });
                                reservation.release(
                                    IMAGE_RESERVATION
                                        - decoded.as_ref().map_or(0, |i| i.rgba.len()),
                                );
                                match decoded {
                                    Ok(image)
                                        if budget.images + image.rgba.len() <= 64 * 1024 * 1024 =>
                                    {
                                        budget.images += image.rgba.len();
                                        EmbedContent::Image(image)
                                    }
                                    Ok(_) => {
                                        EmbedContent::Unavailable("Image budget reached".into())
                                    }
                                    Err(message) => EmbedContent::Unavailable(message),
                                }
                            }
                        }
                        _ => EmbedContent::Unavailable(
                            "Remote/missing embed is not loaded; open explicitly".into(),
                        ),
                    }
                };
                embeds.insert(node.range.start, content);
            }
            links.insert(node.range.start, link);
        }
        Ok(Arc::new(ReadSnapshot {
            path: path.into(),
            root_path: reader.root_path.into(),
            content_hash: *blake3::hash(text.as_bytes()).as_bytes(),
            text,
            buffer,
            unsaved,
            document,
            links,
            embeds,
            catalog_version: (catalog.epoch, catalog.generation),
            _reservation: reservation.clone(),
        }))
    })();
    visited.pop();
    result
}
