//! SSRF-resistant remote image fetching with DNS pinning and redirect checks.

use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use futures_util::StreamExt as _;
use imagegen_bridge_core::{BridgeError, ErrorCode};
use reqwest::{Client, StatusCode, Url, header};

use crate::{ImageLimits, LoadedImage, inspect_image};

/// Explicit policy controlling remote image access.
#[derive(Debug, Clone)]
pub struct RemoteInputPolicy {
    /// Whether remote inputs are enabled.
    pub enabled: bool,
    /// Exact lower-case hostnames permitted. Empty allows any public hostname.
    pub allowed_hosts: BTreeSet<String>,
    /// Allowed destination ports.
    pub allowed_ports: BTreeSet<u16>,
    /// Whether private, loopback, link-local, and reserved networks are allowed.
    pub allow_private_networks: bool,
    /// Maximum number of checked redirects.
    pub max_redirects: u8,
    /// Per-hop request timeout.
    pub timeout: Duration,
    /// Maximum URL bytes.
    pub max_url_bytes: usize,
}

impl Default for RemoteInputPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed_hosts: BTreeSet::new(),
            allowed_ports: BTreeSet::from([80, 443]),
            allow_private_networks: false,
            max_redirects: 3,
            timeout: Duration::from_secs(20),
            max_url_bytes: 8 * 1024,
        }
    }
}

/// Fetches remote images without following unchecked DNS or redirects.
#[derive(Debug, Clone)]
pub struct RemoteImageFetcher {
    policy: RemoteInputPolicy,
    limits: ImageLimits,
}

impl RemoteImageFetcher {
    /// Creates a fetcher. Disabled policy remains a valid fail-closed instance.
    #[must_use]
    pub const fn new(policy: RemoteInputPolicy, limits: ImageLimits) -> Self {
        Self { policy, limits }
    }

    /// Fetches and verifies one HTTP(S) image.
    pub async fn fetch(&self, value: &str) -> Result<LoadedImage, BridgeError> {
        if !self.policy.enabled {
            return Err(remote_error("remote image inputs are disabled"));
        }
        if value.len() > self.policy.max_url_bytes {
            return Err(remote_error(
                "remote image URL exceeds the configured limit",
            ));
        }
        let mut url = Url::parse(value).map_err(|_| remote_error("remote image URL is invalid"))?;
        for redirect_count in 0..=self.policy.max_redirects {
            self.validate_url(&url)?;
            let client = self.pinned_client(&url).await?;
            let response = client
                .get(url.clone())
                .header(header::ACCEPT, "image/png,image/jpeg,image/webp;q=0.9")
                .send()
                .await
                .map_err(|_| remote_error("remote image request failed"))?;

            if response.status().is_redirection() {
                if redirect_count == self.policy.max_redirects {
                    return Err(remote_error("remote image redirect limit exceeded"));
                }
                url = checked_redirect(&url, response.status(), response.headers())?;
                continue;
            }
            if !response.status().is_success() {
                return Err(remote_error(format!(
                    "remote image returned HTTP {}",
                    response.status().as_u16()
                )));
            }
            validate_content_headers(&response, self.limits)?;
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| remote_error("remote image body failed"))?;
                let new_len = bytes.len().checked_add(chunk.len()).ok_or_else(|| {
                    remote_error("remote image exceeds the configured byte limit")
                })?;
                if u64::try_from(new_len).unwrap_or(u64::MAX) > self.limits.max_encoded_bytes {
                    return Err(remote_error(
                        "remote image exceeds the configured byte limit",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            let metadata = inspect_image(&bytes, self.limits)?;
            return Ok(LoadedImage {
                bytes,
                metadata,
                filename: None,
            });
        }
        Err(remote_error("remote image redirect limit exceeded"))
    }

    fn validate_url(&self, url: &Url) -> Result<(), BridgeError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(remote_error("remote image URL must use http or https"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(remote_error(
                "remote image URL must not contain credentials",
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| remote_error("remote image URL has no hostname"))?
            .to_ascii_lowercase();
        if !self.policy.allowed_hosts.is_empty() && !self.policy.allowed_hosts.contains(&host) {
            return Err(remote_error("remote image hostname is not allowed"));
        }
        let port = url
            .port_or_known_default()
            .ok_or_else(|| remote_error("remote image URL has no usable port"))?;
        if !self.policy.allowed_ports.contains(&port) {
            return Err(remote_error("remote image port is not allowed"));
        }
        Ok(())
    }

    async fn pinned_client(&self, url: &Url) -> Result<Client, BridgeError> {
        let host = url
            .host_str()
            .ok_or_else(|| remote_error("remote image URL has no hostname"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| remote_error("remote image URL has no usable port"))?;
        let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| remote_error("remote image hostname could not be resolved"))?
            .collect();
        if addresses.is_empty() {
            return Err(remote_error(
                "remote image hostname resolved to no addresses",
            ));
        }
        if !self.policy.allow_private_networks
            && addresses.iter().any(|address| !is_public_ip(address.ip()))
        {
            return Err(remote_error(
                "remote image hostname resolves to a private or reserved address",
            ));
        }
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(self.policy.timeout)
            .user_agent(concat!("imagegen-bridge/", env!("CARGO_PKG_VERSION")))
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(|_| remote_error("remote image client could not be initialized"))
    }
}

fn checked_redirect(
    current: &Url,
    status: StatusCode,
    headers: &header::HeaderMap,
) -> Result<Url, BridgeError> {
    if !matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    ) {
        return Err(remote_error("unsupported redirect response"));
    }
    let location = headers
        .get(header::LOCATION)
        .ok_or_else(|| remote_error("redirect response has no location"))?
        .to_str()
        .map_err(|_| remote_error("redirect location is invalid"))?;
    current
        .join(location)
        .map_err(|_| remote_error("redirect location is invalid"))
}

fn validate_content_headers(
    response: &reqwest::Response,
    limits: ImageLimits,
) -> Result<(), BridgeError> {
    if response
        .content_length()
        .is_some_and(|length| length > limits.max_encoded_bytes)
    {
        return Err(remote_error(
            "remote image content length exceeds the configured limit",
        ));
    }
    if let Some(content_type) = response.headers().get(header::CONTENT_TYPE) {
        let content_type = content_type
            .to_str()
            .map_err(|_| remote_error("remote image content type is invalid"))?
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !matches!(
            content_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "application/octet-stream"
        ) {
            return Err(remote_error(
                "remote response does not declare a supported image content type",
            ));
        }
    }
    Ok(())
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return is_public_ipv4(mapped);
            }
            !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_multicast()
                && !address.is_unique_local()
                && !address.is_unicast_link_local()
                && !is_ipv6_documentation(address)
        }
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !address.is_private()
        && !address.is_loopback()
        && !address.is_link_local()
        && !address.is_broadcast()
        && !address.is_documentation()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !(octets[0] == 100 && (64..=127).contains(&octets[1]))
        && !(octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        && !(octets[0] == 198 && matches!(octets[1], 18 | 19))
        && octets[0] < 240
}

fn is_ipv6_documentation(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    segments[0] == 0x2001 && segments[1] == 0x0db8
}

fn remote_error(message: impl Into<String>) -> BridgeError {
    BridgeError::new(ErrorCode::Input, message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::inspect::test_png;

    async fn serve_once(body: Vec<u8>, content_type: &str) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let content_type = content_type.to_owned();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        address
    }

    #[tokio::test]
    async fn blocks_loopback_by_default() {
        let fetcher = RemoteImageFetcher::new(
            RemoteInputPolicy {
                enabled: true,
                ..RemoteInputPolicy::default()
            },
            ImageLimits::default(),
        );
        let error = fetcher
            .fetch("http://127.0.0.1/image.png")
            .await
            .unwrap_err();
        assert!(error.message.contains("private or reserved"));
    }

    #[tokio::test]
    async fn fetches_bounded_image_when_private_network_is_explicitly_allowed() {
        let body = test_png(2, 3);
        let address = serve_once(body.clone(), "image/png").await;
        let fetcher = RemoteImageFetcher::new(
            RemoteInputPolicy {
                enabled: true,
                allowed_ports: BTreeSet::from([address.port()]),
                allow_private_networks: true,
                ..RemoteInputPolicy::default()
            },
            ImageLimits::default(),
        );
        let image = fetcher
            .fetch(&format!("http://{address}/image.png"))
            .await
            .unwrap();
        assert_eq!(image.bytes, body);
        assert_eq!((image.metadata.width, image.metadata.height), (2, 3));
    }

    #[test]
    fn reserved_ranges_are_not_public() {
        for address in [
            "10.0.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "100.64.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
        ] {
            assert!(
                !is_public_ip(address.parse().unwrap()),
                "accepted {address}"
            );
        }
    }
}
