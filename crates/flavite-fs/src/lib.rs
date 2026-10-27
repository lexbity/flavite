#![forbid(unsafe_code)]

//! Tier 1 — durability-critical. `Fs` trait, `atomic_write`, watcher wrapper,
//! path safety; `FaultFs` behind the `fault` cargo feature (§4.1, §10.6).
//!
//! # Crash models (§10.6.1)
//!
//! [`MemFs`] has separate volatile and durable state for both file bytes and
//! directory entries. [`FaultFs`](fault::FaultFs) (behind the `fault` feature)
//! wraps any [`Fs`] with scriptable fault injection at modeled operation
//! boundaries, supporting both **ProcessCrash** (retain volatile state) and
//! **PowerLoss** (revert to durable state) models.

pub mod fs;
pub mod mem;

#[cfg(feature = "fault")]
pub mod fault;

#[cfg(test)]
mod proptests;

pub use fs::{DirEntry, Fs, FsError, Metadata, Result, hash_file};
pub use mem::MemFs;

#[cfg(feature = "fault")]
pub use fault::{
    CrashMode, FaultBoundary, FaultContext, FaultFs, FaultKind, FaultPoint, FaultScript,
    PlannedFault, TraceEntry, TraceOutcome,
};
