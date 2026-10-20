//! Where a request comes from (this computer, the home network or the tunnel), worked out from the connection
//! and its headers; that decides what it needs to get in.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::connect_info::Connected;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::page::page_lang;
use crate::i18n::Msg;
use crate::sys::net;

/// Both ends of a connection, and for one from this computer the program that opened it. Worked out once, when
/// the connection is accepted.
#[derive(Clone, Copy, Debug)]
pub(super) struct Conn {
    pub(super) peer: SocketAddr,
    /// This computer's address the connection reached
    local: SocketAddr,
    client: Option<Client>,
}

/// The program on this computer that opened a connection.
#[derive(Clone, Copy, Debug)]
struct Client {
    pid: u32,
    /// Runs in StrCu's Windows session (the same user at this screen)
    same_session: bool,
}

impl Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>> for Conn {
    fn connect_info(s: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        let peer = *s.remote_addr();
        let local = s.io().local_addr().unwrap_or(peer);
        let pid = if peer.ip().is_loopback() { net::client_pid(peer, local) } else { None };
        Conn { peer, local, client: pid.map(|pid| Client { pid, same_session: net::same_session(pid) }) }
    }
}

/// Where a request comes from; that decides what it needs to get in.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Via {
    /// A program in this Windows session on this computer. It signs in like everything else (the panel can
    /// do what the computer can do, which is more than the person running that program may); what it still
    /// decides is the name in the log, the brake wrong passwords count against, and that a fingerprint is
    /// offered on `localhost`.
    Local,
    /// The home network: sign-in needed
    Home(IpAddr),
    /// Through the tunnel: Cloudflare Access, then sign-in
    Remote,
}

impl Via {
    /// Where an event came from, for the log.
    pub(super) fn source(&self, h: &HeaderMap) -> Msg {
        match self {
            Via::Local => Msg::new("src.local"),
            Via::Home(ip) => Msg::new("src.home").with("ip", ip.to_string()),
            Via::Remote => Msg::new("src.remote").with("ip", cf_ip(h)),
        }
    }

    /// Wrong passwords are counted per source, so someone on the home network cannot lock out remote sign-in.
    pub(super) fn key(&self, h: &HeaderMap) -> String {
        match self {
            Via::Local => "local".into(),
            Via::Home(ip) => ip.to_string(),
            Via::Remote => format!("remote {}", cf_ip(h)),
        }
    }

    pub(super) fn name(&self) -> &'static str {
        match self {
            Via::Local => "local",
            Via::Home(_) => "home",
            Via::Remote => "remote",
        }
    }
}

/// The visitor's address as Cloudflare reports it; trusted only once Access has verified the request.
pub(super) fn cf_ip(h: &HeaderMap) -> String {
    h.get("cf-connecting-ip").and_then(|v| v.to_str().ok()).unwrap_or("?").to_string()
}

/// The Host header without the port, in lower case.
pub(super) fn host_name(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::HOST)?.to_str().ok()?;
    let name = match v.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) && !name.ends_with(':') => name,
        _ => v,
    };
    Some(name.to_ascii_lowercase())
}

/// Did the request come through cloudflared? Cloudflare adds Cf-Ray to every request and Host is the
/// public address. Anything uncertain counts as tunnel.
fn via_tunnel(h: &HeaderMap) -> bool {
    let local_host = host_name(h).is_some_and(|n| matches!(n.as_str(), "127.0.0.1" | "localhost" | "[::1]"));
    !local_host || h.contains_key("cf-ray") || h.contains_key("cf-connecting-ip")
}

/// From the home network the address must be one of this computer's: an IP address or the computer's name
/// (`pc`). Any other name means a site pointed its own name at this computer (DNS rebinding).
fn home_host(h: &HeaderMap, pc: &str) -> bool {
    let Some(name) = host_name(h) else { return false };
    if name.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok() {
        return true;
    }
    let first = name.split('.').next().unwrap_or_default();
    let suffix = &name[first.len()..];
    !pc.is_empty() && first.eq_ignore_ascii_case(pc) && matches!(suffix, "" | ".local" | ".lan" | ".home" | ".home.arpa" | ".internal")
}

/// Where a request comes from, or why it is turned away. `tunnel_pid` is cloudflared's process id, `private`
/// this computer's addresses on networks Windows counts as private, `pc` the computer's name.
fn classify(c: &Conn, h: &HeaderMap, tunnel_pid: u32, private: &[Ipv4Addr], pc: &str) -> std::result::Result<Via, Msg> {
    if c.peer.ip().is_loopback() {
        if c.client.is_some_and(|cl| cl.pid == tunnel_pid) || via_tunnel(h) {
            return Ok(Via::Remote);
        }
        return match c.client {
            Some(cl) if cl.same_session => Ok(Via::Local),
            _ => Err(Msg::new("err.other_session")),
        };
    }
    let ip = c.peer.ip();
    let reached = match c.local.ip() {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
    };
    let Some(reached) = reached.filter(|_| net::is_home_ip(ip)) else { return Err(Msg::new("err.not_home")) };
    if !private.contains(&reached) {
        return Err(Msg::new("err.public_network"));
    }
    if !home_host(h, pc) {
        return Err(Msg::new("err.unknown_host"));
    }
    Ok(Via::Home(ip))
}

impl AppState {
    /// This computer's addresses on private networks; read again after 15 seconds.
    async fn private_ips(&self) -> Arc<Vec<Ipv4Addr>> {
        let mut c = self.private.lock().await;
        if let Some((at, ips)) = c.as_ref()
            && at.elapsed() < Duration::from_secs(15)
        {
            return ips.clone();
        }
        let ips = Arc::new(tokio::task::spawn_blocking(net::private_ipv4).await.unwrap_or_default());
        *c = Some((Instant::now(), ips.clone()));
        ips
    }

    pub(super) async fn classify(&self, c: &Conn, h: &HeaderMap) -> std::result::Result<Via, Msg> {
        let private = if c.peer.ip().is_loopback() { Arc::default() } else { self.private_ips().await };
        let pc = std::env::var("COMPUTERNAME").unwrap_or_default();
        classify(c, h, self.tunnel_pid.load(Ordering::Relaxed), &private, &pc)
    }

    /// Turns a request away, saying why in the visitor's language. Logged once a minute per address, so a
    /// scanner cannot flood the log.
    pub(super) fn deny(&self, ip: String, reason: Msg, h: &HeaderMap) -> Response {
        let mut seen = self.denied.lock().unwrap();
        seen.retain(|_, t| t.elapsed() < Duration::from_secs(60));
        if !seen.contains_key(&ip) {
            seen.insert(ip.clone(), Instant::now());
            self.log.add(Msg::new("ev.access_denied").with("ip", ip).with("reason", reason.clone()), false);
        }
        let text = format!("StrCu: {}", page_lang(h).render(&reason));
        (StatusCode::FORBIDDEN, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn conn(peer: &str, local: &str, client: Option<(u32, bool)>) -> Conn {
        Conn {
            peer: peer.parse().unwrap(),
            local: local.parse().unwrap(),
            client: client.map(|(pid, same_session)| Client { pid, same_session }),
        }
    }

    #[test]
    fn where_requests_come_from() {
        let private: Vec<Ipv4Addr> = vec!["192.168.1.24".parse().unwrap()];
        let at = |c: &Conn, h: &[(&'static str, &str)]| classify(c, &headers(h), 4242, &private, "DESKTOP-1");
        let local_host = [("host", "localhost:8765")];

        // This computer: the same Windows session gets in, another user's session does not
        let mine = conn("127.0.0.1:50000", "127.0.0.1:8765", Some((100, true)));
        assert_eq!(at(&mine, &local_host), Ok(Via::Local));
        let theirs = conn("127.0.0.1:50001", "127.0.0.1:8765", Some((200, false)));
        assert_eq!(at(&theirs, &local_host).unwrap_err().code, "err.other_session");
        let unknown = conn("127.0.0.1:50002", "127.0.0.1:8765", None);
        assert_eq!(at(&unknown, &local_host).unwrap_err().code, "err.other_session");

        // cloudflared always counts as remote, with or without Cloudflare's headers
        let tunnel = conn("127.0.0.1:50003", "127.0.0.1:8765", Some((4242, true)));
        assert_eq!(at(&tunnel, &local_host), Ok(Via::Remote));
        assert_eq!(at(&mine, &[("host", "localhost:8765"), ("cf-ray", "x")]), Ok(Via::Remote));
        // A site that points its own name at 127.0.0.1 is treated as remote, so it needs Access
        assert_eq!(at(&mine, &[("host", "evil.example:8765")]), Ok(Via::Remote));

        // Home network: a private address, on a private network, typed as an IP or the computer's name
        let phone = conn("192.168.1.50:41000", "192.168.1.24:8765", None);
        let home = Ok(Via::Home("192.168.1.50".parse().unwrap()));
        assert_eq!(at(&phone, &[("host", "192.168.1.24:8765")]), home);
        assert_eq!(at(&phone, &[("host", "desktop-1.local:8765")]), home);
        assert_eq!(at(&phone, &[("host", "DESKTOP-1:8765")]), home);
        assert_eq!(at(&phone, &[("host", "evil.example:8765")]).unwrap_err().code, "err.unknown_host");
        assert_eq!(at(&phone, &[("host", "desktop-1.evil.example")]).unwrap_err().code, "err.unknown_host");
        // Cloudflare's headers from the home network change nothing
        assert_eq!(at(&phone, &[("host", "192.168.1.24:8765"), ("cf-connecting-ip", "1.2.3.4")]), home);
        // A network Windows counts as public (café): turned away
        let cafe = conn("10.0.0.9:41000", "10.0.0.5:8765", None);
        assert_eq!(at(&cafe, &[("host", "10.0.0.5:8765")]).unwrap_err().code, "err.public_network");
        // Not a home network address: turned away
        let outside = conn("203.0.113.7:41000", "192.168.1.24:8765", None);
        assert_eq!(at(&outside, &[("host", "192.168.1.24:8765")]).unwrap_err().code, "err.not_home");
        let tailscale = conn("100.101.102.103:41000", "192.168.1.24:8765", None);
        assert_eq!(at(&tailscale, &[("host", "192.168.1.24:8765")]).unwrap_err().code, "err.not_home");
    }
}
