//! Shared HTTP configuration for model artifact downloads.

use std::net::{IpAddr, Ipv4Addr};

const USER_AGENT: &str = concat!("zvec-grep/", env!("CARGO_PKG_VERSION"));

pub(crate) struct ArtifactHttpClient {
    default: reqwest::Client,
    ipv4: reqwest::Client,
}

impl ArtifactHttpClient {
    pub(crate) fn new() -> Self {
        Self::build(reqwest::Client::builder(), reqwest::Client::builder())
    }

    fn build(default: reqwest::ClientBuilder, ipv4: reqwest::ClientBuilder) -> Self {
        Self {
            default: default
                .user_agent(USER_AGENT)
                .build()
                .expect("initialize model download HTTP client"),
            // Binding to an IPv4 address also limits DNS results to IPv4. Leave
            // normal proxy discovery, redirects, and TLS verification intact.
            ipv4: ipv4
                .user_agent(USER_AGENT)
                .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
                .build()
                .expect("initialize IPv4 model download HTTP client"),
        }
    }

    pub(super) fn default_client(&self) -> &reqwest::Client {
        &self.default
    }

    pub(super) fn ipv4_client(&self) -> &reqwest::Client {
        &self.ipv4
    }

    #[cfg(test)]
    pub(super) fn with_builders(
        default: reqwest::ClientBuilder,
        ipv4: reqwest::ClientBuilder,
    ) -> Self {
        Self::build(default, ipv4)
    }
}

#[cfg(test)]
mod tests;
