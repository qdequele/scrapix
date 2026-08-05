//! Snapshot test pinning the OpenAPI spec to `contracts/openapi.json`.
//!
//! The committed snapshot is the frozen API contract for the Rails SaaS
//! migration (SCR-85): any change to routes or schemas fails this test so
//! contract drift is always an explicit, reviewed decision.
//!
//! To update the snapshot after an intentional API change:
//!
//! ```bash
//! UPDATE_OPENAPI_SNAPSHOT=1 cargo test -p scrapix-api --test openapi_snapshot
//! ```

use std::path::PathBuf;

use utoipa::OpenApi;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/openapi.json")
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
        "OpenAPI spec drifted from contracts/openapi.json. If the change is \
         intentional, regenerate with UPDATE_OPENAPI_SNAPSHOT=1 cargo test \
         -p scrapix-api --test openapi_snapshot and review the diff."
    );
}
