//! The engine must not write Rails-owned tables (spec 2a). Reads for auth and
//! the credit pre-check are allowed; writes go through lab events.
//!
//! Scan scope: every `.rs` file under `bins/` and `crates/` (this file
//! excluded), skipping `target/` and `migrations/` directories. SQL `.sql`
//! fixtures are not scanned.
//!
//! Detection runs over the whole file text, not line by line, so a statement
//! split across lines (`"UPDATE \` + newline + `accounts SET ...`) is caught.
//! Rust string line-continuations are removed first, then whitespace between
//! SQL tokens may span newlines. Schema-qualified (`public.accounts`),
//! quoted (`"accounts"`) and `ONLY` forms are covered, as are `MERGE INTO`
//! and `TRUNCATE`.

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

/// Removes Rust string line-continuations (`\` + newline + leading
/// whitespace), replacing each with a single space. Returns the normalized
/// text plus, for every byte of it, the 1-based line of the original text.
fn normalize(text: &str) -> (String, Vec<usize>) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut lines = Vec::with_capacity(text.len());
    let mut line = 1usize;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'\n') {
            let start_line = line;
            i += 2;
            line += 1;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n') {
                if bytes[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            out.push(' ');
            lines.push(start_line);
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        if ch == '\n' {
            line += 1;
        }
        let n = ch.len_utf8();
        out.push(ch);
        for _ in 0..n {
            // Newline itself belongs to the line it ends.
            lines.push(if ch == '\n' { line - 1 } else { line });
        }
        i += n;
    }
    (out, lines)
}

/// Returns `(line, matched text)` for every write to a Rails-owned table.
fn find_rails_writes(text: &str) -> Vec<(usize, String)> {
    let pattern = regex::Regex::new(&format!(
        r#"(?is)\b(INSERT\s+INTO|UPDATE|DELETE\s+FROM|MERGE\s+INTO|TRUNCATE(\s+TABLE)?)\s+(ONLY\s+)?("?public"?\.)?"?({})"?\b"#,
        RAILS_TABLES.join("|")
    ))
    .unwrap();
    let (norm, lines) = normalize(text);
    pattern
        .find_iter(&norm)
        .map(|m| {
            (
                lines[m.start()],
                m.as_str().split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .collect()
}

#[test]
fn engine_never_writes_rails_tables() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("bins"), &mut files);
    rust_files(&root.join("crates"), &mut files);
    assert!(!files.is_empty(), "scan found no .rs files");
    let mut hits = Vec::new();
    for f in files {
        if f.ends_with("lab_boundary.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        for (line, m) in find_rails_writes(&text) {
            hits.push(format!("{}:{}: {}", f.display(), line, m));
        }
    }
    assert!(
        hits.is_empty(),
        "engine writes Rails-owned tables:\n{}",
        hits.join("\n")
    );
}

#[cfg(test)]
mod detector {
    use super::find_rails_writes;

    fn flagged(s: &str) -> bool {
        !find_rails_writes(s).is_empty()
    }

    #[test]
    fn flags_writes() {
        assert!(flagged(
            r#"sqlx::query("UPDATE accounts SET credits_balance = 1")"#
        ));
        assert!(flagged(
            "sqlx::query(\"UPDATE \\\n            accounts SET x = 1\")"
        ));
        assert!(flagged("let q = \"UPDATE\n  accounts\n SET x = 1\";"));
        assert!(flagged("UPDATE public.accounts SET x = 1"));
        assert!(flagged(r#"DELETE FROM "oauth_tokens" WHERE 1=1"#));
        assert!(flagged("INSERT INTO ONLY transactions (a) VALUES (1)"));
        assert!(flagged("TRUNCATE scheduled_emails"));
        assert!(flagged("MERGE INTO api_keys USING x"));
    }

    #[test]
    fn reports_the_line_of_a_continued_statement() {
        let text = "a\nb\nquery(\"UPDATE \\\n    accounts SET x = 1\")";
        assert_eq!(find_rails_writes(text)[0].0, 3);
    }

    #[test]
    fn ignores_other_tables_and_reads() {
        assert!(!flagged("UPDATE lab_events SET attempts = 1"));
        assert!(!flagged("INSERT INTO jobs (id) VALUES (1)"));
        assert!(!flagged("SELECT * FROM accounts"));
        assert!(!flagged("UPDATE account_members_view SET x = 1"));
        assert!(!flagged("UPDATE accounts_archive SET x = 1"));
    }
}
