//! Deployment mode and startup validation (standalone vs hosted).
//!
//! Resolved once at startup; any error aborts the process before a port is
//! bound, so a misconfigured engine never serves requests unauthenticated.

use crate::meili::MeiliTarget;
use crate::Args;

pub const DEFAULT_SQLITE_URL: &str = "sqlite://./data/scrapix.db";
const MIN_ADMIN_KEY_LEN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Standalone,
    Hosted,
}

#[derive(Debug, Clone)]
pub enum AuthSetting {
    AdminKey(String),
    Disabled,
    Saas { jwt_secret: String },
}

#[derive(Debug, Clone)]
pub enum StoreUrl {
    Sqlite(String),
    Postgres(String),
}

#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub mode: Mode,
    pub auth: AuthSetting,
    pub store: StoreUrl,
    pub meilisearch: Option<MeiliTarget>,
}

#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConfigError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ConfigError> {
    Err(ConfigError(msg.into()))
}

fn non_empty(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn is_postgres(url: &str) -> bool {
    url.starts_with("postgres://") || url.starts_with("postgresql://")
}

impl EngineSettings {
    pub fn resolve(args: &Args) -> Result<Self, ConfigError> {
        let mode = match args.mode.trim() {
            "standalone" => Mode::Standalone,
            "hosted" => Mode::Hosted,
            other => {
                return err(format!(
                    "SCRAPIX_MODE must be `standalone` or `hosted`, got `{other}`"
                ))
            }
        };
        let database_url = non_empty(&args.database_url);
        let auth_disabled = match non_empty(&args.auth).as_deref() {
            None => false,
            Some("disabled") => true,
            Some(other) => {
                return err(format!(
                    "SCRAPIX_AUTH only accepts `disabled`, got `{other}`"
                ))
            }
        };
        let meilisearch = match non_empty(&args.meilisearch_url) {
            None => None,
            Some(url) => {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return err(format!(
                        "MEILISEARCH_URL must start with http:// or https://, got `{url}`"
                    ));
                }
                Some(MeiliTarget {
                    url: url.trim_end_matches('/').to_string(),
                    api_key: non_empty(&args.meilisearch_api_key),
                })
            }
        };

        match mode {
            Mode::Hosted => {
                if auth_disabled {
                    return err("SCRAPIX_AUTH=disabled is not allowed with SCRAPIX_MODE=hosted");
                }
                let Some(url) = database_url else {
                    return err("SCRAPIX_MODE=hosted requires DATABASE_URL (the Rails Postgres)");
                };
                if !is_postgres(&url) {
                    return err("SCRAPIX_MODE=hosted requires a postgres:// DATABASE_URL");
                }
                let jwt_secret = non_empty(&args.jwt_secret).unwrap_or_else(|| {
                    if std::env::var("ENVIRONMENT").as_deref() == Ok("production") {
                        panic!(
                            "JWT_SECRET is required in production. Set the JWT_SECRET environment variable."
                        );
                    }
                    tracing::warn!(
                        "JWT_SECRET not set — using insecure default. Set JWT_SECRET in production!"
                    );
                    "dev-jwt-secret-change-in-production".to_string()
                });
                Ok(Self {
                    mode,
                    auth: AuthSetting::Saas { jwt_secret },
                    store: StoreUrl::Postgres(url),
                    meilisearch,
                })
            }
            Mode::Standalone => {
                if non_empty(&args.jwt_secret).is_some() {
                    tracing::info!("JWT_SECRET is ignored in standalone mode");
                }
                let auth = match (non_empty(&args.admin_key), auth_disabled) {
                    (_, true) => AuthSetting::Disabled,
                    (Some(k), false) if k.chars().count() >= MIN_ADMIN_KEY_LEN => {
                        AuthSetting::AdminKey(k)
                    }
                    (Some(_), false) => {
                        return err(format!(
                            "SCRAPIX_ADMIN_KEY must be at least {MIN_ADMIN_KEY_LEN} characters"
                        ))
                    }
                    (None, false) => {
                        let hint = match database_url.as_deref() {
                            Some(u) if is_postgres(u) => {
                                " If this is a hosted (Rails) deployment, set SCRAPIX_MODE=hosted."
                            }
                            _ => "",
                        };
                        return err(format!(
                            "SCRAPIX_ADMIN_KEY is required in standalone mode (or SCRAPIX_AUTH=disabled for local dev).{hint}"
                        ));
                    }
                };
                let store = match database_url {
                    None => StoreUrl::Sqlite(DEFAULT_SQLITE_URL.to_string()),
                    Some(u) if u.starts_with("sqlite:") => StoreUrl::Sqlite(u),
                    Some(u) if is_postgres(&u) => StoreUrl::Postgres(u),
                    Some(_) => {
                        return err(
                            "DATABASE_URL must be sqlite: or postgres:// in standalone mode",
                        )
                    }
                };
                Ok(Self {
                    mode,
                    auth,
                    store,
                    meilisearch,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Builds `Args` from an explicit set of (env-var-name, value) pairs.
    ///
    /// `Args::parse_from` still reads `env = ...` attributes from the real
    /// process environment for any field not given on argv, so — to keep
    /// this test hermetic — we parse a bare argv (no flags at all) and then
    /// explicitly assign every one of the seven fields `resolve` reads,
    /// defaulting to `None` (or `"standalone"` for `mode`) when the pair is
    /// absent, instead of trusting parse_from to leave them unset.
    fn args(env: &[(&str, &str)]) -> Args {
        let get = |name: &str| -> Option<String> {
            env.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        };

        let mut parsed = Args::parse_from(["scrapix-api"]);
        parsed.mode = get("SCRAPIX_MODE").unwrap_or_else(|| "standalone".to_string());
        parsed.admin_key = get("SCRAPIX_ADMIN_KEY");
        parsed.auth = get("SCRAPIX_AUTH");
        parsed.database_url = get("DATABASE_URL");
        parsed.jwt_secret = get("JWT_SECRET");
        parsed.meilisearch_url = get("MEILISEARCH_URL");
        parsed.meilisearch_api_key = get("MEILISEARCH_API_KEY");
        parsed
    }

    const KEY: &str = "0123456789abcdef";

    #[test]
    fn standalone_defaults_to_sqlite_and_admin_key() {
        let s = EngineSettings::resolve(&args(&[("SCRAPIX_ADMIN_KEY", KEY)])).unwrap();
        assert!(matches!(s.mode, Mode::Standalone));
        assert!(matches!(s.auth, AuthSetting::AdminKey(ref k) if k == KEY));
        assert!(matches!(s.store, StoreUrl::Sqlite(ref u) if u == "sqlite://./data/scrapix.db"));
        assert!(s.meilisearch.is_none());
    }

    #[test]
    fn standalone_without_key_refuses() {
        let e = EngineSettings::resolve(&args(&[])).unwrap_err();
        assert!(e.0.contains("SCRAPIX_ADMIN_KEY"), "{}", e.0);
    }

    #[test]
    fn standalone_without_key_but_postgres_hints_hosted_mode() {
        let e = EngineSettings::resolve(&args(&[("DATABASE_URL", "postgres://u:p@db/scrapix")]))
            .unwrap_err();
        assert!(e.0.contains("SCRAPIX_MODE=hosted"), "{}", e.0);
    }

    #[test]
    fn auth_disabled_is_allowed_in_standalone_only() {
        let s = EngineSettings::resolve(&args(&[("SCRAPIX_AUTH", "disabled")])).unwrap();
        assert!(matches!(s.auth, AuthSetting::Disabled));
        let e = EngineSettings::resolve(&args(&[
            ("SCRAPIX_MODE", "hosted"),
            ("SCRAPIX_AUTH", "disabled"),
            ("DATABASE_URL", "postgres://db/x"),
        ]))
        .unwrap_err();
        assert!(e.0.contains("SCRAPIX_AUTH=disabled"), "{}", e.0);
    }

    #[test]
    fn short_key_refuses_and_key_is_trimmed() {
        assert!(EngineSettings::resolve(&args(&[("SCRAPIX_ADMIN_KEY", "short")])).is_err());
        let s = EngineSettings::resolve(&args(&[("SCRAPIX_ADMIN_KEY", "  0123456789abcdef\n")]))
            .unwrap();
        assert!(matches!(s.auth, AuthSetting::AdminKey(ref k) if k == KEY));
        // whitespace-only counts as unset
        let e = EngineSettings::resolve(&args(&[("SCRAPIX_ADMIN_KEY", "   ")])).unwrap_err();
        assert!(e.0.contains("SCRAPIX_ADMIN_KEY"));
    }

    #[test]
    fn database_url_schemes() {
        let pg = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("DATABASE_URL", "postgresql://db/x"),
        ]))
        .unwrap();
        assert!(matches!(pg.store, StoreUrl::Postgres(_)));
        assert!(EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("DATABASE_URL", "mysql://db/x"),
        ]))
        .is_err());
    }

    #[test]
    fn hosted_requires_postgres_and_reads_jwt() {
        assert!(EngineSettings::resolve(&args(&[("SCRAPIX_MODE", "hosted")])).is_err());
        assert!(EngineSettings::resolve(&args(&[
            ("SCRAPIX_MODE", "hosted"),
            ("DATABASE_URL", "sqlite://x.db"),
        ]))
        .is_err());
        let s = EngineSettings::resolve(&args(&[
            ("SCRAPIX_MODE", "hosted"),
            ("DATABASE_URL", "postgres://db/x"),
            ("JWT_SECRET", "s3cret"),
        ]))
        .unwrap();
        assert!(matches!(s.auth, AuthSetting::Saas { ref jwt_secret } if jwt_secret == "s3cret"));
    }

    #[test]
    fn unknown_mode_refuses() {
        assert!(EngineSettings::resolve(&args(&[("SCRAPIX_MODE", "cloud")])).is_err());
    }

    #[test]
    fn meilisearch_url_validation() {
        let s = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("MEILISEARCH_URL", "http://meili:7700/"),
            ("MEILISEARCH_API_KEY", "mk"),
        ]))
        .unwrap();
        let m = s.meilisearch.unwrap();
        assert_eq!(m.url, "http://meili:7700");
        assert_eq!(m.api_key.as_deref(), Some("mk"));
        let e = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("MEILISEARCH_URL", "meili:7700"),
        ]))
        .unwrap_err();
        assert!(e.0.contains("MEILISEARCH_URL"), "{}", e.0);
    }
}
