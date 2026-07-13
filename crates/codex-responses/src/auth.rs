//! Bounded, redaction-safe Codex OAuth credential discovery.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use imagegen_bridge_core::{BridgeError, ErrorCode};
use secrecy::SecretString;
use serde::Deserialize;

const MAX_AUTH_FILE_BYTES: u64 = 1024 * 1024;

/// OAuth credentials required by the private Codex Responses route.
pub struct CodexOAuthCredentials {
    /// OAuth bearer access token.
    pub access_token: SecretString,
    /// `ChatGPT` workspace/account identifier when present.
    pub account_id: Option<SecretString>,
}

impl std::fmt::Debug for CodexOAuthCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexOAuthCredentials")
            .field("access_token", &"[REDACTED]")
            .field(
                "account_id",
                &self.account_id.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Reloadable credential source; providers call it for each upstream request.
#[async_trait]
pub trait CodexCredentialSource: Send + Sync {
    /// Loads a current credential snapshot.
    async fn load(&self) -> Result<CodexOAuthCredentials, BridgeError>;
}

/// Reads the Codex CLI `auth.json` file without persisting another token copy.
#[derive(Debug, Clone)]
pub struct CodexAuthFile {
    path: PathBuf,
}

impl CodexAuthFile {
    /// Uses `$CODEX_HOME/auth.json` or `~/.codex/auth.json`.
    pub fn discover() -> Result<Self, BridgeError> {
        let root = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
            .ok_or_else(|| auth_error("could not determine the Codex home directory"))?;
        Ok(Self::new(root.join("auth.json")))
    }

    /// Uses an explicit auth file path.
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Returns the configured path for diagnostics without reading its contents.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl CodexCredentialSource for CodexAuthFile {
    async fn load(&self) -> Result<CodexOAuthCredentials, BridgeError> {
        let metadata = tokio::fs::metadata(&self.path)
            .await
            .map_err(|_| auth_error("Codex OAuth credentials were not found"))?;
        if !metadata.is_file() || metadata.len() > MAX_AUTH_FILE_BYTES {
            return Err(auth_error("Codex auth file is not a bounded regular file"));
        }
        check_permissions(&metadata)?;
        let contents = tokio::fs::read(&self.path)
            .await
            .map_err(|_| auth_error("Codex OAuth credentials could not be read"))?;
        if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_AUTH_FILE_BYTES {
            return Err(auth_error("Codex auth file exceeds the byte limit"));
        }
        let auth: AuthFile = serde_json::from_slice(&contents)
            .map_err(|_| auth_error("Codex auth file has an unsupported shape"))?;
        if auth.auth_mode.as_deref() != Some("chatgpt") {
            return Err(auth_error("Codex is not logged in with ChatGPT OAuth"));
        }
        let tokens = auth
            .tokens
            .ok_or_else(|| auth_error("Codex auth file contains no OAuth tokens"))?;
        if tokens.access_token.trim().is_empty() {
            return Err(auth_error("Codex OAuth access token is empty"));
        }
        Ok(CodexOAuthCredentials {
            access_token: SecretString::from(tokens.access_token),
            account_id: tokens
                .account_id
                .filter(|value| !value.trim().is_empty())
                .map(SecretString::from),
        })
    }
}

#[derive(Deserialize)]
struct AuthFile {
    auth_mode: Option<String>,
    tokens: Option<AuthTokens>,
}

#[derive(Deserialize)]
struct AuthTokens {
    access_token: String,
    account_id: Option<String>,
}

#[cfg(unix)]
fn check_permissions(metadata: &std::fs::Metadata) -> Result<(), BridgeError> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.mode() & 0o022 != 0 {
        return Err(auth_error(
            "Codex auth file must not be writable by group or other users",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_metadata: &std::fs::Metadata) -> Result<(), BridgeError> {
    Ok(())
}

fn auth_error(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Authentication, message).with_provider("codex-responses")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use secrecy::ExposeSecret as _;

    use super::*;

    #[tokio::test]
    async fn loads_chatgpt_tokens_without_exposing_them_in_debug() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        tokio::fs::write(
            &path,
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"secret-access","account_id":"secret-account"}}"#,
        )
        .await
        .unwrap();
        let credentials = CodexAuthFile::new(path).load().await.unwrap();
        assert_eq!(credentials.access_token.expose_secret(), "secret-access");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("secret-access"));
        assert!(!debug.contains("secret-account"));
    }

    #[tokio::test]
    async fn rejects_api_key_and_missing_oauth_shapes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth.json");
        tokio::fs::write(
            &path,
            br#"{"auth_mode":"apikey","OPENAI_API_KEY":"secret"}"#,
        )
        .await
        .unwrap();
        let error = CodexAuthFile::new(path).load().await.unwrap_err();
        assert_eq!(error.code, ErrorCode::Authentication);
        assert!(!format!("{error:?}").contains("secret"));
    }
}
