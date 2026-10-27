//! S3 spike, slice 2 of 2 — filesystem-semantics probe (PLAN-C slice C2).
//!
//! Spec basis: §9 Phase 0 spike S3 ("atomic-replace semantics on Windows and a
//! network FS, cloud placeholder detection (notes and attachments),
//! advisory-lock behavior after kill -9"), D6 (atomic write; symlinks are
//! resolved so we write the target; MUST NOT replace a symlink with a regular
//! file), §4.8 (cloud placeholders are never hydrated by background scans),
//! §4.12 (advisory locks die with the process).
//!
//! The probe records per-OS filesystem semantics as evidence for the
//! `flavite-fs` `Fs` trait method contracts (C5) and for the instance-lock
//! design (C8, §4.12/§4.13):
//!
//! 1. **rename-over semantics** — `rename(2)` on Unix: mode/mtime/content
//!    evidence (the inode moves, so mode and mtime travel with it). The
//!    Windows `ReplaceFileW` vs `MoveFileExW` runtime comparison is
//!    source-verified and pending a manual run.
//! 2. **symlink rename-over** — demonstrates the D6 hazard: renaming a temp
//!    file directly over the *link path* replaces the link itself with a
//!    regular file; resolving first (writing the target) preserves the link.
//! 3. **fsync** — file + directory fsync timing per location (a disk-backed
//!    directory and the default temp directory), with filesystem
//!    identification on Linux. Network-FS caveats are documented in the spike
//!    doc, not measurable here.
//! 4. **cloud placeholders** — Windows `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`
//!    / `_RECALL_ON_OPEN`; macOS `UF_DATALESS` (`st_flags`); Linux has no
//!    standard — treat as normal (§4.8).
//! 5. **advisory locks** — a child process holds flock/LockFileEx; the parent
//!    `kill -9`s it; the lock must be acquirable immediately afterward
//!    (§4.12: advisory locks die with the process).
//! 6. **native identity / same-root aliases / opened targets** — two paths to
//!    the same directory (one via a symlink alias) produce one native
//!    identity; a different file produces a different identity; opening a note
//!    through a symlinked parent yields the *target's* identity (§4.16.2/
//!    §4.13, the basis for root-key contention, PathKey uniqueness, and
//!    opened-target containment checks).
//! 7. **no-clobber rename** — plain `rename(2)` clobbers an existing
//!    destination; the `rename_no_replace` contract needs
//!    `renameat2(RENAME_NOREPLACE)` on Linux (`renamex_np(RENAME_EXCL)` on
//!    macOS, `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` on Windows).
//!
//! The probe is a dev tool, not product code. Its `--self-check` mode asserts
//! the properties the production wrappers rely on; CI re-runs it on Linux
//! headless. What cannot be measured on the host OS is reported with
//! `measured: false` (or `dir_fsync_supported: false`) and an honest label —
//! never fabricated.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::spike_s3::{LatencyStats, latency_stats};

/// Timeout for the child lock holder to report ownership.
const LOCK_CHILD_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum time allowed after `kill -9` before the lock is acquirable again.
const LOCK_REACQUIRE_BUDGET: Duration = Duration::from_secs(5);
/// Self-check tolerance for immediate reacquisition after kill (§4.12).
const LOCK_IMMEDIATE_MS: u64 = 1_000;
/// Number of fsync samples per measurement.
const FSYNC_SAMPLES: usize = 5;
/// Size of the file written before each file-fsync sample.
const FSYNC_FILE_BYTES: usize = 1024 * 1024;
/// Spacing between fsync samples.
const FSYNC_SPACING: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------
// Platform primitives (raw OS APIs; the probe is dev tooling, not a safe
// production wrapper — C5 builds the safe wrappers from this evidence)
// ---------------------------------------------------------------------------

/// Try once to take an exclusive advisory lock on `file`, nonblocking.
///
/// Returns `Ok(())` when acquired; returns the OS error otherwise (typically
/// `WOULDBLOCK` on Unix, `ERROR_LOCK_VIOLATION` on Windows).
#[cfg(unix)]
pub fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Windows variant of [`try_lock_exclusive`]: `LockFileEx` with
/// `LOCKFILE_EXCLUSIVE_LOCK` over byte range [0, 1).
#[cfg(windows)]
pub fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let rc = unsafe { LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, &mut overlapped) };
    if rc != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// A native file identity: (volume, inode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct NativeIdentity {
    /// Volume identifier (Unix: `st_dev`; Windows: volume serial number).
    pub volume: u64,
    /// File identifier (Unix: `st_ino`; Windows: file index high/low).
    pub inode: u64,
}

/// Read the native identity of `path`.
#[cfg(unix)]
pub fn native_identity(path: &Path) -> Result<NativeIdentity> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).with_context(|| format!("stat failed: {}", path.display()))?;
    Ok(NativeIdentity {
        volume: md.dev(),
        inode: md.ino(),
    })
}

/// Windows variant of [`native_identity`]: `CreateFileW` +
/// `GetFileInformationByHandle`.
#[cfg(windows)]
pub fn native_identity(path: &Path) -> Result<NativeIdentity> {
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, GetFileInformationByHandle,
        FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    };
    const OPEN_EXISTING: u32 = 3;
    let wide = wide_path(path);
    let handle: HANDLE = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        anyhow::bail!("CreateFileW failed for {}", path.display());
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    unsafe { _ = close_handle(handle); }
    if ok == 0 {
        anyhow::bail!("GetFileInformationByHandle failed for {}", path.display());
    }
    Ok(NativeIdentity {
        volume: info.dwVolumeSerialNumber as u64,
        inode: ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64),
    })
}

/// Encode a path as a NUL-terminated UTF-16 buffer.
#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}

/// Close a Windows handle.
#[cfg(windows)]
unsafe fn close_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> i32 {
    windows_sys::Win32::Foundation::CloseHandle(handle)
}

/// Detect whether `path` is a cloud placeholder ("dataless" file that is not
/// downloaded locally). Notes and attachments alike (§4.8).
///
/// - Windows: `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` /
///   `FILE_ATTRIBUTE_RECALL_ON_OPEN`.
/// - macOS: `UF_DATALESS` in `st_flags`.
/// - Linux: no standard attribute — returns `false` (treat as normal).
pub fn detect_placeholder(path: &Path) -> Result<bool> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileAttributesW, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
            FILE_ATTRIBUTE_RECALL_ON_OPEN,
        };
        let attrs = unsafe { GetFileAttributesW(wide_path(path).as_ptr()) };
        if attrs == u32::MAX {
            anyhow::bail!("GetFileAttributesW failed for {}", path.display());
        }
        return Ok(attrs & (FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | FILE_ATTRIBUTE_RECALL_ON_OPEN) != 0);
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        // UF_DATALESS — <sys/stat.h>, the macOS flag marking dataless
        // (iCloud/APFS) files. libc does not expose it on all toolchains, so
        // the constant is spelled from the header.
        const UF_DATALESS: u32 = 0x8000_0000;
        let c = CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("non-UTF8 path: {}", path.display()))?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::stat(c.as_ptr(), &mut st) } != 0 {
            anyhow::bail!("stat failed for {}", path.display());
        }
        return Ok(st.st_flags & UF_DATALESS != 0);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // Linux: no standard placeholder attribute (§4.8: "treat as normal").
        let _ = path;
        Ok(false)
    }
}

/// Identify the filesystem backing `path` (Linux `statfs` magic).
#[cfg(target_os = "linux")]
pub fn fs_type_name(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return "unknown".to_string();
    };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return "unknown".to_string();
    }
    match st.f_type {
        0x0102_1994 => "tmpfs".to_string(),
        0x0000_EF53 => "ext4".to_string(),
        0x9123_683E => "btrfs".to_string(),
        0x5846_5342 => "xfs".to_string(),
        0x0000_6969 => "nfs".to_string(),
        0x0000_517B => "smb".to_string(),
        _ => "unknown".to_string(),
    }
}

/// Non-Linux Unix (macOS): report "unknown (non-Linux unix)".
#[cfg(all(unix, not(target_os = "linux")))]
pub fn fs_type_name(_path: &Path) -> String {
    "unknown (non-Linux unix)".to_string()
}

/// Windows: report "ntfs (assumed)" — statfs has no direct equivalent.
#[cfg(windows)]
pub fn fs_type_name(_path: &Path) -> String {
    "ntfs (assumed)".to_string()
}

// ---------------------------------------------------------------------------
// Scenario reports
// ---------------------------------------------------------------------------

/// Scenario 1: rename-over semantics.
#[derive(Debug, Clone, Serialize)]
pub struct RenameOverReport {
    /// Whether this host could exercise the scenario (false = pending manual
    /// run, labeled in `note`).
    pub measured: bool,
    /// Host OS.
    pub os: String,
    /// The destination kept the source's permission mode after the rename.
    pub mode_preserved: bool,
    /// The destination now holds the source's content (inode swap).
    pub content_replaced: bool,
    /// The destination kept the source's modification time (same inode).
    pub mtime_preserved: bool,
    /// Honest label when `measured` is false.
    pub note: Option<String>,
}

/// Scenario 2: symlink rename-over hazard.
#[derive(Debug, Clone, Serialize)]
pub struct SymlinkReport {
    /// Whether this host could exercise the scenario.
    pub measured: bool,
    /// Renaming a temp file directly over the link path replaced the link
    /// itself with a regular file (the D6 hazard).
    pub hazard_demonstrated: bool,
    /// Resolving the symlink first and renaming over the *target* preserved
    /// the link (the D6 requirement).
    pub resolve_first_preserves_link: bool,
    /// Honest label when `measured` is false.
    pub note: Option<String>,
}

/// One fsync measurement location.
#[derive(Debug, Clone, Serialize)]
pub struct FsyncLocation {
    /// The directory measured.
    pub path: PathBuf,
    /// Filesystem identification (Linux `statfs` magic; see [`fs_type_name`]).
    pub fs: String,
    /// File-fsync latencies (microseconds).
    pub file_fsync_us: LatencyStats,
    /// Whether directory fsync is supported at this location. When `false`,
    /// `dir_fsync_us` is empty and `dir_fsync_note` explains why. This is
    /// evidence, not an error: Windows cannot flush a directory handle with
    /// `FlushFileBuffers`, so the `Fs::fsync_dir` contract there is
    /// best-effort (§4.9 weaker durability mode).
    pub dir_fsync_supported: bool,
    /// Directory-fsync latencies (microseconds).
    pub dir_fsync_us: LatencyStats,
    /// Honest label when `dir_fsync_supported` is `false`.
    pub dir_fsync_note: Option<String>,
}

/// Scenario 3: fsync behavior.
#[derive(Debug, Clone, Serialize)]
pub struct FsyncReport {
    /// Measured locations.
    pub locations: Vec<FsyncLocation>,
}

/// Scenario 4: cloud-placeholder detection.
#[derive(Debug, Clone, Serialize)]
pub struct PlaceholderReport {
    /// The detection backend for this OS.
    pub backend: String,
    /// Whether a freshly created, fully local file reports as a placeholder
    /// (must be false on every OS).
    pub fresh_file_is_placeholder: bool,
}

/// Scenario 5: advisory lock survived process death.
#[derive(Debug, Clone, Serialize)]
pub struct LockReport {
    /// The child reported it held the lock.
    pub child_held: bool,
    /// The parent independently confirmed the lock was held before the kill.
    pub confirmed_held: bool,
    /// The lock was reacquirable after `kill -9`.
    pub reacquired: bool,
    /// Time from kill to reacquisition, in milliseconds.
    pub reacquire_ms: Option<u64>,
    /// Honest label on failure.
    pub note: Option<String>,
}

/// Scenario 6: native identity and same-root aliases.
#[derive(Debug, Clone, Serialize)]
pub struct IdentityReport {
    /// Whether this host could exercise the scenario.
    pub measured: bool,
    /// A symlink alias to the vault root has the same native identity as the
    /// root itself (§4.13: alternate paths contend on one root guard).
    pub alias_matches: bool,
    /// A different file inside the vault has a different identity.
    pub distinct_differs: bool,
    /// A note reached through a symlinked parent directory resolves to the
    /// real target's identity, not the alias's path string (§4.16.2:
    /// containment is validated on the *opened target*).
    pub opened_target_matches: bool,
    /// Honest label when `measured` is false.
    pub note: Option<String>,
}

/// One scenario's report, tagged with its scenario id.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "scenario", rename_all = "snake_case")]
pub enum ScenarioReport {
    /// Scenario 1.
    RenameOver(RenameOverReport),
    /// Scenario 2.
    Symlink(SymlinkReport),
    /// Scenario 3.
    Fsync(FsyncReport),
    /// Scenario 4.
    Placeholder(PlaceholderReport),
    /// Scenario 5.
    Lock(LockReport),
    /// Scenario 6.
    Identity(IdentityReport),
    /// Scenario 7.
    NoClobber(NoClobberReport),
}

/// A named scenario report.
#[derive(Debug, Clone, Serialize)]
pub struct NamedReport {
    /// Scenario name (`1-rename-over`, …, `6-identity`).
    pub name: String,
    /// The report.
    pub report: ScenarioReport,
}

/// A full probe run.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    /// Host OS.
    pub os: String,
    /// Host architecture.
    pub arch: String,
    /// Scenario reports, in scenario order.
    pub scenarios: Vec<NamedReport>,
}

/// Which scenarios to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScenarioSelection {
    /// Scenario 1: rename-over semantics.
    pub rename_over: bool,
    /// Scenario 2: symlink hazard.
    pub symlink: bool,
    /// Scenario 3: fsync.
    pub fsync: bool,
    /// Scenario 4: placeholders.
    pub placeholder: bool,
    /// Scenario 5: advisory lock kill.
    pub lock: bool,
    /// Scenario 6: native identity.
    pub identity: bool,
    /// Scenario 7: no-clobber rename.
    pub no_clobber: bool,
}

/// Parse a scenario list: `all` or a comma-separated subset of `1..7`.
///
/// # Errors
/// Returns an error on an unknown or empty scenario selection.
pub fn parse_selection(input: &str) -> Result<ScenarioSelection> {
    if input == "all" {
        return Ok(ScenarioSelection {
            rename_over: true,
            symlink: true,
            fsync: true,
            placeholder: true,
            lock: true,
            identity: true,
            no_clobber: true,
        });
    }
    let mut selection = ScenarioSelection::default();
    for part in input.split(',') {
        match part.trim() {
            "1" => selection.rename_over = true,
            "2" => selection.symlink = true,
            "3" => selection.fsync = true,
            "4" => selection.placeholder = true,
            "5" => selection.lock = true,
            "6" => selection.identity = true,
            "7" => selection.no_clobber = true,
            other => {
                anyhow::bail!("unknown scenario '{other}' (expected 1..7 or all)");
            }
        }
    }
    let any = selection.rename_over
        || selection.symlink
        || selection.fsync
        || selection.placeholder
        || selection.lock
        || selection.identity
        || selection.no_clobber;
    if !any {
        anyhow::bail!("empty scenario selection");
    }
    Ok(selection)
}

// ---------------------------------------------------------------------------
// Scenario runners
// ---------------------------------------------------------------------------

/// Scenario 1 runner. On Unix, exercises `rename(2)` semantics; on Windows,
/// reports the pending-manual `ReplaceFileW` vs `MoveFileExW` comparison.
pub fn run_rename_over(dir: &Path) -> Result<RenameOverReport> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::create_dir_all(dir).context("scenario dir")?;
        let src = dir.join("src.md");
        let dst = dir.join("dst.md");
        std::fs::write(&src, b"new").context("write src")?;
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640))
            .context("set src mode")?;
        std::fs::write(&dst, b"old").context("write dst")?;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600))
            .context("set dst mode")?;
        let src_md = std::fs::metadata(&src).context("stat src")?;
        let src_mtime = src_md.mtime();
        let src_mtime_nsec = src_md.mtime_nsec();
        thread::sleep(Duration::from_millis(20));
        std::fs::rename(&src, &dst).context("rename src -> dst")?;
        let dst_md = std::fs::metadata(&dst).context("stat dst")?;
        let content_replaced = std::fs::read(&dst).context("read dst")? == b"new";
        let mode_preserved = dst_md.permissions().mode() & 0o777 == 0o640;
        let mtime_preserved = dst_md.mtime() == src_mtime
            && dst_md.mtime_nsec() == src_mtime_nsec;
        Ok(RenameOverReport {
            measured: true,
            os: std::env::consts::OS.to_string(),
            mode_preserved,
            content_replaced,
            mtime_preserved,
            note: None,
        })
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(RenameOverReport {
            measured: false,
            os: std::env::consts::OS.to_string(),
            mode_preserved: false,
            content_replaced: false,
            mtime_preserved: false,
            note: Some(
                "pending manual run: ReplaceFileW vs MoveFileExW comparison on Windows",
            )
            .map(str::to_string),
        })
    }
}

/// Scenario 2 runner: the D6 symlink hazard.
pub fn run_symlink(dir: &Path) -> Result<SymlinkReport> {
    #[cfg(unix)]
    {
        std::fs::create_dir_all(dir).context("scenario dir")?;
        let target = dir.join("target.txt");
        let link = dir.join("link.txt");
        let tmp = dir.join(".tmp");
        // Phase A: rename over the link path.
        std::fs::write(&target, b"T").context("write target")?;
        std::os::unix::fs::symlink(&target, &link).context("create symlink")?;
        let link_md = std::fs::symlink_metadata(&link).context("lstat link")?;
        let was_link = link_md.file_type().is_symlink();
        std::fs::write(&tmp, b"X").context("write tmp")?;
        std::fs::rename(&tmp, &link).context("rename over link path")?;
        let now_md = std::fs::symlink_metadata(&link).context("lstat after rename")?;
        let hazard_demonstrated = was_link && !now_md.file_type().is_symlink();
        // Phase B: resolve first, rename over the target. Phase A left a
        // regular file at the link path, so clear it before relinking.
        std::fs::remove_file(&link).context("clear regular file at link path")?;
        std::os::unix::fs::symlink(&target, &link).context("recreate symlink")?;
        let resolved = std::fs::canonicalize(&link).context("canonicalize link")?;
        std::fs::write(&tmp, b"Y").context("write tmp")?;
        std::fs::rename(&tmp, &resolved).context("rename over resolved target")?;
        let final_md = std::fs::symlink_metadata(&link).context("lstat final link")?;
        let resolve_first_preserves_link =
            final_md.file_type().is_symlink() && std::fs::read(&target).context("read target")? == b"Y";
        Ok(SymlinkReport {
            measured: true,
            hazard_demonstrated,
            resolve_first_preserves_link,
            note: None,
        })
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(SymlinkReport {
            measured: false,
            hazard_demonstrated: false,
            resolve_first_preserves_link: false,
            note: Some(
                "pending manual run: Windows symlink creation requires privilege (developer \
                 mode); behavior of MoveFileExW/ReplaceFileW on reparse points to verify",
            )
            .map(str::to_string),
        })
    }
}

/// Perform one directory fsync, returning the elapsed microseconds.
///
/// Errors are returned as messages so an unsupported directory flush is
/// *recorded evidence* (e.g. Windows `FlushFileBuffers` on a directory
/// handle) rather than an aborted probe run.
#[cfg(unix)]
fn fsync_dir_once(path: &Path) -> std::result::Result<u64, String> {
    use std::os::unix::io::AsRawFd;
    let dir_file = std::fs::File::open(path).map_err(|e| format!("open dir: {e}"))?;
    let t0 = Instant::now();
    let rc = unsafe { libc::fsync(dir_file.as_raw_fd()) };
    let elapsed = u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);
    if rc == 0 {
        Ok(elapsed)
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

/// Windows variant of [`fsync_dir_once`]: `FlushFileBuffers` over a directory
/// handle opened with `FILE_FLAG_BACKUP_SEMANTICS`. Known to fail
/// (`ERROR_ACCESS_DENIED`) on real Windows — recorded as evidence for the
/// best-effort `fsync_dir` contract.
#[cfg(windows)]
fn fsync_dir_once(path: &Path) -> std::result::Result<u64, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
    };
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle: HANDLE = unsafe {
        CreateFileW(
            wide.as_ptr(),
            1, // FILE_LIST_DIRECTORY
            1 | 2 | 4, // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
            std::ptr::null(),
            3, // OPEN_EXISTING
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(format!(
            "CreateFileW (dir) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let t0 = Instant::now();
    let ok = unsafe { FlushFileBuffers(handle) };
    let elapsed = u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX);
    unsafe { CloseHandle(handle); }
    if ok == 0 {
        return Err(format!(
            "FlushFileBuffers (dir) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(elapsed)
}

/// Measure file and directory fsync latencies at one location.
fn measure_fsync_location(path: &Path) -> Result<FsyncLocation> {
    let file_path = path.join(".fsync-probe.bin");
    let mut file_samples = Vec::with_capacity(FSYNC_SAMPLES);
    for _ in 0..FSYNC_SAMPLES {
        let mut file = std::fs::File::create(&file_path).context("create fsync file")?;
        use std::io::Write as _;
        file.write_all(&vec![0x5A; FSYNC_FILE_BYTES])
            .context("write fsync file")?;
        let t0 = Instant::now();
        file.sync_all().context("fsync file")?;
        file_samples.push(u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX));
        drop(file);
        thread::sleep(FSYNC_SPACING);
    }
    let _ = std::fs::remove_file(&file_path);

    let mut dir_samples = Vec::with_capacity(FSYNC_SAMPLES);
    let mut dir_fsync_supported = true;
    let mut dir_fsync_note = None;
    for _ in 0..FSYNC_SAMPLES {
        match fsync_dir_once(path) {
            Ok(elapsed) => dir_samples.push(elapsed),
            Err(err) => {
                dir_fsync_supported = false;
                dir_fsync_note = Some(err);
                break;
            }
        }
        thread::sleep(FSYNC_SPACING);
    }

    Ok(FsyncLocation {
        path: path.to_path_buf(),
        fs: fs_type_name(path),
        file_fsync_us: latency_stats(&file_samples),
        dir_fsync_supported,
        dir_fsync_us: latency_stats(&dir_samples),
        dir_fsync_note,
    })
}

/// Scenario 3 runner.
pub fn run_fsync(extra_dir: Option<&Path>) -> Result<FsyncReport> {
    let mut locations = Vec::new();
    // The default temp location (typically tmpfs on Linux).
    locations.push(measure_fsync_location(&std::env::temp_dir())?);
    if let Some(extra) = extra_dir {
        std::fs::create_dir_all(extra).context("extra fsync dir")?;
        locations.push(measure_fsync_location(extra)?);
    }
    Ok(FsyncReport { locations })
}

/// Scenario 4 runner.
pub fn run_placeholder(dir: &Path) -> Result<PlaceholderReport> {
    std::fs::create_dir_all(dir).context("scenario dir")?;
    let file = dir.join("local.md");
    std::fs::write(&file, b"# local").context("write local file")?;
    let backend = if cfg!(windows) {
        "FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS | RECALL_ON_OPEN".to_string()
    } else if cfg!(target_os = "macos") {
        "UF_DATALESS (st_flags)".to_string()
    } else {
        "none (Linux: treat as normal, §4.8)".to_string()
    };
    let fresh_file_is_placeholder = detect_placeholder(&file)?;
    Ok(PlaceholderReport {
        backend,
        fresh_file_is_placeholder,
    })
}

/// The child-process mode: acquire the advisory lock, announce it, then sleep
/// for a long time (the parent kills us).
pub fn hold_lock_child(path: &Path) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open lock file: {}", path.display()))?;
    loop {
        match try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
    println!("LOCKED");
    std::io::stdout().flush().context("flush child stdout")?;
    thread::sleep(Duration::from_secs(120));
    Ok(())
}

/// Scenario 5 runner: spawn a child that holds the lock, kill it with
/// `kill -9` (`Child::kill`), and verify immediate reacquisition.
pub fn run_lock(dir: &Path) -> Result<LockReport> {
    std::fs::create_dir_all(dir).context("scenario dir")?;
    let lock_path = dir.join("instance.lock");
    let exe = std::env::current_exe().context("current executable")?;
    let mut child = Command::new(&exe)
        .arg("--hold-lock")
        .arg(&lock_path)
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn lock child")?;
    let stdout = child.stdout.take().context("child stdout")?;
    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let lines = std::io::BufReader::new(stdout).lines();
        for line in lines.map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let child_held = rx.recv_timeout(LOCK_CHILD_TIMEOUT).ok().as_deref() == Some("LOCKED");
    if !child_held {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(LockReport {
            child_held: false,
            confirmed_held: false,
            reacquired: false,
            reacquire_ms: None,
            note: Some("child never reported the lock (timeout or acquisition failure)"
                .to_string()),
        });
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .context("open lock file for probe")?;
    let confirmed_held = try_lock_exclusive(&file).is_err();
    let t0 = Instant::now();
    let kill_ok = child.kill().is_ok();
    let _ = child.wait();
    let mut reacquired = false;
    let mut reacquire_ms = None;
    while t0.elapsed() <= LOCK_REACQUIRE_BUDGET {
        match try_lock_exclusive(&file) {
            Ok(()) => {
                reacquired = true;
                reacquire_ms = Some(u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX));
                break;
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
    drop(reader);
    let note = if kill_ok {
        None
    } else {
        Some("child kill failed".to_string())
    };
    Ok(LockReport {
        child_held: true,
        confirmed_held,
        reacquired,
        reacquire_ms,
        note,
    })
}

/// Scenario 6 runner: same-root alias identity and opened-target resolution
/// (Unix: `stat` dev/ino; Windows: `GetFileInformationByHandle` via
/// [`native_identity`]).
pub fn run_identity(dir: &Path) -> Result<IdentityReport> {
    #[cfg(unix)]
    {
        std::fs::create_dir_all(dir).context("scenario dir")?;
        let alias = dir.join("alias");
        std::os::unix::fs::symlink(dir, &alias).context("create alias symlink")?;
        let file = dir.join("note.md");
        std::fs::write(&file, b"# note").context("write note")?;
        let root_id = native_identity(dir)?;
        let alias_id = native_identity(&alias)?;
        let file_id = native_identity(&file)?;
        let alias_matches = root_id == alias_id;
        let distinct_differs = root_id != file_id && alias_id != file_id;
        // Opened-target containment (§4.16.2): a note reached through a
        // symlinked parent must resolve to the *target's* identity — path
        // strings alias, identity does not.
        let real_dir = dir.join("real");
        std::fs::create_dir_all(&real_dir).context("create real dir")?;
        let real_note = real_dir.join("note.md");
        std::fs::write(&real_note, b"# real note").context("write real note")?;
        let alias_parent = dir.join("alias-parent");
        std::os::unix::fs::symlink(&real_dir, &alias_parent).context("symlink parent")?;
        let real_id = native_identity(&real_note)?;
        let via_alias_id = native_identity(&alias_parent.join("note.md"))?;
        let opened_target_matches = real_id == via_alias_id && real_id != file_id;
        Ok(IdentityReport {
            measured: true,
            alias_matches,
            distinct_differs,
            opened_target_matches,
            note: None,
        })
    }
    #[cfg(windows)]
    {
        let _ = dir;
        Ok(IdentityReport {
            measured: false,
            alias_matches: false,
            distinct_differs: false,
            opened_target_matches: false,
            note: Some("pending manual run: GetFileInformationByHandle volume/file-index comparison"
                .to_string()),
        })
    }
}

/// Scenario 7: no-clobber rename semantics (`rename_no_replace` contract).
#[derive(Debug, Clone, Serialize)]
pub struct NoClobberReport {
    /// Whether this host could exercise the scenario.
    pub measured: bool,
    /// Host OS.
    pub os: String,
    /// Plain `rename(2)` over an existing destination clobbered it — the
    /// reason a no-clobber primitive needs more than `rename(2)`.
    pub plain_rename_clobbers: bool,
    /// The no-clobber primitive succeeded when the destination was missing.
    pub noreplace_succeeds_when_missing: bool,
    /// The no-clobber primitive failed with `EEXIST` when the destination
    /// existed, leaving both files untouched.
    pub noreplace_fails_when_present: bool,
    /// Honest label when `measured` is false.
    pub note: Option<String>,
}

/// Linux no-clobber primitive: `renameat2(RENAME_NOREPLACE)`.
#[cfg(target_os = "linux")]
fn renameat2_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from_c = CString::new(from.as_os_str().as_bytes())?;
    let to_c = CString::new(to.as_os_str().as_bytes())?;
    const AT_FDCWD: i32 = -100;
    let rc = unsafe {
        libc::renameat2(
            AT_FDCWD,
            from_c.as_ptr(),
            AT_FDCWD,
            to_c.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Scenario 7 runner. Measured on Linux via `renameat2(RENAME_NOREPLACE)`;
/// macOS (`renamex_np(RENAME_EXCL)`) and Windows (`MoveFileExW` without
/// `MOVEFILE_REPLACE_EXISTING`) are source-verified pending manual runs.
#[cfg(target_os = "linux")]
pub fn run_no_clobber(dir: &Path) -> Result<NoClobberReport> {
    std::fs::create_dir_all(dir).context("scenario dir")?;
    let src = dir.join("src.md");
    let dst = dir.join("dst.md");

    // (a) Plain rename(2) clobbers an existing destination.
    std::fs::write(&src, b"new").context("write src")?;
    std::fs::write(&dst, b"old").context("write dst")?;
    std::fs::rename(&src, &dst).context("plain rename")?;
    let plain_rename_clobbers = std::fs::read(&dst).context("read dst")? == b"new";

    // (b) renameat2(RENAME_NOREPLACE) refuses an existing destination.
    std::fs::write(&src, b"src2").context("rewrite src")?;
    std::fs::write(&dst, b"keep").context("rewrite dst")?;
    let err = renameat2_noreplace(&src, &dst).expect_err("noreplace over existing dest must fail");
    let noreplace_fails_when_present = err.raw_os_error() == Some(libc::EEXIST)
        && std::fs::read(&dst).context("read dst")? == b"keep"
        && src.exists();

    // (c) renameat2(RENAME_NOREPLACE) succeeds into a missing destination.
    std::fs::remove_file(&dst).context("clear dst")?;
    renameat2_noreplace(&src, &dst).context("noreplace into missing dest")?;
    let noreplace_succeeds_when_missing =
        std::fs::read(&dst).context("read dst")? == b"src2" && !src.exists();

    Ok(NoClobberReport {
        measured: true,
        os: std::env::consts::OS.to_string(),
        plain_rename_clobbers,
        noreplace_succeeds_when_missing,
        noreplace_fails_when_present,
        note: None,
    })
}

/// Non-Linux variant of [`run_no_clobber`]: reports the pending manual run.
#[cfg(not(target_os = "linux"))]
pub fn run_no_clobber(_dir: &Path) -> Result<NoClobberReport> {
    let note = if cfg!(target_os = "macos") {
        "pending manual run: renamex_np(RENAME_EXCL) on macOS (plain rename(2) clobbers there too)"
    } else {
        "pending manual run: MoveFileExW without MOVEFILE_REPLACE_EXISTING on Windows"
    };
    Ok(NoClobberReport {
        measured: false,
        os: std::env::consts::OS.to_string(),
        plain_rename_clobbers: false,
        noreplace_succeeds_when_missing: false,
        noreplace_fails_when_present: false,
        note: Some(note.to_string()),
    })
}

/// Run the selected scenarios, each in a fresh subdirectory of `vault`.
pub fn run_all(
    vault: &Path,
    selection: &ScenarioSelection,
    extra_fsync_dir: Option<&Path>,
) -> Result<RunSummary> {
    let mut scenarios = Vec::new();
    if selection.rename_over {
        let report = run_rename_over(&vault.join("s1-rename-over"))?;
        scenarios.push(NamedReport {
            name: "1-rename-over".to_string(),
            report: ScenarioReport::RenameOver(report),
        });
    }
    if selection.symlink {
        let report = run_symlink(&vault.join("s2-symlink"))?;
        scenarios.push(NamedReport {
            name: "2-symlink".to_string(),
            report: ScenarioReport::Symlink(report),
        });
    }
    if selection.fsync {
        let report = run_fsync(extra_fsync_dir)?;
        scenarios.push(NamedReport {
            name: "3-fsync".to_string(),
            report: ScenarioReport::Fsync(report),
        });
    }
    if selection.placeholder {
        let report = run_placeholder(&vault.join("s4-placeholder"))?;
        scenarios.push(NamedReport {
            name: "4-placeholder".to_string(),
            report: ScenarioReport::Placeholder(report),
        });
    }
    if selection.lock {
        let report = run_lock(&vault.join("s5-lock"))?;
        scenarios.push(NamedReport {
            name: "5-lock".to_string(),
            report: ScenarioReport::Lock(report),
        });
    }
    if selection.identity {
        let report = run_identity(&vault.join("s6-identity"))?;
        scenarios.push(NamedReport {
            name: "6-identity".to_string(),
            report: ScenarioReport::Identity(report),
        });
    }
    if selection.no_clobber {
        let report = run_no_clobber(&vault.join("s7-no-clobber"))?;
        scenarios.push(NamedReport {
            name: "7-no-clobber".to_string(),
            report: ScenarioReport::NoClobber(report),
        });
    }
    Ok(RunSummary {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        scenarios,
    })
}

// ---------------------------------------------------------------------------
// Self-checks (the properties the production wrappers rely on)
// ---------------------------------------------------------------------------

/// Self-check scenario 1.
pub fn self_check_rename_over(report: &RenameOverReport) -> Result<()> {
    if !report.measured {
        return Ok(());
    }
    if report.mode_preserved && report.content_replaced && report.mtime_preserved {
        Ok(())
    } else {
        anyhow::bail!(
            "rename-over semantics violated: mode={} content={} mtime={}",
            report.mode_preserved,
            report.content_replaced,
            report.mtime_preserved
        )
    }
}

/// Self-check scenario 2.
pub fn self_check_symlink(report: &SymlinkReport) -> Result<()> {
    if !report.measured {
        return Ok(());
    }
    if report.hazard_demonstrated && report.resolve_first_preserves_link {
        Ok(())
    } else {
        anyhow::bail!(
            "symlink hazard: hazard={} resolve-first={}",
            report.hazard_demonstrated,
            report.resolve_first_preserves_link
        )
    }
}

/// Self-check scenario 3.
pub fn self_check_fsync(report: &FsyncReport) -> Result<()> {
    if report.locations.is_empty() {
        anyhow::bail!("fsync: no locations measured");
    }
    for location in &report.locations {
        if location.file_fsync_us.count != FSYNC_SAMPLES {
            anyhow::bail!(
                "fsync: {} measured {} file-fsync samples (expected {FSYNC_SAMPLES})",
                location.path.display(),
                location.file_fsync_us.count
            );
        }
        if !location.dir_fsync_supported {
            // Recorded evidence, not a failure — but it must be labeled.
            if location.dir_fsync_note.is_none() {
                anyhow::bail!(
                    "fsync: {} reports unsupported dir fsync without a reason",
                    location.path.display()
                );
            }
            continue;
        }
        if location.dir_fsync_us.count != FSYNC_SAMPLES {
            anyhow::bail!(
                "fsync: {} measured {} dir-fsync samples (expected {FSYNC_SAMPLES})",
                location.path.display(),
                location.dir_fsync_us.count
            );
        }
    }
    // On Linux, directory fsync must be supported somewhere (tmpfs and disk
    // filesystems both honor it); an unsupported dir fsync there is a
    // regression in the measurement path, not platform evidence.
    if cfg!(target_os = "linux")
        && !report.locations.iter().any(|l| l.dir_fsync_supported)
    {
        anyhow::bail!("fsync: no Linux location supported directory fsync");
    }
    Ok(())
}

/// Self-check scenario 4.
pub fn self_check_placeholder(report: &PlaceholderReport) -> Result<()> {
    if report.fresh_file_is_placeholder {
        anyhow::bail!("placeholder: a freshly created local file reported as a placeholder");
    }
    Ok(())
}

/// Self-check scenario 5.
pub fn self_check_lock(report: &LockReport) -> Result<()> {
    if report.child_held && report.confirmed_held && report.reacquired {
        if let Some(ms) = report.reacquire_ms {
            if ms <= LOCK_IMMEDIATE_MS {
                return Ok(());
            }
            anyhow::bail!("lock reacquisition after kill -9 took {ms} ms (> 1000 ms)");
        }
        anyhow::bail!("lock reacquired but timing missing");
    } else {
        anyhow::bail!(
            "lock kill semantics: child_held={} confirmed_held={} reacquired={}",
            report.child_held,
            report.confirmed_held,
            report.reacquired
        )
    }
}

/// Self-check scenario 6.
pub fn self_check_identity(report: &IdentityReport) -> Result<()> {
    if !report.measured {
        return Ok(());
    }
    if report.alias_matches && report.distinct_differs && report.opened_target_matches {
        Ok(())
    } else {
        anyhow::bail!(
            "identity: alias_matches={} distinct_differs={} opened_target_matches={}",
            report.alias_matches,
            report.distinct_differs,
            report.opened_target_matches
        )
    }
}

/// Self-check scenario 7.
pub fn self_check_no_clobber(report: &NoClobberReport) -> Result<()> {
    if !report.measured {
        return Ok(());
    }
    if report.plain_rename_clobbers
        && report.noreplace_succeeds_when_missing
        && report.noreplace_fails_when_present
    {
        Ok(())
    } else {
        anyhow::bail!(
            "no-clobber: plain_clobbers={} succeeds_when_missing={} fails_when_present={}",
            report.plain_rename_clobbers,
            report.noreplace_succeeds_when_missing,
            report.noreplace_fails_when_present
        )
    }
}

/// Run the self-checks for every scenario in the summary. An empty list means
/// every property held.
#[must_use]
pub fn self_check(summary: &RunSummary) -> Vec<String> {
    let mut failures = Vec::new();
    for named in &summary.scenarios {
        let result = match &named.report {
            ScenarioReport::RenameOver(r) => self_check_rename_over(r),
            ScenarioReport::Symlink(r) => self_check_symlink(r),
            ScenarioReport::Fsync(r) => self_check_fsync(r),
            ScenarioReport::Placeholder(r) => self_check_placeholder(r),
            ScenarioReport::Lock(r) => self_check_lock(r),
            ScenarioReport::Identity(r) => self_check_identity(r),
            ScenarioReport::NoClobber(r) => self_check_no_clobber(r),
        };
        if let Err(failure) = result {
            failures.push(format!("scenario {}: {failure}", named.name));
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render a compact human-readable summary (bin stdout).
#[must_use]
pub fn render_summary(summary: &RunSummary) -> String {
    let mut out = format!(
        "s3_fs_probe: os={} arch={}\n",
        summary.os, summary.arch
    );
    for named in &summary.scenarios {
        out.push_str(&format!("scenario {}: ", named.name));
        match &named.report {
            ScenarioReport::RenameOver(r) => out.push_str(&format!(
                "measured={} mode={} content={} mtime={} {}",
                r.measured,
                r.mode_preserved,
                r.content_replaced,
                r.mtime_preserved,
                r.note.as_deref().unwrap_or("")
            )),
            ScenarioReport::Symlink(r) => out.push_str(&format!(
                "measured={} hazard={} resolve-first={} {}",
                r.measured,
                r.hazard_demonstrated,
                r.resolve_first_preserves_link,
                r.note.as_deref().unwrap_or("")
            )),
            ScenarioReport::Fsync(r) => {
                let locs: Vec<String> = r
                    .locations
                    .iter()
                    .map(|l| {
                        let dir = if l.dir_fsync_supported {
                            format!("{}us p95={}", l.dir_fsync_us.median, l.dir_fsync_us.p95)
                        } else {
                            format!("unsupported ({})", l.dir_fsync_note.as_deref().unwrap_or("?"))
                        };
                        format!(
                            "{}[{}] file {}us p95={} dir {}",
                            l.path.display(),
                            l.fs,
                            l.file_fsync_us.median,
                            l.file_fsync_us.p95,
                            dir
                        )
                    })
                    .collect();
                out.push_str(&locs.join(" | "));
            }
            ScenarioReport::Placeholder(r) => out.push_str(&format!(
                "backend={} fresh_file_is_placeholder={}",
                r.backend, r.fresh_file_is_placeholder
            )),
            ScenarioReport::Lock(r) => out.push_str(&format!(
                "child_held={} confirmed_held={} reacquired={} reacquire_ms={:?} {}",
                r.child_held,
                r.confirmed_held,
                r.reacquired,
                r.reacquire_ms,
                r.note.as_deref().unwrap_or("")
            )),
            ScenarioReport::Identity(r) => out.push_str(&format!(
                "measured={} alias_matches={} distinct_differs={} opened_target_matches={} {}",
                r.measured,
                r.alias_matches,
                r.distinct_differs,
                r.opened_target_matches,
                r.note.as_deref().unwrap_or("")
            )),
            ScenarioReport::NoClobber(r) => out.push_str(&format!(
                "measured={} plain_rename_clobbers={} noreplace_succeeds={} noreplace_fails={} {}",
                r.measured,
                r.plain_rename_clobbers,
                r.noreplace_succeeds_when_missing,
                r.noreplace_fails_when_present,
                r.note.as_deref().unwrap_or("")
            )),
        }
        out.push('\n');
    }
    out
}

impl RunSummary {
    /// Render the measured matrix as markdown tables (for the spike doc).
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = format!("Measured on {} ({}).\n\n", self.os, self.arch);
        for named in &self.scenarios {
            match &named.report {
                ScenarioReport::RenameOver(r) => {
                    out.push_str("### Scenario 1 — rename-over semantics\n\n");
                    out.push_str("| metric | value |\n| --- | --- |\n");
                    out.push_str(&format!("| measured | {} |\n", r.measured));
                    out.push_str(&format!("| mode preserved | {} |\n", r.mode_preserved));
                    out.push_str(&format!("| content replaced (inode swap) | {} |\n", r.content_replaced));
                    out.push_str(&format!("| mtime preserved | {} |\n", r.mtime_preserved));
                    if let Some(note) = &r.note {
                        out.push_str(&format!("| note | {note} |\n"));
                    }
                    out.push('\n');
                }
                ScenarioReport::Symlink(r) => {
                    out.push_str("### Scenario 2 — symlink rename-over (D6 hazard)\n\n");
                    out.push_str("| metric | value |\n| --- | --- |\n");
                    out.push_str(&format!("| measured | {} |\n", r.measured));
                    out.push_str(&format!(
                        "| rename over link path replaces the link | {} |\n",
                        r.hazard_demonstrated
                    ));
                    out.push_str(&format!(
                        "| resolve-first preserves the link | {} |\n",
                        r.resolve_first_preserves_link
                    ));
                    if let Some(note) = &r.note {
                        out.push_str(&format!("| note | {note} |\n"));
                    }
                    out.push('\n');
                }
                ScenarioReport::Fsync(r) => {
                    out.push_str("### Scenario 3 — fsync behavior\n\n");
                    out.push_str("| location | fs | file fsync median/p95 (µs) | dir fsync median/p95 (µs) |\n");
                    out.push_str("| --- | --- | --- | --- |\n");
                    for l in &r.locations {
                        let dir = if l.dir_fsync_supported {
                            format!("{} / {}", l.dir_fsync_us.median, l.dir_fsync_us.p95)
                        } else {
                            format!(
                                "unsupported — {}",
                                l.dir_fsync_note.as_deref().unwrap_or("unlabeled")
                            )
                        };
                        out.push_str(&format!(
                            "| {} | {} | {} / {} | {} |\n",
                            l.path.display(),
                            l.fs,
                            l.file_fsync_us.median,
                            l.file_fsync_us.p95,
                            dir
                        ));
                    }
                    out.push('\n');
                }
                ScenarioReport::Placeholder(r) => {
                    out.push_str("### Scenario 4 — cloud placeholders\n\n");
                    out.push_str("| metric | value |\n| --- | --- |\n");
                    out.push_str(&format!("| detection backend | {} |\n", r.backend));
                    out.push_str(&format!(
                        "| fresh local file reports as placeholder | {} |\n",
                        r.fresh_file_is_placeholder
                    ));
                    out.push('\n');
                }
                ScenarioReport::Lock(r) => {
                    out.push_str("### Scenario 5 — advisory lock after kill -9 (§4.12)\n\n");
                    out.push_str("| metric | value |\n| --- | --- |\n");
                    out.push_str(&format!("| child held lock | {} |\n", r.child_held));
                    out.push_str(&format!("| parent confirmed held | {} |\n", r.confirmed_held));
                    out.push_str(&format!("| reacquired after kill | {} |\n", r.reacquired));
                    out.push_str(&format!(
                        "| reacquire time (ms) | {:?} |\n",
                        r.reacquire_ms
                    ));
                    if let Some(note) = &r.note {
                        out.push_str(&format!("| note | {note} |\n"));
                    }
                    out.push('\n');
                }
                ScenarioReport::Identity(r) => {
                    out.push_str("### Scenario 6 — native identity / same-root aliases / opened targets (§4.16.2/§4.13)\n\n");
                    out.push_str("| metric | value |\n| --- | --- |\n");
                    out.push_str(&format!("| measured | {} |\n", r.measured));
                    out.push_str(&format!("| alias has same identity as root | {} |\n", r.alias_matches));
                    out.push_str(&format!("| distinct file differs | {} |\n", r.distinct_differs));
                    out.push_str(&format!(
                        "| note via symlinked parent resolves to target identity | {} |\n",
                        r.opened_target_matches
                    ));
                    if let Some(note) = &r.note {
                        out.push_str(&format!("| note | {note} |\n"));
                    }
                    out.push('\n');
                }
                ScenarioReport::NoClobber(r) => {
                    out.push_str("### Scenario 7 — no-clobber rename (`rename_no_replace` contract)\n\n");
                    out.push_str("| metric | value |\n| --- | --- |\n");
                    out.push_str(&format!("| measured | {} |\n", r.measured));
                    out.push_str(&format!("| plain `rename(2)` clobbers existing destination | {} |\n", r.plain_rename_clobbers));
                    out.push_str(&format!("| no-clobber primitive succeeds into missing destination | {} |\n", r.noreplace_succeeds_when_missing));
                    out.push_str(&format!("| no-clobber primitive fails `EEXIST` over existing destination | {} |\n", r.noreplace_fails_when_present));
                    if let Some(note) = &r.note {
                        out.push_str(&format!("| note | {note} |\n"));
                    }
                    out.push('\n');
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spike_s3::TempVault;

    #[test]
    fn parse_selection_all_and_subsets() {
        let all = parse_selection("all").expect("all parses");
        assert!(all.rename_over && all.symlink && all.fsync && all.placeholder && all.lock && all.identity && all.no_clobber);
        let subset = parse_selection("1,3,5,7").expect("subset parses");
        assert!(subset.rename_over);
        assert!(!subset.symlink);
        assert!(subset.fsync);
        assert!(!subset.placeholder);
        assert!(subset.lock);
        assert!(!subset.identity);
        assert!(subset.no_clobber);
        assert!(parse_selection("8").is_err());
        assert!(parse_selection("7").is_ok());
        assert!(parse_selection("").is_err());
        assert!(parse_selection("1,x").is_err());
    }

    #[test]
    fn self_check_lock_logic() {
        let ok = LockReport {
            child_held: true,
            confirmed_held: true,
            reacquired: true,
            reacquire_ms: Some(3),
            note: None,
        };
        assert!(self_check_lock(&ok).is_ok());

        let slow = LockReport {
            child_held: true,
            confirmed_held: true,
            reacquired: true,
            reacquire_ms: Some(2_000),
            note: None,
        };
        assert!(self_check_lock(&slow).is_err());

        let not_held = LockReport {
            child_held: false,
            confirmed_held: false,
            reacquired: false,
            reacquire_ms: None,
            note: Some("child never reported".to_string()),
        };
        assert!(self_check_lock(&not_held).is_err());
    }

    #[test]
    fn self_check_placeholder_logic() {
        let ok = PlaceholderReport {
            backend: "none".to_string(),
            fresh_file_is_placeholder: false,
        };
        assert!(self_check_placeholder(&ok).is_ok());
        let bad = PlaceholderReport {
            backend: "none".to_string(),
            fresh_file_is_placeholder: true,
        };
        assert!(self_check_placeholder(&bad).is_err());
    }

    #[test]
    fn self_check_no_clobber_logic() {
        let ok = NoClobberReport {
            measured: true,
            os: "linux".to_string(),
            plain_rename_clobbers: true,
            noreplace_succeeds_when_missing: true,
            noreplace_fails_when_present: true,
            note: None,
        };
        assert!(self_check_no_clobber(&ok).is_ok());
        let no_clobber = NoClobberReport {
            measured: true,
            os: "linux".to_string(),
            plain_rename_clobbers: false,
            noreplace_succeeds_when_missing: true,
            noreplace_fails_when_present: true,
            note: None,
        };
        assert!(self_check_no_clobber(&no_clobber).is_err());
        let unmeasured = NoClobberReport {
            measured: false,
            os: "windows".to_string(),
            plain_rename_clobbers: false,
            noreplace_succeeds_when_missing: false,
            noreplace_fails_when_present: false,
            note: Some("pending manual".to_string()),
        };
        assert!(self_check_no_clobber(&unmeasured).is_ok());
    }

    #[test]
    fn self_check_fsync_requires_labels_for_unsupported_dir() {
        let unlabeled = FsyncReport {
            locations: vec![FsyncLocation {
                path: PathBuf::from("/x"),
                fs: "ntfs (assumed)".to_string(),
                file_fsync_us: latency_stats(&[1, 2, 3, 4, 5]),
                dir_fsync_supported: false,
                dir_fsync_us: latency_stats(&[]),
                dir_fsync_note: None,
            }],
        };
        assert!(self_check_fsync(&unlabeled).is_err());
        let labeled = FsyncReport {
            locations: vec![FsyncLocation {
                path: PathBuf::from("/x"),
                fs: "ntfs (assumed)".to_string(),
                file_fsync_us: latency_stats(&[1, 2, 3, 4, 5]),
                dir_fsync_supported: false,
                dir_fsync_us: latency_stats(&[]),
                dir_fsync_note: Some("FlushFileBuffers (dir) failed: Access denied".to_string()),
            }],
        };
        if cfg!(target_os = "linux") {
            // On Linux the harness requires at least one dir-capable location.
            assert!(self_check_fsync(&labeled).is_err());
        } else {
            assert!(self_check_fsync(&labeled).is_ok());
        }
    }

    #[test]
    fn self_check_skips_unmeasured_scenarios() {
        let unmeasured = RenameOverReport {
            measured: false,
            os: "windows".to_string(),
            mode_preserved: false,
            content_replaced: false,
            mtime_preserved: false,
            note: Some("pending manual".to_string()),
        };
        assert!(self_check_rename_over(&unmeasured).is_ok());
        let unmeasured_symlink = SymlinkReport {
            measured: false,
            hazard_demonstrated: false,
            resolve_first_preserves_link: false,
            note: Some("pending manual".to_string()),
        };
        assert!(self_check_symlink(&unmeasured_symlink).is_ok());
        let unmeasured_identity = IdentityReport {
            measured: false,
            alias_matches: false,
            distinct_differs: false,
            opened_target_matches: false,
            note: Some("pending manual".to_string()),
        };
        assert!(self_check_identity(&unmeasured_identity).is_ok());
    }

    #[test]
    fn summary_self_check_collects_failures() {
        let summary = RunSummary {
            os: "test".to_string(),
            arch: "test".to_string(),
            scenarios: vec![NamedReport {
                name: "5-lock".to_string(),
                report: ScenarioReport::Lock(LockReport {
                    child_held: false,
                    confirmed_held: false,
                    reacquired: false,
                    reacquire_ms: None,
                    note: Some("x".to_string()),
                }),
            }],
        };
        let failures = self_check(&summary);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("5-lock"));
    }

    // --- Live tests ---
    //
    // The measurement scenarios return `measured: false` on platforms whose
    // manual runs are still pending (Windows, per the recorded deferral), so
    // the tests that assert a measurement ran are gated to Unix. The
    // self-check still runs everywhere the probe runs.

    #[cfg(unix)]
    #[test]
    fn live_rename_over_semantics() {
        let vault = TempVault::new("live-rename-over");
        let report = run_rename_over(vault.path()).expect("scenario 1 runs");
        assert!(report.measured);
        self_check_rename_over(&report).expect("rename(2) semantics hold");
    }

    #[cfg(unix)]
    #[test]
    fn live_symlink_hazard_and_resolve_first() {
        let vault = TempVault::new("live-symlink");
        let report = run_symlink(vault.path()).expect("scenario 2 runs");
        assert!(report.measured);
        self_check_symlink(&report).expect("D6 hazard and resolve-first hold");
    }

    #[test]
    fn live_placeholder_false_for_fresh_file() {
        let vault = TempVault::new("live-placeholder");
        let report = run_placeholder(vault.path()).expect("scenario 4 runs");
        self_check_placeholder(&report).expect("fresh local file is never a placeholder");
    }

    #[cfg(unix)]
    #[test]
    fn live_identity_alias_and_opened_target() {
        let vault = TempVault::new("live-identity");
        let report = run_identity(vault.path()).expect("scenario 6 runs");
        assert!(report.measured);
        self_check_identity(&report).expect("alias and opened-target identity semantics hold");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_no_clobber_rename() {
        let vault = TempVault::new("live-no-clobber");
        let report = run_no_clobber(vault.path()).expect("scenario 7 runs");
        assert!(report.measured);
        self_check_no_clobber(&report).expect("rename_no_replace contract primitives hold");
    }

    #[test]
    fn live_fsync_measures_both_dirs() {
        let report = run_fsync(None).expect("scenario 3 runs");
        self_check_fsync(&report).expect("fsync measured and labeled");
        assert!(!report.locations.is_empty());
        for location in &report.locations {
            assert!(location.file_fsync_us.count >= FSYNC_SAMPLES);
            if location.dir_fsync_supported {
                assert!(location.dir_fsync_us.count >= FSYNC_SAMPLES);
            } else {
                assert!(location.dir_fsync_note.is_some());
            }
        }
    }

    // Scenario 5 (advisory lock after `kill -9`) is proven through the probe
    // binary, not `cargo test`: the lock-holder child must be the probe
    // binary's `--hold-lock` mode, and under `cargo test` the current
    // executable is the libtest harness (which rejects unknown flags). Run it
    // via `just spike-s3-fs --self-check` — the CI job does exactly that.
}
