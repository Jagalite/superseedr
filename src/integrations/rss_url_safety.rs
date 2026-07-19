// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use reqwest::Url;
use std::net::IpAddr;

use crate::networking::runtime::NetworkLease;

fn ip_is_safe(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local())
        }
    }
}

fn resolved_ips_are_safe<I>(ips: I) -> bool
where
    I: IntoIterator<Item = IpAddr>,
{
    let mut saw_any = false;
    for ip in ips {
        saw_any = true;
        if !ip_is_safe(ip) {
            return false;
        }
    }
    saw_any
}

pub(crate) async fn is_safe_rss_item_url(network_lease: &NetworkLease, value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return false;
    }

    let host = match url.host_str() {
        Some(host) => host,
        None => return false,
    };
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    let normalized_host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .to_string();
    if let Ok(ip) = normalized_host.parse::<IpAddr>() {
        return ip_is_safe(ip);
    }

    let Some(port) = url.port_or_known_default() else {
        return false;
    };
    let lookup = network_lease.resolve(&normalized_host, port).await;
    match lookup {
        Ok(addrs) => resolved_ips_are_safe(addrs.into_iter().map(|a| a.ip())),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{ip_is_safe, is_safe_rss_item_url, resolved_ips_are_safe};
    use crate::networking::runtime::NetworkSupervisor;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[tokio::test]
    async fn rss_item_url_guard_rejects_localhost_and_private_literal_ips() {
        let (network_handle, _task) = NetworkSupervisor::spawn_unrestricted().unwrap();
        let network_lease = network_handle.try_lease().unwrap();
        assert!(!is_safe_rss_item_url(&network_lease, "http://localhost/file.torrent").await);
        assert!(!is_safe_rss_item_url(&network_lease, "https://127.0.0.1/file.torrent").await);
        assert!(!is_safe_rss_item_url(&network_lease, "https://192.168.10.5/file.torrent").await);
        assert!(!is_safe_rss_item_url(&network_lease, "https://[::1]/file.torrent").await);
    }

    #[test]
    fn ip_guard_rejects_private_and_accepts_public_literals() {
        assert!(!ip_is_safe(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(!ip_is_safe(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20))));
        assert!(!ip_is_safe(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(ip_is_safe(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    #[test]
    fn resolved_host_guard_rejects_any_private_result() {
        let mixed = vec![
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 8)),
        ];
        assert!(!resolved_ips_are_safe(mixed));
    }

    #[test]
    fn resolved_host_guard_accepts_all_public_results() {
        let public = vec![
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        ];
        assert!(resolved_ips_are_safe(public));
    }
}
