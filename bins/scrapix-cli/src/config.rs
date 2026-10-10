use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default)]
    pub api_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
}

impl CliConfig {
    pub fn config_dir() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .context("Could not determine config directory")?
            .join("scrapix");
        Ok(dir)
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config: {}", path.display()))?;
        toml::from_str(&content).with_context(|| "Failed to parse config.toml")
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir()?;
        std::fs::create_dir_all(&dir)?;
        let path = Self::config_path()?;
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    pub fn clear() -> Result<()> {
        let path = Self::config_path()?;
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// The stored credential: the API key saved by `scrapix login`.
    pub fn auth_credential(&self) -> Option<AuthCredential> {
        self.api_key
            .as_ref()
            .map(|k| AuthCredential::ApiKey(k.clone()))
    }
}

#[derive(Debug, Clone)]
pub enum AuthCredential {
    ApiKey(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config written by an older CLI (OAuth browser login) still loads;
    /// its token fields are ignored and only the API key counts.
    #[test]
    fn a_config_with_legacy_oauth_fields_still_parses() {
        let cfg: CliConfig = toml::from_str(
            r#"
            api_url = "https://api.example.com"
            access_token = "tok"
            refresh_token = "ref"
            token_expires_at = 1700000000
            oauth_client_id = "cli"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.api_url.as_deref(), Some("https://api.example.com"));
        assert!(cfg.auth_credential().is_none());
    }
}
