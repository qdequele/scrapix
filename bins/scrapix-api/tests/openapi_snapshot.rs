//! Snapshot test pinning the engine's OpenAPI spec to
//! `contracts/openapi.engine.json`.
//!
//! Since the SaaS split (SCR-85 phase 9) there are two specs:
//!
//! - `contracts/openapi.json` — the **frozen full-platform public spec**
//!   (engine + SaaS routes). It is the contract the Rails app implements and
//!   the source the MCP server generates its tools from. The engine no
//!   longer serves most of those routes, so it is *not* regenerated from
//!   this crate — treat it as hand-frozen.
//! - `contracts/openapi.engine.json` — the engine-only spec served at
//!   `/openapi.json`, pinned here so route/schema drift is an explicit,
//!   reviewed decision.
//!
//! To update the engine snapshot after an intentional API change:
//!
//! ```bash
//! UPDATE_OPENAPI_SNAPSHOT=1 cargo test -p scrapix-api --test openapi_snapshot
//! ```

use std::path::PathBuf;

use utoipa::OpenApi;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/openapi.engine.json")
}

#[test]
fn openapi_snapshot_is_up_to_date() {
    let spec = scrapix_api::openapi::ScrapixApi::openapi()
        .to_pretty_json()
        .expect("OpenAPI spec serializes to JSON");
    // Trailing newline so the committed file is POSIX-friendly.
    let spec = format!("{spec}\n");

    let path = snapshot_path();

    if std::env::var("UPDATE_OPENAPI_SNAPSHOT").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("create contracts dir");
        std::fs::write(&path, &spec).expect("write OpenAPI snapshot");
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing OpenAPI snapshot at {}; generate it with \
             UPDATE_OPENAPI_SNAPSHOT=1 cargo test -p scrapix-api --test openapi_snapshot",
            path.display()
        )
    });

    assert_eq!(
        committed, spec,
        "OpenAPI spec drifted from contracts/openapi.engine.json. If the \
         change is intentional, regenerate with UPDATE_OPENAPI_SNAPSHOT=1 \
         cargo test -p scrapix-api --test openapi_snapshot and review the diff."
    );
}
