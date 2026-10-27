#![forbid(unsafe_code)]

//! Tier 1 — durability-critical. `EditorModel` over ropey text with
//! multi-selection, undo tree with time-based coalescing, per-line EOL side
//! table, and the D7 buffer state machine. Zero UI dependencies (§4.1, D14,
//! §4.16.1).
