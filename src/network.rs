//! HTTPS transport for explicitly approved public asset/service hosts.
use anyhow::{Result, anyhow, ensure};
use reqwest::blocking::Client;
use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use url::Url;
static DNS_WORKERS: AtomicUsize = AtomicUsize::new(0);
struct DnsPermit;
impl Drop for DnsPermit {
    fn drop(&mut self) {
        DNS_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}
pub fn approved_url(address: &str, exact: &[&str], families: &[&str]) -> Result<Url> {
    let mut url = Url::parse(address).map_err(|_| anyhow!("资源地址无效"))?;
    if url.scheme() == "http" {
        url.set_scheme("https")
            .map_err(|_| anyhow!("资源协议无效"))?;
    }
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
            && url.fragment().is_none()
            && address.len() <= 4096,
        "仅允许标准HTTPS资源地址"
    );
    let host = url.host_str().ok_or_else(|| anyhow!("资源主机缺失"))?;
    ensure!(
        exact.contains(&host)
            || families.iter().any(|family| host == *family
                || host
                    .strip_suffix(family)
                    .is_some_and(|prefix| prefix.ends_with('.'))),
        "资源主机未获批准"
    );
    Ok(url)
}
fn routable(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
                return true;
            }
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_multicast()
                && !ip.is_unspecified()
                && !ip.is_broadcast()
                && !ip.is_documentation()
                && o[0] != 0
                && o[0] < 224
                && !(o[0] == 100 && (64..128).contains(&o[1]))
                && !(o[0] == 192 && o[1] == 0)
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && s[1] == 0xdb8)
                && ip.to_ipv4_mapped().is_none()
        }
    }
}
#[allow(deprecated)] // Preserve compatibility before Atomic::try_update's rename.
pub fn client(url: &Url, exact: &[&str], families: &[&str], timeout: Duration) -> Result<Client> {
    let url = approved_url(url.as_str(), exact, families)?;
    let host = url.host_str().unwrap().to_owned();
    DNS_WORKERS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < 8).then_some(count + 1)
        })
        .map_err(|_| anyhow!("资源解析任务繁忙，请稍后重试"))?;
    let permit = DnsPermit;
    let query = host.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("approved-dns".into())
        .spawn(move || {
            let _permit = permit;
            let result = (query.as_str(), 443)
                .to_socket_addrs()
                .map(|items| items.collect::<Vec<_>>());
            let _ = tx.send(result);
        })
        .map_err(|_| anyhow!("无法安排地址解析"))?;
    let mut addresses: Vec<SocketAddr> = rx
        .recv_timeout(Duration::from_secs(4))
        .map_err(|_| anyhow!("资源地址解析超时"))?
        .map_err(|_| anyhow!("资源地址解析失败"))?
        .into_iter()
        .filter(|a| routable(a.ip()))
        .collect();
    ensure!(!addresses.is_empty(), "资源地址未通过公网校验");
    addresses.sort_by_key(|address| !address.is_ipv4());
    Client::builder()
        .no_proxy()
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .resolve_to_addrs(&host, &addresses)
        .connect_timeout(timeout)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| anyhow!("无法准备资源连接"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hosts_ports_and_credentials_are_scoped() {
        let allowed = ["hdslb.com"];
        assert!(
            approved_url("http://i0.hdslb.com/a.png", &[], &allowed)
                .unwrap()
                .as_str()
                .starts_with("https:")
        );
        for bad in [
            "https://hdslb.com.attacker.invalid/a",
            "https://127.0.0.1/a",
            "https://u:p@i0.hdslb.com/a",
            "https://i0.hdslb.com:8080/a",
            "file:///private",
        ] {
            assert!(approved_url(bad, &[], &allowed).is_err());
        }
    }
}
