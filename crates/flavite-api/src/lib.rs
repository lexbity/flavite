#![forbid(unsafe_code)]

//! Tier 2. `Lattice` facade owning the `Session` and command registry;
//! typed commands/results, revisioned read views, versioned event bus,
//! `Query` AST, capability declarations. The same surface a future plugin
//! host would expose (D16, §4.5).
