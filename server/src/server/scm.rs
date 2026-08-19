//! Resolved source-control-management (SCM) host registry.
//!
//! At startup the server takes the `[[system.scm_hosts]]` rows from
//! `system.toml` (deserialised as `ScmHostConfig`) and resolves each row's
//! `read_token_env` pointer against the process environment. The result is a
//! `ScmHostRegistry` of `ResolvedScmHost` values that the poller and the
//! `github_pr` URL parser consult at runtime.
//!
//! Why a registry instead of folding everything into the per-environment
//! config: the GitHub host is a property of the *data* (the PR's URL),
//! not of the runtime sandbox. The same `code-auto` environment can drive
//! a `github.com` PR or a `github.snooguts.net` PR depending on which
//! repo the operator picked, so the host needs to be looked up per-PR.

use std::env;

use crate::error::AppError;

use super::config::ScmHostConfig;

/// What kind of SCM the host runs. Determines which `Poller`/webhook
/// implementation handles it. Today only `Github` is implemented; new
/// kinds plug in by adding another impl + an arm in this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScmHostKind {
    Github,
}

impl ScmHostKind {
    fn parse(value: &str) -> Result<Self, AppError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "github" => Ok(Self::Github),
            other => Err(AppError::Config(format!(
                "unknown scm_host kind '{other}'; expected one of: github"
            ))),
        }
    }
}

impl std::fmt::Display for ScmHostKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Github => f.write_str("github"),
        }
    }
}

/// A host config row with `read_token_env` already resolved against the
/// process environment.
///
/// `token` is mandatory because every supported `kind` needs auth to be
/// useful; if you don't want a host polled, omit the row from
/// `[[system.scm_hosts]]` rather than registering it without a token.
#[derive(Debug, Clone)]
pub struct ResolvedScmHost {
    pub kind: ScmHostKind,
    pub host: String,
    pub api_base_url: String,
    pub read_token: String,
}

/// Lookup table for resolved hosts. Constructed once at startup; cheap to
/// clone (shared across the server).
#[derive(Debug, Clone, Default)]
pub struct ScmHostRegistry {
    hosts: Vec<ResolvedScmHost>,
}

impl ScmHostRegistry {
    #[cfg(test)]
    pub fn github_com_for_tests() -> Self {
        Self {
            hosts: vec![ResolvedScmHost {
                kind: ScmHostKind::Github,
                host: "github.com".to_string(),
                api_base_url: "https://api.github.com".to_string(),
                read_token: "test-token".to_string(),
            }],
        }
    }

    /// Hard-fails (returns `AppError::Config`) if any declared row has
    /// a missing or empty `host` / `api_base_url` / `read_token_env`, or if
    /// `read_token_env` points at an env var that is unset or empty. Silent
    /// skipping makes "polling stopped working" debugging miserable;
    /// if you don't want a host registered, delete its
    /// `[[system.scm_hosts]]` row.
    pub fn from_configs(configs: &[ScmHostConfig]) -> Result<Self, AppError> {
        let mut hosts = Vec::with_capacity(configs.len());
        for cfg in configs {
            let kind = ScmHostKind::parse(&cfg.kind)?;
            if cfg.host.trim().is_empty() {
                return Err(AppError::Config(
                    "scm_host entry has empty `host`".to_string(),
                ));
            }
            let host = protocol::canonical_github_host(&cfg.host).ok_or_else(|| {
                AppError::Config(format!("scm_host entry has invalid `host` '{}'", cfg.host))
            })?;
            if host.is_empty() {
                return Err(AppError::Config(
                    "scm_host entry has empty `host`".to_string(),
                ));
            }
            let api_base_url =
                protocol::canonical_github_api_base_for_host(&host, &cfg.api_base_url).ok_or_else(
                    || AppError::Config(format!("scm_host '{host}' has invalid `api_base_url`")),
                )?;
            let token_env_name = cfg
                .read_token_env
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    AppError::Config(format!(
                        "scm_host '{host}' (kind '{kind}') is missing `read_token_env`; \
                         declare which env var holds the auth token, or remove the row"
                    ))
                })?;
            let read_token = read_secret_env(token_env_name).ok_or_else(|| {
                AppError::Config(format!(
                    "scm_host '{host}': env var '{token_env_name}' (referenced by `read_token_env`) \
                     is empty or unset; populate it (e.g. in agent-hub/.env) or remove the row"
                ))
            })?;
            hosts.push(ResolvedScmHost {
                kind,
                host,
                api_base_url,
                read_token,
            });
        }
        if hosts.is_empty() {
            return Err(AppError::Config(
                "at least one system.scm_hosts entry is required".to_string(),
            ));
        }
        Ok(Self { hosts })
    }

    /// Find a configured host by its name (case-insensitive).
    pub fn find(&self, host: &str) -> Option<&ResolvedScmHost> {
        let needle = host.trim().to_ascii_lowercase();
        self.hosts.iter().find(|h| h.host == needle)
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

fn read_secret_env(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    env::var(trimmed)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: &str, host: &str, base: &str, read_token_env: Option<&str>) -> ScmHostConfig {
        ScmHostConfig {
            kind: kind.into(),
            host: host.into(),
            api_base_url: base.into(),
            read_token_env: read_token_env.map(Into::into),
        }
    }

    #[test]
    fn lookup_is_case_insensitive_on_host() {
        // SAFETY: each test uses a unique env var name so parallel
        // execution in the same test binary doesn't race.
        unsafe { env::set_var("SCM_TEST_TOKEN_INSENS", "tok") };
        let registry = ScmHostRegistry::from_configs(&[cfg(
            "github",
            "GitHub.SnooGuts.NET",
            "https://github.snooguts.net/api/v3/",
            Some("SCM_TEST_TOKEN_INSENS"),
        )])
        .expect("parse");
        let h = registry
            .find("github.snooguts.net")
            .expect("host resolves case-insensitively");
        assert_eq!(h.kind, ScmHostKind::Github);
        assert_eq!(h.api_base_url, "https://github.snooguts.net/api/v3");
        assert_eq!(h.read_token, "tok");
        unsafe { env::remove_var("SCM_TEST_TOKEN_INSENS") };
    }

    #[test]
    fn api_base_cannot_cross_scm_host_identity() {
        unsafe { env::set_var("SCM_TEST_TOKEN_CROSS_HOST", "tok") };
        for (host, api_base) in [
            ("github.com", "https://github.com/api/v3"),
            ("ghe.example.com", "https://api.other.example/api/v3"),
        ] {
            assert!(
                ScmHostRegistry::from_configs(&[cfg(
                    "github",
                    host,
                    api_base,
                    Some("SCM_TEST_TOKEN_CROSS_HOST")
                )])
                .is_err()
            );
        }
        unsafe { env::remove_var("SCM_TEST_TOKEN_CROSS_HOST") };
    }

    #[test]
    fn empty_registry_is_rejected() {
        assert!(ScmHostRegistry::from_configs(&[]).is_err());
    }

    #[test]
    fn empty_host_is_a_config_error() {
        let err = ScmHostRegistry::from_configs(&[cfg(
            "github",
            "",
            "https://api.github.com",
            Some("SCM_TEST_TOKEN_EMPTY_HOST"),
        )])
        .expect_err("empty host should reject");
        assert!(format!("{err}").contains("empty `host`"));
    }

    #[test]
    fn unknown_kind_is_a_config_error() {
        let err = ScmHostRegistry::from_configs(&[cfg(
            "magic",
            "magic.example.com",
            "https://magic.example.com/api",
            Some("SCM_TEST_TOKEN_MAGIC"),
        )])
        .expect_err("unknown kind should reject");
        assert!(format!("{err}").contains("unknown scm_host kind"));
    }

    /// Hard-fail #1: row declares `read_token_env = "..."` but the env var is
    /// empty/unset. This is the case Nick hit with `GITHUB_ENTERPRISE_TOKEN`
    /// and the failure mode we explicitly do not want to be silent.
    #[test]
    fn missing_env_var_value_is_a_hard_config_error() {
        // ensure the env var is not set
        unsafe { env::remove_var("SCM_TEST_TOKEN_DEFINITELY_UNSET") };
        let err = ScmHostRegistry::from_configs(&[cfg(
            "github",
            "ghe.example.com",
            "https://ghe.example.com/api/v3",
            Some("SCM_TEST_TOKEN_DEFINITELY_UNSET"),
        )])
        .expect_err("unset env var should reject");
        let msg = format!("{err}");
        assert!(
            msg.contains("SCM_TEST_TOKEN_DEFINITELY_UNSET"),
            "error mentions the env var name: {msg}"
        );
        assert!(msg.contains("empty or unset"), "error is explicit: {msg}");
    }

    /// Hard-fail #2: row omits `read_token_env` altogether. We don't have a
    /// "polling-disabled host" mode; an unauthed `kind = "github"` row
    /// is functionally useless.
    #[test]
    fn omitted_read_token_env_field_is_a_hard_config_error() {
        let err = ScmHostRegistry::from_configs(&[cfg(
            "github",
            "ghe.example.com",
            "https://ghe.example.com/api/v3",
            None,
        )])
        .expect_err("missing read_token_env should reject");
        let msg = format!("{err}");
        assert!(
            msg.contains("missing `read_token_env`"),
            "error mentions missing read_token_env: {msg}"
        );
    }

    /// Hard-fail #3: empty-string env var is treated identically to
    /// unset. Whitespace-only is also rejected.
    #[test]
    fn empty_string_env_var_value_is_a_hard_config_error() {
        unsafe { env::set_var("SCM_TEST_TOKEN_BLANK", "   ") };
        let err = ScmHostRegistry::from_configs(&[cfg(
            "github",
            "ghe.example.com",
            "https://ghe.example.com/api/v3",
            Some("SCM_TEST_TOKEN_BLANK"),
        )])
        .expect_err("blank env var should reject");
        unsafe { env::remove_var("SCM_TEST_TOKEN_BLANK") };
        assert!(format!("{err}").contains("empty or unset"));
    }
}
