//! The engine must not write Rails-owned tables (spec 2a). Reads for auth and
//! the credit pre-check are allowed; writes go through lab events.

use std::path::{Path, PathBuf};

const RAILS_TABLES: &[&str] = &[
    "accounts",
    "transactions",
    "crawl_configs",
    "scheduled_emails",
    "oauth_tokens",
    "oauth_authorization_codes",
    "api_keys",
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().unwrap().to_string_lossy();
            if name == "target" || name == "migrations" {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn engine_never_writes_rails_tables() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("bins"), &mut files);
    rust_files(&root.join("crates"), &mut files);
    let pattern = regex::Regex::new(&format!(
        r"(?i)\b(INSERT\s+INTO|UPDATE|DELETE\s+FROM)\s+({})\b",
        RAILS_TABLES.join("|")
    ))
    .unwrap();
    let mut hits = Vec::new();
    for f in files {
        if f.ends_with("lab_boundary.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        for (i, line) in text.lines().enumerate() {
            if pattern.is_match(line) {
                hits.push(format!("{}:{}: {}", f.display(), i + 1, line.trim()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "engine writes Rails-owned tables:\n{}",
        hits.join("\n")
    );
}
