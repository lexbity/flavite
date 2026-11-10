# Pinned cryoglyph lru patch

This directory contains a narrow local patch to the `cryoglyph` 0.1.0 glyph
renderer used by `iced_wgpu` 0.14.0. The crate is MIT OR Apache-2.0 OR Zlib,
as recorded in its Cargo manifest. Copyright remains with the cryoglyph
contributors (a fork of glyphon by the iced project). The patched source adds
no Flavite-specific behavior.

| Crate | Version | SHA-256 of registry archive |
|---|---:|---|
| `cryoglyph` | 0.1.0 | `08bc795bdbccdbd461736fb163930a009da6597b226d6f6fce33e7e8eb6ec519` |

The original archive came from crates.io and can be re-fetched by the pinned
version above (`cryoglyph-0.1.0.crate`).

## Local delta

- `Cargo.toml`: the `lru` dependency is bumped from `"0.16"` to `"0.18"`.
  RUSTSEC-2026-0253 reports a use-after-free in `LruCache::pop()` for
  `lru < 0.18.2` (missing panic safety when a stored key's `Drop` panics);
  cryoglyph 0.1.0 pins `lru = "0.16"` and has no upstream release on
  `lru 0.18`, so the dependency bump is applied here. Every `LruCache` API
  cryoglyph uses (`unbounded_with_hasher`, `put`, `get`, `peek_lru`,
  `pop_lru`) is unchanged between lru 0.16 and 0.18, so the port is the
  version bump alone. With the patch, `lru` 0.16.x leaves the dependency
  graph entirely (tantivy already used the patched `lru` 0.18.5).

Upstream references: <https://github.com/iced-rs/cryoglyph>,
<https://github.com/jeromefroe/lru-rs> (fix in lru-rs#238),
<https://rustsec.org/advisories/RUSTSEC-2026-0253>.
