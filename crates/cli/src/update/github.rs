use std::time::Duration;

use imagegen_bridge::core::{BridgeError, ErrorCode};
use reqwest::Url;
use semver::Version;
use serde::{Deserialize, Serialize};

pub(super) const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_API_BASE: &str = "https://api.github.com/repos/Crimsab/imagegen-bridge";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct Release {
    pub tag_name: String,
    pub html_url: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub(super) fn version(&self) -> Result<Version, BridgeError> {
        Version::parse(self.tag_name.trim_start_matches('v'))
            .map_err(|_| protocol("latest GitHub release has an invalid semantic version tag"))
    }

    pub(super) fn asset(&self, name: &str) -> Result<&Asset, BridgeError> {
        self.assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or_else(|| {
                protocol(format!(
                    "latest release does not contain required asset {name}"
                ))
            })
    }
}

pub(super) async fn latest(passive: bool) -> Result<Release, BridgeError> {
    let base = api_base()?;
    let url = base
        .join("releases/latest")
        .map_err(|_| BridgeError::new(ErrorCode::Configuration, "invalid update API base URL"))?;
    let timeout = if passive {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(15)
    };
    let connect_timeout = if passive {
        Duration::from_millis(750)
    } else {
        Duration::from_secs(5)
    };
    let client = reqwest::Client::builder()
        .user_agent(format!("imagegen-bridge/{}", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|_| upstream("could not initialize the update client"))?;
    client
        .get(url)
        .send()
        .await
        .map_err(|_| upstream("could not reach GitHub Releases"))?
        .error_for_status()
        .map_err(|_| upstream("GitHub Releases returned an unsuccessful response"))?
        .json::<Release>()
        .await
        .map_err(|_| protocol("GitHub Releases returned an invalid response"))
}

pub(super) async fn download(asset: &Asset) -> Result<Vec<u8>, BridgeError> {
    if asset.size > MAX_DOWNLOAD_BYTES {
        return Err(protocol("release asset exceeds the update size limit"));
    }
    let url = Url::parse(&asset.browser_download_url)
        .map_err(|_| protocol("release asset URL is invalid"))?;
    validate_url(&url)?;
    let client = reqwest::Client::builder()
        .user_agent(format!("imagegen-bridge/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| upstream("could not initialize the update client"))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| upstream("release asset download failed"))?
        .error_for_status()
        .map_err(|_| upstream("release asset download returned an unsuccessful response"))?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_DOWNLOAD_BYTES)
    {
        return Err(protocol("release asset exceeds the update size limit"));
    }
    let mut bytes = Vec::with_capacity(asset.size.min(MAX_DOWNLOAD_BYTES) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| upstream("release asset download was interrupted"))?
    {
        if bytes.len().saturating_add(chunk.len()) as u64 > MAX_DOWNLOAD_BYTES {
            return Err(protocol("release asset exceeds the update size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() as u64 != asset.size {
        return Err(protocol("release asset size does not match its manifest"));
    }
    Ok(bytes)
}

fn api_base() -> Result<Url, BridgeError> {
    let raw = std::env::var("IMAGEGEN_BRIDGE_UPDATE_API_BASE")
        .unwrap_or_else(|_| DEFAULT_API_BASE.to_owned());
    let mut url = Url::parse(raw.trim_end_matches('/'))
        .map_err(|_| BridgeError::new(ErrorCode::Configuration, "invalid update API base URL"))?;
    validate_url(&url)?;
    let path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

fn validate_url(url: &Url) -> Result<(), BridgeError> {
    let local_http = url.scheme() == "http"
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if url.scheme() != "https" && !local_http {
        return Err(BridgeError::new(
            ErrorCode::Configuration,
            "update URLs must use HTTPS (HTTP is accepted only for localhost tests)",
        ));
    }
    Ok(())
}

fn upstream(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Upstream, message).retryable(true)
}

fn protocol(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Protocol, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prefixed_semver() -> Result<(), Box<dyn std::error::Error>> {
        let release = Release {
            tag_name: "v1.2.3".into(),
            html_url: String::new(),
            assets: Vec::new(),
        };
        assert_eq!(release.version()?, Version::new(1, 2, 3));
        Ok(())
    }
}
