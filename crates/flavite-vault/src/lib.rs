#![forbid(unsafe_code)]

//! Tier 1 — durability-critical. Headless `Session` owning `BufferManager`
//! and pending operations; `VaultActor` (watcher, write tokens, journal,
//! history); `IndexActor` (sole SQLite + tantivy writer, read pool);
//! `ParsePool` (§4.1, §4.2, §4.5, D3, D6, D9).
