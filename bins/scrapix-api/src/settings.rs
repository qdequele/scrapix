//! Deployment mode and startup validation (standalone vs hosted).
//!
//! Resolved once at startup; any error aborts the process before a port is
//! bound, so a misconfigured engine never serves requests unauthenticated.

use crate::meili::MeiliTarget;
use crate::Args;

pub const DEFAULT_SQLITE_URL: &str = "sqlite://./data/scrapix.db";
const MIN_ADMIN_KEY_LEN: usize = 16;
const MIN_LAB_SECRET_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Standalone,
    Hosted,
}

#[derive(Debug, Clone)]
pub enum AuthSetting {
    AdminKey(String),
    Disabled,
    /// Hosted: credentials are resolved by the Lab.
    Lab,
}

#[derive(Debug, Clone)]
pub enum StoreUrl {
    Sqlite(String),
    Postgres(String),
}

/// How the hosted engine reaches the Lab (`Some` iff hosted). A standalone
/// engine has no Lab.
#[derive(Debug, Clone)]
pub struct LabSettings {
    /// The Lab base URL (no trailing `/`): events go to
    /// `{url}/internal/events`, lookups to `{url}/internal/*`.
    pub url: String,
    /// `LAB_INSTANCE_ID`: sent as `X-Lab-Instance-Id` on every call.
    pub instance_id: String,
    /// `LAB_INSTANCE_SECRET`: Bearer on service calls, HMAC key on events.
    pub instance_secret: String,
    /// `LAB_SERVICE_TOKEN`, which the Lab presents when it calls this
    /// engine for an account. Never sent to the Lab.
    pub service_token: String,
}

#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub mode: Mode,
    pub auth: AuthSetting,
    pub store: StoreUrl,
    pub meilisearch: Option<MeiliTarget>,
    /// `Some` iff hosted.
    pub lab: Option<LabSettings>,
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

const INSTANCE_SECRET_LEN: usize = 64;

fn lab_url_from(args: &Args) -> Result<Option<String>, ConfigError> {
    let url = match (non_empty(&args.lab_url), non_empty(&args.lab_events_url)) {
        (Some(u), _) => u,
        (None, Some(e)) => {
            tracing::warn!("LAB_EVENTS_URL is deprecated: set LAB_URL to the Lab base URL");
            crate::lab_client::LabClient::base_from_events_url(&e)
        }
        (None, None) => return Ok(None),
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return err(format!(
            "LAB_URL must start with http:// or https://, got `{url}`"
        ));
    }
    Ok(Some(url.trim_end_matches('/').to_string()))
}

/// Hosted: `LAB_INSTANCE_ID` (uuid) and `LAB_INSTANCE_SECRET` (64 hex
/// chars), both required.
fn instance_credentials(args: &Args) -> Result<(String, String), ConfigError> {
    let id = non_empty(&args.lab_instance_id);
    let secret = non_empty(&args.lab_instance_secret);
    match (id, secret) {
        (None, _) => err("SCRAPIX_MODE=hosted requires LAB_INSTANCE_ID (minted by the Lab: bin/rails lab:hosted_engine:create)"),
        (_, None) => err("SCRAPIX_MODE=hosted requires LAB_INSTANCE_SECRET (minted with LAB_INSTANCE_ID by the Lab)"),
        (Some(id), Some(secret)) => {
            if uuid::Uuid::parse_str(&id).is_err() {
                return err(format!("LAB_INSTANCE_ID must be a uuid, got `{id}`"));
            }
            if secret.len() != INSTANCE_SECRET_LEN || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
                return err("LAB_INSTANCE_SECRET must be the 64 hex characters the Lab minted");
            }
            Ok((id, secret))
        }
    }
}

fn warn_ignored_events_secret(args: &Args) {
    if non_empty(&args.lab_events_secret).is_some() {
        tracing::warn!(
            "LAB_EVENTS_SECRET is ignored: event batches are signed with LAB_INSTANCE_SECRET (unset it)"
        );
    }
}

/// The engine's own job store, in both modes: SQLite by default, or a
/// dedicated Postgres.
fn store_from(database_url: Option<String>) -> Result<StoreUrl, ConfigError> {
    match database_url {
        None => Ok(StoreUrl::Sqlite(DEFAULT_SQLITE_URL.to_string())),
        Some(u) if u.starts_with("sqlite:") => Ok(StoreUrl::Sqlite(u)),
        Some(u) if is_postgres(&u) => Ok(StoreUrl::Postgres(u)),
        Some(_) => err("DATABASE_URL must be sqlite: or postgres://"),
    }
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
                let url = lab_url_from(args)?.ok_or_else(|| {
                    ConfigError(
                        "SCRAPIX_MODE=hosted requires LAB_URL (the Lab base URL, e.g. http://127.0.0.1:8091)".into(),
                    )
                })?;
                if non_empty(&args.jwt_secret).is_some() {
                    tracing::info!("JWT_SECRET is ignored: the Lab verifies sessions");
                }
                warn_ignored_events_secret(args);
                let (instance_id, instance_secret) = instance_credentials(args)?;
                let service_token = match non_empty(&args.lab_service_token) {
                    Some(s) if s.chars().count() >= MIN_LAB_SECRET_LEN => s,
                    Some(_) => {
                        return err(format!(
                            "LAB_SERVICE_TOKEN must be at least {MIN_LAB_SECRET_LEN} characters"
                        ))
                    }
                    None => return err("SCRAPIX_MODE=hosted requires LAB_SERVICE_TOKEN (the Lab presents it when it calls this engine)"),
                };
                if meilisearch.is_some() {
                    tracing::warn!("MEILISEARCH_URL is ignored in hosted mode: tenants use the Meilisearch targets registered in the Lab");
                }
                let lab = LabSettings {
                    url,
                    instance_id,
                    instance_secret,
                    service_token,
                };
                if database_url.is_none() {
                    tracing::warn!(
                        "SCRAPIX_MODE=hosted without DATABASE_URL: the engine uses its default SQLite \
                         file ({DEFAULT_SQLITE_URL}). Undelivered Lab events (usage to bill) and job \
                         history live there: keep it on persistent storage, or set DATABASE_URL"
                    );
                }
                Ok(Self {
                    mode,
                    auth: AuthSetting::Lab,
                    store: store_from(database_url)?,
                    meilisearch: None,
                    lab: Some(lab),
                })
            }
            Mode::Standalone => {
                if non_empty(&args.jwt_secret).is_some() {
                    tracing::info!("JWT_SECRET is ignored in standalone mode");
                }
                if non_empty(&args.lab_instance_id).is_some()
                    || non_empty(&args.lab_instance_secret).is_some()
                {
                    return err(
                        "LAB_INSTANCE_ID/LAB_INSTANCE_SECRET are hosted-engine credentials: set SCRAPIX_MODE=hosted, or unset them (a standalone engine has no Lab)",
                    );
                }
                if non_empty(&args.lab_url).is_some()
                    || non_empty(&args.lab_events_url).is_some()
                    || non_empty(&args.lab_events_secret).is_some()
                    || non_empty(&args.lab_service_token).is_some()
                {
                    tracing::info!("LAB_* variables are ignored in standalone mode");
                }
                let auth = match (non_empty(&args.admin_key), auth_disabled) {
                    (Some(_), true) => {
                        return err(
                            "SCRAPIX_ADMIN_KEY and SCRAPIX_AUTH=disabled conflict: unset SCRAPIX_AUTH to require the key, or unset SCRAPIX_ADMIN_KEY to run without auth (local dev only)",
                        )
                    }
                    (None, true) => AuthSetting::Disabled,
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
                                " If this is a hosted deployment, set SCRAPIX_MODE=hosted (with LAB_URL and the engine's own DATABASE_URL)."
                            }
                            _ => "",
                        };
                        return err(format!(
                            "SCRAPIX_ADMIN_KEY is required in standalone mode (or SCRAPIX_AUTH=disabled for local dev).{hint}"
                        ));
                    }
                };
                Ok(Self {
                    mode,
                    auth,
                    store: store_from(database_url)?,
                    meilisearch,
                    lab: None,
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
    /// explicitly assign every one of the ten fields `resolve` reads,
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
        parsed.lab_url = get("LAB_URL");
        parsed.lab_events_url = get("LAB_EVENTS_URL");
        parsed.lab_events_secret = get("LAB_EVENTS_SECRET");
        parsed.lab_service_token = get("LAB_SERVICE_TOKEN");
        parsed.lab_instance_id = get("LAB_INSTANCE_ID");
        parsed.lab_instance_secret = get("LAB_INSTANCE_SECRET");
        parsed
    }

    const KEY: &str = "0123456789abcdef";
    const SECRET32: &str = "0123456789abcdef0123456789abcdef";
    const INSTANCE_ID: &str = "0f0f0f0f-0f0f-4f0f-8f0f-0f0f0f0f0f0f";
    const INSTANCE_SECRET: &str =
        "abababababababababababababababababababababababababababababababab";
    const LAB: [(&str, &str); 4] = [
        ("LAB_URL", "http://127.0.0.1:8091"),
        ("LAB_INSTANCE_ID", INSTANCE_ID),
        ("LAB_INSTANCE_SECRET", INSTANCE_SECRET),
        ("LAB_SERVICE_TOKEN", SECRET32),
    ];

    fn hosted(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut v = vec![("SCRAPIX_MODE", "hosted")];
        v.extend(LAB);
        v.extend_from_slice(extra);
        v
    }

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
        let e =
            EngineSettings::resolve(&args(&hosted(&[("SCRAPIX_AUTH", "disabled")]))).unwrap_err();
        assert!(e.0.contains("SCRAPIX_AUTH=disabled"), "{}", e.0);
    }

    #[test]
    fn admin_key_with_auth_disabled_refuses() {
        // Conflicting config: never silently drop a configured key.
        let e = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("SCRAPIX_AUTH", "disabled"),
        ]))
        .unwrap_err();
        assert!(e.0.contains("SCRAPIX_ADMIN_KEY"), "{}", e.0);
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
    fn hosted_needs_no_jwt_secret_and_defaults_to_its_own_sqlite() {
        let s = EngineSettings::resolve(&args(&hosted(&[]))).unwrap();
        assert!(matches!(s.auth, AuthSetting::Lab));
        assert!(matches!(s.store, StoreUrl::Sqlite(ref u) if u == DEFAULT_SQLITE_URL));
        assert_eq!(s.lab.unwrap().url, "http://127.0.0.1:8091");
    }

    #[test]
    fn hosted_accepts_its_own_postgres_and_ignores_jwt_secret() {
        let s = EngineSettings::resolve(&args(&hosted(&[
            ("DATABASE_URL", "postgres://db/scrapix_engine"),
            ("JWT_SECRET", "x"),
        ])))
        .unwrap();
        assert!(matches!(s.store, StoreUrl::Postgres(_)));
    }

    #[test]
    fn hosted_falls_back_to_lab_events_url_and_derives_the_base() {
        let env = [
            ("SCRAPIX_MODE", "hosted"),
            ("LAB_EVENTS_URL", "http://127.0.0.1:8091/internal/events"),
            ("LAB_INSTANCE_ID", INSTANCE_ID),
            ("LAB_INSTANCE_SECRET", INSTANCE_SECRET),
            ("LAB_SERVICE_TOKEN", SECRET32),
        ];
        assert_eq!(
            EngineSettings::resolve(&args(&env))
                .unwrap()
                .lab
                .unwrap()
                .url,
            "http://127.0.0.1:8091"
        );
    }

    #[test]
    fn hosted_requires_lab_url_instance_credentials_and_service_token() {
        let e = EngineSettings::resolve(&args(&[("SCRAPIX_MODE", "hosted")])).unwrap_err();
        assert!(e.0.contains("LAB_URL"), "{}", e.0);
        let mut no_id = hosted(&[]);
        no_id.retain(|(k, _)| *k != "LAB_INSTANCE_ID");
        assert!(EngineSettings::resolve(&args(&no_id))
            .unwrap_err()
            .0
            .contains("LAB_INSTANCE_ID"));
        let mut bad_id = hosted(&[]);
        bad_id[2] = ("LAB_INSTANCE_ID", "not-a-uuid");
        assert!(EngineSettings::resolve(&args(&bad_id))
            .unwrap_err()
            .0
            .contains("uuid"));
        let mut short = hosted(&[]);
        short[3] = ("LAB_INSTANCE_SECRET", "abcdef");
        assert!(EngineSettings::resolve(&args(&short))
            .unwrap_err()
            .0
            .contains("64 hex"));
        let mut not_hex = hosted(&[]);
        not_hex[3] = (
            "LAB_INSTANCE_SECRET",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        );
        assert!(EngineSettings::resolve(&args(&not_hex))
            .unwrap_err()
            .0
            .contains("64 hex"));
        let mut no_token = hosted(&[]);
        no_token.retain(|(k, _)| *k != "LAB_SERVICE_TOKEN");
        assert!(EngineSettings::resolve(&args(&no_token))
            .unwrap_err()
            .0
            .contains("LAB_SERVICE_TOKEN"));
        let mut bad = hosted(&[]);
        bad[1] = ("LAB_URL", "127.0.0.1:8091");
        assert!(EngineSettings::resolve(&args(&bad))
            .unwrap_err()
            .0
            .contains("LAB_URL"));
    }

    #[test]
    fn hosted_keeps_the_instance_credentials_and_the_service_token() {
        let s = EngineSettings::resolve(&args(&hosted(&[]))).unwrap();
        let lab = s.lab.unwrap();
        assert_eq!(lab.instance_id, INSTANCE_ID);
        assert_eq!(lab.instance_secret, INSTANCE_SECRET);
        assert_eq!(lab.service_token, SECRET32);
    }

    #[test]
    fn hosted_accepts_but_ignores_lab_events_secret() {
        let s =
            EngineSettings::resolve(&args(&hosted(&[("LAB_EVENTS_SECRET", SECRET32)]))).unwrap();
        assert!(s.lab.is_some());
    }

    #[test]
    fn standalone_refuses_instance_credentials() {
        // Spec 3.3: no "lab-connected standalone". A hosted env pasted onto a
        // standalone engine must not run unbilled.
        let e = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("LAB_URL", "http://127.0.0.1:8091"),
            ("LAB_INSTANCE_ID", INSTANCE_ID),
            ("LAB_INSTANCE_SECRET", INSTANCE_SECRET),
        ]))
        .unwrap_err();
        assert!(e.0.contains("LAB_INSTANCE_ID"), "{}", e.0);
        assert!(e.0.contains("SCRAPIX_MODE=hosted"), "{}", e.0);
        let e = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("LAB_INSTANCE_SECRET", INSTANCE_SECRET),
        ]))
        .unwrap_err();
        assert!(e.0.contains("LAB_INSTANCE_SECRET"), "{}", e.0);
    }

    #[test]
    fn standalone_ignores_lab_url() {
        let s = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("LAB_URL", "http://x"),
        ]))
        .unwrap();
        assert!(s.lab.is_none());
    }

    #[test]
    fn standalone_ignores_lab_settings() {
        let s = EngineSettings::resolve(&args(&[
            ("SCRAPIX_ADMIN_KEY", KEY),
            ("LAB_EVENTS_URL", "http://x"),
        ]))
        .unwrap();
        assert!(s.lab.is_none());
    }

    #[test]
    fn unknown_mode_refuses() {
        assert!(EngineSettings::resolve(&args(&[("SCRAPIX_MODE", "cloud")])).is_err());
    }

    #[test]
    fn hosted_ignores_the_operator_meilisearch() {
        let s = EngineSettings::resolve(&args(&hosted(&[
            ("MEILISEARCH_URL", "http://ops:7700"),
            ("MEILISEARCH_API_KEY", "ops"),
        ])))
        .unwrap();
        assert!(
            s.meilisearch.is_none(),
            "a tenant must never land in the operator's Meilisearch"
        );
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
