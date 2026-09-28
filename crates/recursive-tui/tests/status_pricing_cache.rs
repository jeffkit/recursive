//! Issue #41 regression: TUI status-bar pricing must not touch the provider
//! catalog (disk IO + TOML/JSON parsing) on every render frame.
//!
//! Before the fix, `ui::status::build_line` -> `app::estimate_cost` ->
//! `llm::pricing_for` -> `providers::all_presets_effective` re-read
//! `providers.d/*.toml` and `providers_cache.json` per frame. `App` now caches
//! `(model_name, Option<ModelPricing>)` and counts catalog resolutions via
//! `pricing_lookup_count()`; the render path reads the cache only.

use recursive_tui::app::App;
use recursive_tui::ui::status;

#[test]
fn build_line_resolves_pricing_once_per_model_not_per_frame() {
    let home = tempfile::tempdir().expect("tempdir");
    // Pin RECURSIVE_HOME so pricing_for reads the bundled catalog
    // deterministically, isolated from parallel env-mutating tests.
    let _pin = recursive::test_util::PinnedRecursiveHome::new(home.path());

    let app = App::default();
    app.reset_pricing_lookup_count();

    // Three render frames with the same model_name — e.g. an idle TUI
    // repainting at 60fps. Pricing must resolve at most once.
    let _ = status::build_line(&app);
    let _ = status::build_line(&app);
    let _ = status::build_line(&app);

    assert_eq!(
        app.pricing_lookup_count(),
        1,
        "pricing must be resolved once per model_name, not once per frame"
    );
}

#[test]
fn pricing_cache_invalidates_on_model_change() {
    let home = tempfile::tempdir().expect("tempdir");
    let _pin = recursive::test_util::PinnedRecursiveHome::new(home.path());

    let mut app = App::default();
    app.reset_pricing_lookup_count();

    let _ = status::build_line(&app);
    let _ = status::build_line(&app);
    app.model_name = "gpt-4o-mini".to_string();
    let _ = status::build_line(&app);

    assert_eq!(
        app.pricing_lookup_count(),
        2,
        "changing model_name must re-resolve pricing exactly once"
    );
}
