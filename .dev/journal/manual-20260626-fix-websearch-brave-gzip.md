# Manual edit: fix-websearch-brave-gzip

**Date**: 2026-06-26
**Goal**: Fix WebSearch Brave provider "response parse failed: expected value at line 1 column 1"

**Root cause**: `search_brave()` sent `Accept-Encoding: gzip` header, but reqwest is built with `default-features = false` and **without** the `gzip` feature. Brave's server sent gzip-compressed data which reqwest never decompressed, so `serde_json::from_str()` received binary garbage.

**Fix**: Removed the `Accept-Encoding: gzip` header from the Brave search request. Without this explicit header the server sends uncompressed JSON, which parses correctly.

**Files touched**: `src/tools/web_search.rs`
- Removed `.header("Accept-Encoding", "gzip")` from `search_brave`
- Reformatted via `cargo fmt --all` (5 serde_json parsing blocks had minor wrapping issues)

**Tests added**: none (existing 12 tests cover the mock path)
**Notes**: Same risk applies to all providers — none have explicit gzip headers, so they're unaffected. If gzip compression is desired in the future, add `gzip` to reqwest's feature list in `Cargo.toml` rather than setting the header manually.
