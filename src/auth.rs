//! One saved personal access token per OS user, independent of projects
//! and worktrees. An explicit FIGMA_TOKEN always wins; an empty value
//! deliberately disables saved credentials (offline use and test isolation).

use std::ffi::OsString;

use keyring::Entry;
use serde_json::{Value, json};

const SERVICE: &str = "figmog";
const ACCOUNT: &str = "figma-token";
pub(crate) const MAX_TOKEN_BYTES: usize = 4096;

struct Credential {
    value: String,
    source: &'static str,
}

#[cfg(test)]
struct TestAuth {
    entry: Entry,
    environment: Option<OsString>,
}

#[cfg(test)]
thread_local! {
    static TEST_AUTH: std::cell::RefCell<Option<TestAuth>> = const { std::cell::RefCell::new(None) };
}

fn environment_token() -> Option<OsString> {
    #[cfg(test)]
    if let Some(environment) =
        TEST_AUTH.with(|auth| auth.borrow().as_ref().map(|auth| auth.environment.clone()))
    {
        return environment;
    }
    std::env::var_os("FIGMA_TOKEN")
}

fn entry<T>(use_entry: impl FnOnce(&Entry) -> Result<T, String>) -> Result<T, String> {
    #[cfg(test)]
    if TEST_AUTH.with(|auth| auth.borrow().is_some()) {
        return TEST_AUTH.with(|auth| use_entry(&auth.borrow().as_ref().unwrap().entry));
    }
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(
            "saved credentials are supported on macOS and Linux; set FIGMA_TOKEN on this platform"
                .into(),
        );
    }
    let entry = Entry::new(SERVICE, ACCOUNT).map_err(|_| storage_error("open"))?;
    use_entry(&entry)
}

// Do not format backend errors: BadEncoding, for example, can contain
// secret bytes. Errors and JSON results must never reveal the token.
fn storage_error(action: &str) -> String {
    format!(
        "could not {action} the saved Figma token in the system credential store — unlock macOS Keychain or start a Linux Secret Service; alternatively set FIGMA_TOKEN (an empty value disables saved credentials)"
    )
}

fn read_saved(entry: &Entry) -> Result<Option<String>, String> {
    match entry.get_password() {
        Ok(token) => normalize_token(&token).map(|token| Some(token.to_string())),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(storage_error("read")),
    }
}

fn resolve_with(
    environment: Option<OsString>,
    saved: impl FnOnce() -> Result<Option<String>, String>,
) -> Result<Option<Credential>, String> {
    if let Some(value) = environment {
        let value = value
            .into_string()
            .map_err(|_| "FIGMA_TOKEN must be valid UTF-8".to_string())?;
        if value.is_empty() {
            return Ok(None);
        }
        return Ok(Some(Credential {
            value: normalize_token(&value)?.to_string(),
            source: "environment",
        }));
    }
    Ok(saved()?.map(|value| Credential {
        value,
        source: "credential_store",
    }))
}

fn resolve() -> Result<Option<Credential>, String> {
    resolve_with(environment_token(), || entry(read_saved))
}

/// Shared by serve, network pulls, and direct image downloads.
pub(crate) fn token() -> Result<Option<String>, String> {
    // A locked/unavailable store must not prevent an idle MCP server or
    // cached images from working offline. Network callers report missing
    // credentials; `auth status` keeps the storage error visible.
    let credential = resolve_with(
        environment_token(),
        || Ok(entry(read_saved).unwrap_or(None)),
    )?;
    Ok(credential.map(|credential| credential.value))
}

pub(crate) fn missing_token(operation: &str) -> String {
    let reason = if environment_token().is_some_and(|value| value.is_empty()) {
        "FIGMA_TOKEN is empty, which disables saved credentials; unset FIGMA_TOKEN to use saved credentials"
    } else {
        "FIGMA_TOKEN not set and no saved token is available"
    };
    format!(
        "{reason} — required for {operation}; run `figmog auth login` or set FIGMA_TOKEN (`figmog auth status` checks credential-store access)"
    )
}

/// Validate only the input shape, without a network request or requiring
/// extra Figma scopes. Expiration and file permissions are checked by Figma
/// when the token is actually used.
pub(crate) fn normalize_token(token: &str) -> Result<&str, String> {
    let token = token.trim();
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
        return Err("token must contain between 1 and 4096 bytes".into());
    }
    if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("token must be a single line of ASCII text without spaces".into());
    }
    Ok(token)
}

fn save_to(entry: &Entry, token: &str) -> Result<(), String> {
    let token = normalize_token(token)?;
    entry.set_password(token).map_err(|_| storage_error("save"))
}

pub(crate) fn login(token: &str) -> Result<Value, String> {
    // Reject invalid input before accessing the system store.
    let token = normalize_token(token)?;
    entry(|entry| save_to(entry, token))?;
    Ok(json!({
        "saved": true,
        "storage": "credential_store",
        "environment_override": environment_token().is_some(),
    }))
}

pub(crate) fn status() -> Result<Value, String> {
    let credential = resolve()?;
    Ok(json!({
        "configured": credential.is_some(),
        "source": credential.map(|credential| credential.source),
        "validated": false,
    }))
}

fn remove_from(entry: &Entry) -> Result<bool, String> {
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(_) => Err(storage_error("remove")),
    }
}

pub(crate) fn logout() -> Result<Value, String> {
    let removed = entry(remove_from)?;
    Ok(json!({
        "removed": removed,
        "environment_override": environment_token().is_some(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring::mock::MockCredential;

    fn mock_entry() -> Entry {
        Entry::new_with_credential(Box::new(MockCredential::default()))
    }

    struct MockAuth;

    impl MockAuth {
        fn new(environment: Option<OsString>) -> Self {
            TEST_AUTH.with(|auth| {
                assert!(auth.borrow().is_none());
                *auth.borrow_mut() = Some(TestAuth {
                    entry: mock_entry(),
                    environment,
                });
            });
            Self
        }

        fn set_environment(&self, environment: Option<OsString>) {
            TEST_AUTH.with(|auth| auth.borrow_mut().as_mut().unwrap().environment = environment);
        }
    }

    impl Drop for MockAuth {
        fn drop(&mut self) {
            TEST_AUTH.with(|auth| auth.borrow_mut().take());
        }
    }

    #[test]
    fn auth_commands_share_the_saved_credential() {
        let auth = MockAuth::new(None);
        assert_eq!(
            status().unwrap(),
            json!({"configured": false, "source": null, "validated": false})
        );
        let saved = login("figd_command_round_trip").unwrap();
        assert_eq!(
            saved,
            json!({"saved": true, "storage": "credential_store", "environment_override": false})
        );
        TEST_AUTH.with(|auth| {
            assert_eq!(
                auth.borrow()
                    .as_ref()
                    .unwrap()
                    .entry
                    .get_password()
                    .unwrap(),
                "figd_command_round_trip"
            );
        });
        assert_eq!(token().unwrap().as_deref(), Some("figd_command_round_trip"));
        let configured = status().unwrap();
        assert_eq!(
            configured,
            json!({"configured": true, "source": "credential_store", "validated": false})
        );
        assert!(!configured.to_string().contains("figd_command_round_trip"));

        auth.set_environment(Some("".into()));
        assert!(token().unwrap().is_none());
        assert_eq!(status().unwrap()["configured"], false);
        auth.set_environment(None);
        assert_eq!(token().unwrap().as_deref(), Some("figd_command_round_trip"));

        assert_eq!(
            logout().unwrap(),
            json!({"removed": true, "environment_override": false})
        );
        TEST_AUTH.with(|auth| {
            assert!(matches!(
                auth.borrow().as_ref().unwrap().entry.get_password(),
                Err(keyring::Error::NoEntry)
            ));
        });
        assert!(token().unwrap().is_none());
        assert_eq!(
            status().unwrap(),
            json!({"configured": false, "source": null, "validated": false})
        );
        assert_eq!(logout().unwrap()["removed"], false);
    }

    #[test]
    fn missing_token_distinguishes_empty_and_unset_environment() {
        let auth = MockAuth::new(None);
        let unset = missing_token("network pulls");
        assert!(unset.contains("FIGMA_TOKEN not set"));
        auth.set_environment(Some("".into()));
        let empty = missing_token("network pulls");
        assert!(empty.contains("FIGMA_TOKEN is empty"));
        assert!(empty.contains("disables saved credentials"));
        assert!(empty.contains("unset FIGMA_TOKEN to use saved credentials"));
        assert!(!empty.contains("FIGMA_TOKEN not set"));
        for message in [unset, empty] {
            assert!(message.contains("network pulls"));
            assert!(message.contains("figmog auth login"));
            assert!(message.contains("set FIGMA_TOKEN"));
            assert!(message.contains("figmog auth status"));
            assert!(message.contains("credential-store access"));
        }
    }

    #[test]
    fn saved_token_can_be_replaced_resolved_and_removed() {
        let entry = mock_entry();
        assert!(read_saved(&entry).unwrap().is_none());
        save_to(&entry, "figd_first").unwrap();
        save_to(&entry, "figd_replacement\n").unwrap();
        let credential = resolve_with(None, || read_saved(&entry)).unwrap().unwrap();
        assert_eq!(credential.value, "figd_replacement");
        assert_eq!(credential.source, "credential_store");
        assert!(remove_from(&entry).unwrap());
        assert!(!remove_from(&entry).unwrap());
        assert!(resolve_with(None, || read_saved(&entry)).unwrap().is_none());
    }

    #[test]
    fn environment_overrides_saved_credentials_without_accessing_the_store() {
        let credential = resolve_with(Some("figd_environment".into()), || {
            panic!("must not access the credential store with an environment override")
        })
        .unwrap()
        .unwrap();
        assert_eq!(credential.value, "figd_environment");
        assert_eq!(credential.source, "environment");
        assert!(
            resolve_with(Some("".into()), || panic!("offline override"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_input_is_rejected_without_replacing_a_saved_token() {
        let entry = mock_entry();
        save_to(&entry, "figd_original").unwrap();
        for invalid in ["", " \n", "figd_a\nfigd_b", "figd_\0secret", "figd_é"] {
            assert!(save_to(&entry, invalid).is_err());
        }
        assert!(save_to(&entry, &"x".repeat(MAX_TOKEN_BYTES + 1)).is_err());
        assert_eq!(read_saved(&entry).unwrap().unwrap(), "figd_original");
    }

    #[test]
    fn credential_store_failures_are_actionable_and_never_expose_backend_secrets() {
        let entry = mock_entry();
        let mock = entry
            .get_credential()
            .downcast_ref::<MockCredential>()
            .unwrap();
        mock.set_error(keyring::Error::PlatformFailure(Box::new(
            std::io::Error::other("figd_backend_secret"),
        )));
        let error = read_saved(&entry).unwrap_err();
        assert!(error.contains("credential store"));
        assert!(error.contains("FIGMA_TOKEN"));
        assert!(!error.contains("figd_backend_secret"));

        mock.set_error(keyring::Error::BadEncoding(b"figd_raw_secret".to_vec()));
        let error = read_saved(&entry).unwrap_err();
        assert!(!error.contains("figd_raw_secret"));

        mock.set_error(keyring::Error::NoStorageAccess(Box::new(
            std::io::Error::other("locked"),
        )));
        assert!(save_to(&entry, "figd_new").unwrap_err().contains("save"));
        mock.set_error(keyring::Error::NoStorageAccess(Box::new(
            std::io::Error::other("locked"),
        )));
        assert!(remove_from(&entry).unwrap_err().contains("remove"));
    }

    #[test]
    fn invalid_environment_never_falls_back_to_saved_credentials() {
        assert!(
            resolve_with(Some("figd_a\nfigd_b".into()), || panic!("invalid override")).is_err()
        );
        use std::os::unix::ffi::OsStringExt;
        assert!(
            resolve_with(Some(OsString::from_vec(vec![0xff])), || {
                panic!("invalid encoding")
            })
            .is_err()
        );
    }

    #[test]
    #[ignore = "requires an unlocked native credential store; uses a disposable synthetic entry"]
    fn native_credential_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let service = format!(
            "figmog-test-{}",
            dir.path().file_name().unwrap().to_string_lossy()
        );
        struct Cleanup(Entry);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.delete_credential();
            }
        }
        let entry = Cleanup(Entry::new(&service, ACCOUNT).unwrap());
        save_to(&entry.0, "figd_synthetic_test_only").unwrap();
        assert_eq!(
            read_saved(&entry.0).unwrap().unwrap(),
            "figd_synthetic_test_only"
        );
        assert!(remove_from(&entry.0).unwrap());
        assert!(read_saved(&entry.0).unwrap().is_none());
    }
}
