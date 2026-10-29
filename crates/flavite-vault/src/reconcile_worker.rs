//! Filesystem-only reconcile worker (D14).
//!
//! This module deliberately has no index or buffer dependencies. It returns
//! immutable filesystem observations; the VaultActor applies them after the
//! per-path save-watermark check.

use crate::types::{ReconcileMode, ReconcileScope};
use flavite_fs::Fs;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct ReconcileJob {
    pub id: u64,
    pub scope: ReconcileScope,
    pub mode: ReconcileMode,
    pub watermark: u64,
    pub nominated: HashSet<PathBuf>,
    pub reply: Option<crossbeam::channel::Sender<crate::actor::VaultActorResp>>,
}

#[derive(Debug, Clone)]
pub(crate) enum ScanOutcome {
    Note {
        path: PathBuf,
        path_key: Vec<u8>,
        display_path: String,
        size: u64,
        mtime_ns: i64,
        bytes: Option<Vec<u8>>,
        parsed: Option<flavite_syntax::pulldown_glue::ParsedNote>,
    },
    Attachment {
        path: PathBuf,
        path_key: Vec<u8>,
        display_path: String,
        size: u64,
        mtime_ns: i64,
        placeholder: bool,
        hash: Option<[u8; 32]>,
        bytes: Option<Vec<u8>>,
    },
    Missing(PathBuf),
}

#[derive(Debug, Default)]
pub(crate) struct ScanResult {
    pub outcomes: Vec<ScanOutcome>,
    pub present_notes: HashSet<Vec<u8>>,
    pub present_attachments: HashSet<Vec<u8>>,
    /// If traversal/read metadata was incomplete, absence cannot safely mean deletion.
    pub incomplete: bool,
    pub total: u32,
    pub full_scan: bool,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanProgress {
    pub job_id: u64,
    pub done: u32,
    pub total: u32,
}

pub(crate) enum WorkerMessage {
    Progress(ScanProgress),
    Batch {
        job_id: u64,
        outcomes: Vec<ScanOutcome>,
    },
    Complete {
        job_id: u64,
        result: ScanResult,
    },
}

pub(crate) fn scan(
    fs: &dyn Fs,
    root: &Path,
    job: &ReconcileJob,
    progress: &crossbeam::channel::Sender<WorkerMessage>,
) -> ScanResult {
    let requested_paths = if !job.nominated.is_empty() {
        Some(job.nominated.clone())
    } else if let ReconcileScope::Single(path) = &job.scope {
        Some(HashSet::from([path.clone()]))
    } else {
        None
    };
    let fallback_full = requested_paths.as_ref().is_some_and(|paths| {
        paths.iter().any(|path| {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            };
            match fs.exists(&path) {
                Ok(false) => false,
                Ok(true) => fs.metadata(&path).map_or(true, |metadata| metadata.is_dir),
                Err(_) => true,
            }
        })
    });
    let nominated = if fallback_full { None } else { requested_paths };
    let full_scan = nominated.is_none();
    let (paths, traversal_incomplete) = if let Some(nominated) = nominated {
        (
            nominated
                .into_iter()
                .map(|path| {
                    if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    }
                })
                .collect::<Vec<_>>(),
            false,
        )
    } else {
        walk_paths(fs, root)
    };
    let mut result = ScanResult {
        total: paths.len().min(u32::MAX as usize) as u32,
        incomplete: traversal_incomplete,
        full_scan,
        ..ScanResult::default()
    };
    for (index, path) in paths.into_iter().enumerate() {
        if index > 0 && index.is_multiple_of(128) && !result.outcomes.is_empty() {
            let outcomes = std::mem::take(&mut result.outcomes);
            if progress
                .send(WorkerMessage::Batch {
                    job_id: job.id,
                    outcomes,
                })
                .is_err()
            {
                result.incomplete = true;
                return result;
            }
        }
        if index.is_multiple_of(128) || index + 1 == result.total as usize {
            let _ = progress.try_send(WorkerMessage::Progress(ScanProgress {
                job_id: job.id,
                done: index.min(u32::MAX as usize) as u32,
                total: result.total,
            }));
        }
        if !path.starts_with(root) || fs.validate_in_root(root, &path).is_err() {
            result.incomplete = true;
            continue;
        }
        let exists = match fs.exists(&path) {
            Ok(exists) => exists,
            Err(_) => {
                result.incomplete = true;
                continue;
            }
        };
        if !exists {
            result.outcomes.push(ScanOutcome::Missing(path));
            continue;
        }
        let metadata = match fs.metadata(&path) {
            Ok(metadata) if !metadata.is_dir && !metadata.is_symlink => metadata,
            Ok(_) => continue,
            Err(_) => {
                result.incomplete = true;
                continue;
            }
        };
        let relative = match path.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => {
                result.incomplete = true;
                continue;
            }
        };
        let path_key = match fs.encode_path_key(relative) {
            Ok(key) => key,
            Err(_) => {
                result.incomplete = true;
                continue;
            }
        };
        let display_path = relative.to_string_lossy().replace('\\', "/");
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            result.present_notes.insert(path_key.clone());
            let bytes = if metadata.size <= flavite_index::MAX_NOTE_BYTES as u64 {
                match fs.read_capped(&path, flavite_index::MAX_NOTE_BYTES as u64) {
                    Ok(bytes) => Some(bytes),
                    Err(_) => {
                        result.incomplete = true;
                        None
                    }
                }
            } else {
                None
            };
            let parsed = bytes
                .as_deref()
                .and_then(|bytes| flavite_syntax::pulldown_glue::parse_note(bytes).ok());
            result.outcomes.push(ScanOutcome::Note {
                path,
                path_key,
                display_path,
                size: metadata.size,
                mtime_ns: metadata.mtime_ns,
                bytes,
                parsed,
            });
            continue;
        }
        let Some(_kind) = attachment_kind(&path) else {
            continue;
        };
        result.present_attachments.insert(path_key.clone());
        let placeholder = fs.is_placeholder(&path).unwrap_or(false);
        let hash_cap = flavite_index::types::ATTACHMENT_HASH_MAX as u64;
        let bytes_for_hash = if !placeholder && metadata.size <= hash_cap {
            match fs.read_capped(&path, hash_cap) {
                Ok(bytes) => Some(bytes),
                Err(_) => {
                    result.incomplete = true;
                    None
                }
            }
        } else {
            None
        };
        let hash = bytes_for_hash
            .as_deref()
            .map(|bytes| *blake3::hash(bytes).as_bytes());
        let hydrated = !placeholder && (metadata.size > hash_cap || hash.is_some());
        let bytes = (hydrated && metadata.size <= hash_cap)
            .then(|| bytes_for_hash.clone())
            .flatten();
        result.outcomes.push(ScanOutcome::Attachment {
            path,
            path_key,
            display_path,
            size: metadata.size,
            mtime_ns: metadata.mtime_ns,
            placeholder,
            hash,
            bytes,
        });
    }
    if !result.outcomes.is_empty() {
        let outcomes = std::mem::take(&mut result.outcomes);
        if progress
            .send(WorkerMessage::Batch {
                job_id: job.id,
                outcomes,
            })
            .is_err()
        {
            result.incomplete = true;
            return result;
        }
    }
    let _ = progress.try_send(WorkerMessage::Progress(ScanProgress {
        job_id: job.id,
        done: result.total,
        total: result.total,
    }));
    result.complete = true;
    result
}

fn attachment_kind(path: &Path) -> Option<()> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "svg"
            | "avif"
            | "bmp"
            | "ico"
            | "pdf"
            | "mp3"
            | "wav"
            | "ogg"
            | "flac"
            | "m4a"
            | "mp4"
            | "webm"
            | "mov"
            | "mkv"
            | "zip"
            | "tar"
            | "gz"
            | "tgz"
    )
    .then_some(())
}

fn walk_paths(fs: &dyn Fs, root: &Path) -> (Vec<PathBuf>, bool) {
    let mut stack = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    let mut incomplete = false;
    while let Some(directory) = stack.pop() {
        let entries = match fs.read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        for entry in entries {
            if entry.is_dir {
                if !entry.is_symlink && entry.name != ".flavite" {
                    stack.push(entry.path);
                }
            } else if !entry.is_symlink {
                paths.push(entry.path);
            }
        }
    }
    (paths, incomplete)
}
