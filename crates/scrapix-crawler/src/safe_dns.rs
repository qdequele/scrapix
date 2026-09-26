//! DNS resolution for reqwest that goes through the caching resolver and refuses
//! non-public addresses (SSRF protection that also covers redirects and DNS rebinding
//! at connect time).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::dns::CachingDnsResolver;

/// Returns `true` if `ip` is a publicly routable address — i.e. not loopback,
/// private, link-local, documentation, unspecified, multicast, broadcast,
/// CGNAT (100.64.0.0/10), unique-local (fc00::/7), or an IPv4-mapped IPv6
/// address wrapping any of the above.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (o[0] == 100 && (o[1] & 0b1100_0000) == 64) // 100.64.0.0/10 CGNAT
                || o[0] == 0)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // unique local
                || (seg0 & 0xffc0) == 0xfe80) // link local
        }
    }
}

/// A `reqwest::dns::Resolve` implementation that resolves through the
/// caching DNS resolver (so reqwest and the crawler's own cache don't
/// double-resolve) and refuses to hand back non-public addresses unless
/// `allow_private` is set.
pub(crate) struct SafeResolver {
    pub cache: Option<Arc<CachingDnsResolver>>,
    pub allow_private: bool,
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let cache = self.cache.clone();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let host = name.as_str().to_string();
            let ips: Vec<IpAddr> = match cache {
                Some(c) => c
                    .resolve(&host)
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?,
                None => tokio::net::lookup_host((host.as_str(), 0))
                    .await?
                    .map(|sa| sa.ip())
                    .collect(),
            };
            let allowed: Vec<SocketAddr> = ips
                .into_iter()
                .filter(|ip| allow_private || is_public_ip(*ip))
                .map(|ip| SocketAddr::new(ip, 0))
                .collect();
            if allowed.is_empty() {
                return Err(format!("{host} resolves only to non-public addresses").into());
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_ips() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(
                !is_public_ip(ip.parse::<IpAddr>().unwrap()),
                "{ip} must be non-public"
            );
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(
                is_public_ip(ip.parse::<IpAddr>().unwrap()),
                "{ip} must be public"
            );
        }
    }
}
