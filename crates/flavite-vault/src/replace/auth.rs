//! Pure authenticated-record helpers (RFC-0006 §6.9). The device-local key
//! store lives OUTSIDE this module (`crate::operation_key`) because the
//! private OS data directory is not part of the vault and must not be
//! touched by replacement-logic code paths.

pub use crate::operation_key::{KeyStoreError, OperationKey, load_or_create_key};
