//! DNS resolution for reqwest that goes through the caching resolver and refuses
//! non-public addresses (SSRF protection that also covers redirects and DNS rebinding
//! at connect time).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::dns::CachingDnsResolver;

/// Returns `true` if `ip` is a publicly routable address — i.e. not loopback,
/// private, link-local, documentation, unspecified, multicast, broadcast,
/// CGNAT (100.64.0.0/10), benchmarking (198.18.0.0/15), reserved
/// (240.0.0.0/4), IETF protocol assignments (192.0.0.0/24), unique-local
/// (fc00::/7), deprecated site-local (fec0::/10), documentation
/// (2001:db8::/32), 6to4 (2002::/16, refused outright regardless of the
/// embedded IPv4), NAT64 (64:ff9b::/96, refused outright), an IPv4-compatible
/// IPv6 address (`::a.b.c.d`), or an IPv4-mapped IPv6 address wrapping any of
/// the above.
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
                || o[0] == 0
                || (o[0] == 198 && (o[1] & 0b1111_1110) == 18) // 198.18.0.0/15 benchmarking
                || (o[0] & 0b1111_0000) == 240 // 240.0.0.0/4 reserved (incl. 255.255.255.255)
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)) // 192.0.0.0/24 IETF protocol assignments
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let segs = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (segs[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (segs[0] & 0xffc0) == 0xfe80 // link local fe80::/10
                || (segs[0] & 0xffc0) == 0xfec0 // deprecated site-local fec0::/10
                || (segs[0] == 0x2001 && segs[1] == 0x0db8) // documentation 2001:db8::/32
                || segs[0] == 0x2002 // 6to4 2002::/16 — refused outright
                || (segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2..6] == [0, 0, 0, 0]) // NAT64 64:ff9b::/96 — refused outright
                || (segs[..6] == [0, 0, 0, 0, 0, 0] && (segs[6] != 0 || segs[7] != 0)))
            // IPv4-compatible ::a.b.c.d
        }
    }
}

/// Error returned by [`SafeResolver`] when a hostname resolves only to
/// non-public addresses (SSRF refusal). A typed error — rather than a bare
/// `String` boxed into `reqwest::dns::Resolving`'s `BoxError` — lets
/// `HttpFetcher::map_send_error` recover the *original* refusal message by
/// walking `std::error::Error::source()` and `downcast_ref`-ing onto this
/// type, instead of pattern-matching text out of `reqwest::Error`'s `Debug`
/// output (which dumps the entire error chain, not just this message).
#[derive(Debug)]
pub(crate) struct NonPublicAddress {
    pub host: String,
}

impl std::fmt::Display for NonPublicAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} resolves only to non-public addresses", self.host)
    }
}

impl std::error::Error for NonPublicAddress {}

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
                return Err(
                    Box::new(NonPublicAddress { host }) as Box<dyn std::error::Error + Send + Sync>
                );
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

/// Resolve `host` the same way [`SafeResolver`] does (through the caching
/// resolver when given, else `tokio::net::lookup_host`).
async fn resolve_host(
    cache: Option<&Arc<CachingDnsResolver>>,
    host: &str,
) -> scrapix_core::Result<Vec<IpAddr>> {
    match cache {
        Some(c) => c.resolve(host).await,
        None => Ok(tokio::net::lookup_host((host, 0))
            .await
            .map_err(|e| {
                scrapix_core::ScrapixError::Connection(format!("DNS lookup failed for {host}: {e}"))
            })?
            .map(|sa| sa.ip())
            .collect()),
    }
}

/// Validate a per-job proxy URL before any request goes through it.
///
/// hyper connects to an IP-literal proxy without consulting the client's
/// resolver, so a proxy such as `http://169.254.169.254:80` would bypass
/// [`SafeResolver`] entirely. Rules (unless `allow_private`):
/// - the scheme must be `http` or `https` (the only proxy schemes this
///   build of reqwest supports);
/// - a raw-IP host must be a public address;
/// - a hostname must resolve to at least one public address, and to no
///   non-public one.
///
/// Refusals are `ScrapixError::Refused` (terminal, never retried).
pub async fn validate_proxy_url(
    proxy: &str,
    cache: Option<&Arc<CachingDnsResolver>>,
    allow_private: bool,
) -> scrapix_core::Result<()> {
    use scrapix_core::ScrapixError;

    let parsed = url::Url::parse(proxy)
        .map_err(|e| ScrapixError::Refused(format!("invalid proxy URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ScrapixError::Refused(format!(
            "unsupported proxy scheme '{}' (use http or https)",
            parsed.scheme()
        )));
    }
    if allow_private {
        return Ok(());
    }
    let ips = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => vec![IpAddr::V4(ip)],
        Some(url::Host::Ipv6(ip)) => vec![IpAddr::V6(ip)],
        Some(url::Host::Domain(host)) => resolve_host(cache, host).await?,
        None => return Err(ScrapixError::Refused("proxy URL has no host".into())),
    };
    if ips.is_empty() || ips.iter().any(|ip| !is_public_ip(*ip)) {
        return Err(ScrapixError::Refused(format!(
            "proxy {} resolves to a non-public address",
            parsed.host_str().unwrap_or_default()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proxy_validation() {
        use scrapix_core::ScrapixError;
        let refused = |r: scrapix_core::Result<()>| matches!(r, Err(ScrapixError::Refused(_)));
        assert!(refused(
            validate_proxy_url("http://169.254.169.254:80", None, false).await
        ));
        assert!(refused(
            validate_proxy_url("http://[::1]:3128", None, false).await
        ));
        assert!(refused(
            validate_proxy_url("http://localhost:3128", None, false).await
        ));
        assert!(refused(
            validate_proxy_url("socks5://1.1.1.1:1080", None, false).await
        ));
        assert!(refused(validate_proxy_url("not a url", None, false).await));
        assert!(validate_proxy_url("http://1.1.1.1:8080", None, false)
            .await
            .is_ok());
        // Opt-out (tests / self-hosting) allows private proxies.
        assert!(validate_proxy_url("http://127.0.0.1:3128", None, true)
            .await
            .is_ok());
    }

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
            // 198.18.0.0/15 benchmarking
            "198.18.0.1",
            "198.19.255.255",
            // 240.0.0.0/4 reserved
            "240.0.0.1",
            "255.255.255.254",
            // 192.0.0.0/24 IETF protocol assignments
            "192.0.0.1",
            // fec0::/10 deprecated site-local
            "fec0::1",
            // 2001:db8::/32 documentation
            "2001:db8::1",
            // 2002::/16 6to4 — refused outright even though it embeds a public IPv4
            "2002:0101:0101::1",
            // 64:ff9b::/96 NAT64 — refused outright even though it embeds a public IPv4
            "64:ff9b::0101:0101",
            // IPv4-compatible ::a.b.c.d (distinct from the ::ffff: mapped form)
            "::1.2.3.4",
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
