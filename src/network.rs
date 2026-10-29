use anyhow::{Context, Result, ensure};
use reqwest::{Client, Url};
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let b = v.octets();
            !v.is_private()
                && !v.is_loopback()
                && !v.is_link_local()
                && !v.is_unspecified()
                && !v.is_multicast()
                && b[0] != 0
                && b[0] < 224
                && !(b[0] == 100 && (64..=127).contains(&b[1]))
                && !(b[0] == 192 && b[1] == 0)
                && !(b[0] == 198 && (b[1] == 18 || b[1] == 19))
                && !v.is_documentation()
        }
        IpAddr::V6(v) => v
            .to_ipv4_mapped()
            .map(|x| public_ip(IpAddr::V4(x)))
            .unwrap_or_else(|| {
                !v.is_loopback()
                    && !v.is_unspecified()
                    && !v.is_multicast()
                    && !v.is_unique_local()
                    && !v.is_unicast_link_local()
                    && (v.segments()[0] & 0xe000) == 0x2000
                    && v.segments()[0] != 0x2002
                    && !(v.segments()[0] == 0x2001 && matches!(v.segments()[1], 0 | 2 | 0xdb8))
            }),
    }
}
pub async fn client(raw: &str, config: &crate::config::Config) -> Result<(Client, Url)> {
    ensure!(raw.len() <= 8192, "URL too long");
    let url = Url::parse(raw)?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none(),
        "Only plain HTTP(S) URLs are supported"
    );
    let host = url.host_str().context("URL host missing")?;
    let port = url.port_or_known_default().context("URL port missing")?;
    let allowed = config
        .downloads
        .allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host));
    let addresses: Vec<SocketAddr> = if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>()
    {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::lookup_host((host, port)),
        )
        .await??
        .collect()
    };
    ensure!(
        !addresses.is_empty() && addresses.len() <= 32,
        "Invalid DNS response"
    );
    ensure!(
        allowed || addresses.iter().all(|a| public_ip(a.ip())),
        "Private or reserved network destinations are blocked"
    );
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(3600))
        .build()?;
    Ok((client, url))
}

pub async fn get(raw: &str, config: &crate::config::Config) -> Result<(reqwest::Response, Url)> {
    let mut target = raw.to_owned();
    for _ in 0..=5 {
        let (client, url) = client(&target, config).await?;
        let response = client.get(url.clone()).send().await?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("Redirect location missing")?
                .to_str()?;
            let next = url.join(location)?;
            ensure!(
                url.scheme() != "https" || next.scheme() == "https",
                "TLS downgrade rejected"
            );
            target = next.to_string();
            continue;
        }
        return Ok((response.error_for_status()?, url));
    }
    anyhow::bail!("Too many download redirects")
}
