//! The unified `scrapix` binary must honor the same global CLI options as
//! the standalone CLI: `SCRAPIX_API_URL` / `SCRAPIX_API_KEY` (env or
//! `--api-url` / `--api-key`) and `--json`.
//!
//! Each test points the binary at a one-shot HTTP server on 127.0.0.1 and
//! checks the request it received. `HOME` / `XDG_CONFIG_HOME` point at an
//! empty temp dir so a developer's `scrapix/config.toml` can't leak into
//! the result.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const HEALTH_BODY: &str = r#"{"status":"ok","version":"test","kafka_connected":false}"#;

/// Serves one request with `HEALTH_BODY`, returning the base URL and a
/// receiver for the lowercased request head (request line + headers).
fn one_shot_server() -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
            head.push_str(&line.to_ascii_lowercase());
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            HEALTH_BODY.len(),
            HEALTH_BODY
        );
        stream.write_all(response.as_bytes()).unwrap();
        tx.send(head).unwrap();
    });
    (url, rx)
}

fn scrapix(home: &tempfile::TempDir) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_scrapix"));
    cmd.env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env_remove("SCRAPIX_API_URL")
        .env_remove("SCRAPIX_API_KEY");
    cmd
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "scrapix exited with {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn received(rx: &mpsc::Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(5))
        .expect("scrapix never reached the API URL it was given")
}

#[test]
fn env_api_url_and_key_are_used() {
    let home = tempfile::tempdir().unwrap();
    let (url, rx) = one_shot_server();

    let output = scrapix(&home)
        .arg("health")
        .env("SCRAPIX_API_URL", &url)
        .env("SCRAPIX_API_KEY", "env-key-123")
        .output()
        .unwrap();

    assert_success(&output);
    let head = received(&rx);
    assert!(head.starts_with("get /health "), "{head}");
    assert!(head.contains("x-api-key: env-key-123\r\n"), "{head}");
}

#[test]
fn flags_after_the_subcommand_override_env() {
    let home = tempfile::tempdir().unwrap();
    let (url, rx) = one_shot_server();

    let output = scrapix(&home)
        .args(["health", "--api-url", &url, "--api-key", "flag-key-456"])
        .env("SCRAPIX_API_URL", "http://127.0.0.1:1")
        .env("SCRAPIX_API_KEY", "env-key-123")
        .output()
        .unwrap();

    assert_success(&output);
    assert!(received(&rx).contains("x-api-key: flag-key-456\r\n"));
}

#[test]
fn json_flag_prints_json() {
    let home = tempfile::tempdir().unwrap();
    let (url, rx) = one_shot_server();

    let output = scrapix(&home)
        .args(["--json", "health"])
        .env("SCRAPIX_API_URL", &url)
        .output()
        .unwrap();

    assert_success(&output);
    received(&rx);
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(body["status"], "ok");
}
