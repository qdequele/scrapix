use anyhow::{Context, Result};
use colored::Colorize;

use crate::client::ApiClient;
use crate::config::{AuthCredential, CliConfig};
use crate::output::{print_error, print_info, print_json, print_success};
use crate::types::{BillingResponse, HealthResponse, WhoamiResponse};

// ============================================================================
// Login command
// ============================================================================

pub async fn handle_login(api_url: &str) -> Result<()> {
    eprintln!(
        "Enter your API key (from {}/dashboard/api-keys):",
        api_url.trim_end_matches("/api").trim_end_matches('/')
    );

    let api_key: String = dialoguer::Password::new()
        .with_prompt("API key")
        .interact()?;

    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        anyhow::bail!("API key cannot be empty");
    }

    let client = verify_api_key(api_url, &api_key).await?;

    let credits_msg = match client.get::<BillingResponse>("/account/billing").await {
        Ok(billing) => format!(" — {} credits remaining", billing.credits_balance),
        Err(_) => String::new(),
    };

    let mut config = CliConfig::load().unwrap_or_default();
    config.api_url = Some(api_url.to_string());
    config.api_key = Some(api_key.clone());
    config.save()?;

    print_success(&format!(
        "Authenticated with key {}{}",
        mask_key(&api_key).cyan(),
        credits_msg
    ));
    Ok(())
}

/// Check `api_key` against an authenticated route (`/health` is public, so
/// it would accept any key) and return the client built with it.
async fn verify_api_key(api_url: &str, api_key: &str) -> Result<ApiClient> {
    let client = ApiClient::new(api_url, Some(AuthCredential::ApiKey(api_key.to_string())));
    client
        .get::<serde_json::Value>("/jobs?limit=1")
        .await
        .with_context(|| {
            format!(
                "API key rejected or unverifiable (GET {}/jobs?limit=1 failed)",
                client.base_url
            )
        })?;
    Ok(client)
}

/// A key for display: at most its last 4 characters (none for a key of 4
/// characters or fewer). Counts chars, so non-ASCII keys never panic.
fn mask_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= 4 {
        return "****".to_string();
    }
    let tail: String = key.chars().skip(len - 4).collect();
    format!("****{tail}")
}

// ============================================================================
// Other auth commands
// ============================================================================

pub async fn handle_logout() -> Result<()> {
    CliConfig::clear()?;
    print_success("Logged out. Credentials removed.");
    Ok(())
}

pub async fn handle_whoami(client: &ApiClient, json: bool) -> Result<()> {
    let whoami: WhoamiResponse = client.get("/auth/me").await?;

    if json {
        print_json(&whoami);
    } else {
        eprintln!();
        eprintln!("{}", "Current User".bold().underline());
        eprintln!();
        if let Some(ref email) = whoami.email {
            eprintln!("  {} {}", "Email:".dimmed(), email);
        }
        if let Some(ref name) = whoami.name {
            eprintln!("  {} {}", "Name:".dimmed(), name);
        }
        if let Some(ref account_name) = whoami.account_name {
            eprintln!("  {} {}", "Account:".dimmed(), account_name);
        }
        if let Some(ref tier) = whoami.tier {
            eprintln!("  {} {}", "Tier:".dimmed(), tier);
        }
        if let Some(credits) = whoami.credits_balance {
            eprintln!("  {} {}", "Credits:".dimmed(), credits);
        }
        eprintln!();
    }
    Ok(())
}

pub async fn handle_status_auth(client: &ApiClient, json: bool) -> Result<()> {
    let health: HealthResponse = client.get("/health").await?;
    let billing = client.get::<BillingResponse>("/account/billing").await.ok();
    let config = CliConfig::load().unwrap_or_default();

    let auth_method = if config.api_key.is_some() {
        "api_key"
    } else {
        "none"
    };

    if json {
        let result = serde_json::json!({
            "authenticated": auth_method != "none",
            "auth_method": auth_method,
            "api_url": client.base_url,
            "api_status": health.status,
            "api_version": health.version,
            "credits_balance": billing.as_ref().map(|b| b.credits_balance),
            "tier": billing.as_ref().and_then(|b| b.tier.clone()),
        });
        print_json(&result);
    } else {
        eprintln!();
        eprintln!("{}", "Auth Status".bold().underline());
        eprintln!();
        eprintln!("  {} {}", "API URL:".dimmed(), client.base_url);
        eprintln!(
            "  {} {}",
            "Status:".dimmed(),
            if health.status == "ok" {
                "connected".green()
            } else {
                "error".red()
            }
        );
        eprintln!("  {} {}", "Version:".dimmed(), health.version);
        eprintln!(
            "  {} {}",
            "Auth:".dimmed(),
            match auth_method {
                "api_key" => "API key".green(),
                _ => "not authenticated".red(),
            }
        );
        if let Some(ref b) = billing {
            if let Some(ref tier) = b.tier {
                eprintln!("  {} {}", "Tier:".dimmed(), tier);
            }
            eprintln!("  {} {}", "Credits:".dimmed(), b.credits_balance);
        }
        eprintln!();

        let config_path = CliConfig::config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        if std::path::Path::new(&config_path).exists() {
            print_info(&format!("Config: {}", config_path));
        } else {
            print_error("No config file found. Run 'scrapix login' to authenticate.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[test]
    fn mask_key_shows_at_most_the_last_four_chars() {
        assert_eq!(mask_key("sk_live_0123456789abcdef"), "****cdef");
        assert_eq!(mask_key("e2e-admin-key-0123456789"), "****6789");
        // Short keys reveal nothing.
        assert_eq!(mask_key("abcd"), "****");
        assert_eq!(mask_key(""), "****");
        // Non-ASCII: counted in chars, never byte-sliced (no panic).
        assert_eq!(mask_key("clé-secrète-très-longue-éàü"), "****-éàü");
        assert_eq!(mask_key("ééééééééééééé"), "****éééé");
    }

    /// One-shot HTTP server answering every request with `status` and
    /// `body`; returns its base URL and the first request line it saw.
    async fn one_shot_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = tx.send(request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        (url, rx)
    }

    #[tokio::test]
    async fn verify_api_key_uses_an_authenticated_route() {
        let (url, rx) = one_shot_server("200 OK", "[]").await;
        verify_api_key(&url, "sk_live_good-key").await.unwrap();
        let request = rx.await.unwrap();
        assert!(request.starts_with("GET /jobs?limit=1 "), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-api-key: sk_live_good-key"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn verify_api_key_rejects_a_wrong_key() {
        let (url, _rx) = one_shot_server(
            "401 Unauthorized",
            r#"{"error":"Missing or invalid admin key","code":"unauthorized"}"#,
        )
        .await;
        let Err(err) = verify_api_key(&url, "wrong-key").await else {
            panic!("a wrong key must fail verification");
        };
        assert!(format!("{err:#}").contains("rejected"), "{err:#}");
    }
}
