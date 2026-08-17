//! Figma REST client. Everything network-facing sits behind [`FigmaApi`]
//! so the rest of the crate is testable offline.

use std::time::Duration;

use serde_json::Value;

/// Errors surfaced by [`FigmaApi`] implementations.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("rate limited; retry after {retry_after:?}")]
    RateLimited { retry_after: Duration },
    #[error("authentication failed — check FIGMA_TOKEN and file access")]
    Auth,
    #[error("figma returned {status}: {msg}")]
    Http { status: u16, msg: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("unexpected response shape: {0}")]
    Parse(String),
}

/// Subset of `GET /v1/files/:key/meta` figmog needs.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMetaResp {
    pub name: String,
    /// "The UTC ISO 8601 time at which the file content was last modified."
    pub last_touched_at: String,
}

/// The calls figmog makes. `file_meta` is Tier 3 (cheap, poll it); `file`
/// is Tier 1 (expensive, call only on change); `variables_local` is Tier 2
/// and Enterprise-only, called opportunistically by `pull` (build design
/// §12).
pub trait FigmaApi {
    /// `GET /v1/files/:key/meta` — Tier 3, cheap enough to poll.
    fn file_meta(&self, key: &str) -> Result<FileMetaResp, ApiError>;
    /// `GET /v1/files/:key` — Tier 1, the full document tree.
    fn file(&self, key: &str) -> Result<Value, ApiError>;
    /// `GET /v1/files/:key/variables/local` — Enterprise-only. `Ok(None)`
    /// means "not available on this plan" (never an error: `pull` falls
    /// back to v1 behavior). The default implementation always returns
    /// `Ok(None)`, so test doubles that only care about `file`/`file_meta`
    /// (e.g. `watch::tests::Script`) don't need to know this call exists.
    fn variables_local(&self, key: &str) -> Result<Option<Value>, ApiError> {
        let _ = key;
        Ok(None)
    }
}

pub(crate) fn parse_meta_response(v: &Value) -> Result<FileMetaResp, ApiError> {
    let file = v
        .get("file")
        .ok_or_else(|| ApiError::Parse("no `file` object".into()))?;
    let get = |k: &str| {
        file.get(k)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ApiError::Parse(format!("meta missing `{k}`")))
    };
    Ok(FileMetaResp {
        name: get("name")?,
        last_touched_at: get("last_touched_at")?,
    })
}

pub(crate) fn error_from_status(status: u16, retry_after: Option<&str>, msg: String) -> ApiError {
    match status {
        429 => ApiError::RateLimited {
            retry_after: Duration::from_secs(
                retry_after
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(60),
            ),
        },
        401 | 403 => ApiError::Auth,
        _ => ApiError::Http { status, msg },
    }
}

/// Blocking `ureq` implementation against api.figma.com.
pub struct UreqApi {
    token: String,
    base_url: String,
}

impl UreqApi {
    /// Client against the real `api.figma.com`, authenticated with a
    /// personal access token (`FIGMA_TOKEN`).
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, "https://api.figma.com".into())
    }
    /// `base_url` override for tests / proxies.
    pub fn with_base_url(token: String, base_url: String) -> Self {
        UreqApi { token, base_url }
    }

    fn get_json(&self, path: &str) -> Result<Value, ApiError> {
        let url = format!("{}{}", self.base_url, path);
        match ureq::get(&url).set("X-Figma-Token", &self.token).call() {
            Ok(resp) => resp.into_json().map_err(|e| ApiError::Parse(e.to_string())),
            Err(ureq::Error::Status(status, resp)) => {
                let retry = resp.header("Retry-After").map(str::to_string);
                let msg = resp.into_string().unwrap_or_default();
                Err(error_from_status(status, retry.as_deref(), msg))
            }
            Err(e) => Err(ApiError::Network(e.to_string())),
        }
    }

    /// `GET /v1/files/:key/nodes?ids=<id>` — the native API's closest
    /// equivalent of `figmog_node`. Used only by `figmog bench`'s real-file
    /// API-comparison phase (build design §13); no other call site needs
    /// this endpoint, so it isn't part of the [`FigmaApi`] trait.
    pub(crate) fn file_nodes(&self, key: &str, id: &str) -> Result<Value, ApiError> {
        self.get_json(&format!("/v1/files/{key}/nodes?ids={id}"))
    }
}

impl FigmaApi for UreqApi {
    fn file_meta(&self, key: &str) -> Result<FileMetaResp, ApiError> {
        parse_meta_response(&self.get_json(&format!("/v1/files/{key}/meta"))?)
    }
    fn file(&self, key: &str) -> Result<Value, ApiError> {
        self.get_json(&format!("/v1/files/{key}"))
    }
    fn variables_local(&self, key: &str) -> Result<Option<Value>, ApiError> {
        match self.get_json(&format!("/v1/files/{key}/variables/local")) {
            Ok(v) => Ok(Some(v)),
            Err(e) if variables_local_is_gated(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Whether a `variables_local` failure means "this plan can't see
/// variables" (403/404 — skip silently, v1 behavior holds) rather than a
/// real failure `pull` should propagate. By the time `variables_local` is
/// called, `file()` has already succeeded against the same token, so a 401/
/// 403 here means plan gating, not a bad token — `error_from_status` maps
/// both to `ApiError::Auth`.
fn variables_local_is_gated(e: &ApiError) -> bool {
    matches!(e, ApiError::Auth) || matches!(e, ApiError::Http { status: 404, .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_meta_envelope() {
        let v = serde_json::json!({
            "file": {"name": "F", "last_touched_at": "2026-08-01T00:00:00Z", "folder_name": "x"}
        });
        let m = parse_meta_response(&v).unwrap();
        assert_eq!(m.name, "F");
        assert_eq!(m.last_touched_at, "2026-08-01T00:00:00Z");
    }

    #[test]
    fn meta_missing_fields_is_parse_error() {
        assert!(matches!(
            parse_meta_response(&serde_json::json!({"file": {}})),
            Err(ApiError::Parse(_))
        ));
    }

    #[test]
    fn error_from_status_maps_429_and_403() {
        assert!(matches!(
            error_from_status(429, Some("30"), "slow down".into()),
            ApiError::RateLimited { retry_after } if retry_after == std::time::Duration::from_secs(30)
        ));
        // absent/garbage Retry-After falls back to 60s
        assert!(matches!(
            error_from_status(429, None, String::new()),
            ApiError::RateLimited { retry_after } if retry_after == std::time::Duration::from_secs(60)
        ));
        assert!(matches!(
            error_from_status(403, None, String::new()),
            ApiError::Auth
        ));
        assert!(matches!(
            error_from_status(500, None, "boom".into()),
            ApiError::Http { status: 500, .. }
        ));
    }

    #[test]
    fn variables_local_gated_on_403_and_404_only() {
        assert!(variables_local_is_gated(&ApiError::Auth));
        assert!(variables_local_is_gated(&ApiError::Http {
            status: 404,
            msg: String::new()
        }));
        assert!(!variables_local_is_gated(&ApiError::Http {
            status: 500,
            msg: "boom".into()
        }));
        assert!(!variables_local_is_gated(&ApiError::Network("down".into())));
        assert!(!variables_local_is_gated(&ApiError::RateLimited {
            retry_after: Duration::from_secs(1)
        }));
    }

    /// A test double that only implements the two calls it needs — proving
    /// the trait's default `variables_local` (used by e.g.
    /// `watch::tests::Script`) is `Ok(None)` without requiring every
    /// `FigmaApi` implementor to know the Enterprise endpoint exists.
    struct NoVariablesOverride;
    impl FigmaApi for NoVariablesOverride {
        fn file_meta(&self, _key: &str) -> Result<FileMetaResp, ApiError> {
            unimplemented!("not exercised by this test")
        }
        fn file(&self, _key: &str) -> Result<Value, ApiError> {
            unimplemented!("not exercised by this test")
        }
    }

    #[test]
    fn default_variables_local_is_none() {
        assert!(matches!(
            NoVariablesOverride.variables_local("ABC123"),
            Ok(None)
        ));
    }
}
