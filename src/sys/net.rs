//! The computer's network: its home network addresses, whether Windows counts each network as private,
//! which program opened a connection to the panel, and a port kept to the panel alone.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::windows::io::AsRawSocket;

use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    GetAdaptersAddresses, GetExtendedTcpTable, IP_ADAPTER_ADDRESSES_LH, MIB_TCPROW_OWNER_PID,
    TCP_TABLE_OWNER_PID_CONNECTIONS,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::NetworkListManager::{
    INetworkListManager, NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED, NLM_NETWORK_CATEGORY_PRIVATE, NetworkListManager,
};
use windows::Win32::Networking::WinSock::{AF_INET, SO_EXCLUSIVEADDRUSE, SOCKADDR_IN, SOCKET, SOL_SOCKET, WSAGetLastError, setsockopt};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::core::GUID;

/// An address of this computer on a home network.
#[derive(Clone, Debug, PartialEq)]
pub struct HomeAddr {
    pub ip: Ipv4Addr,
    /// Windows counts the network as private (or a work domain); false on a public network (café, hotel)
    pub private: bool,
}

/// Private IPv4 addresses of connected adapters that have a gateway (Wi-Fi, Ethernet), with the category
/// Windows gives each network. Virtual switches (Hyper-V, WSL) have no gateway and are left out.
pub fn home_addrs() -> Vec<HomeAddr> {
    let adapters = adapters();
    // COM in a thread of its own, whatever the caller's thread has set up
    let private = std::thread::spawn(private_adapters).join().unwrap_or_default();
    adapters
        .into_iter()
        .flat_map(|(guid, ips)| {
            let p = private.iter().any(|g| g.eq_ignore_ascii_case(&guid));
            ips.into_iter().map(move |ip| HomeAddr { ip, private: p })
        })
        .collect()
}

/// Addresses on networks Windows counts as private: where home network access is allowed.
pub fn private_ipv4() -> Vec<Ipv4Addr> {
    home_addrs().into_iter().filter(|a| a.private).map(|a| a.ip).collect()
}

/// Is the address one a home network uses (192.168.x, 10.x, 172.16-31.x)?
pub fn is_home_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => v6.to_ipv4_mapped().is_some_and(|v4| v4.is_private()),
    }
}

/// Adapter id ("{GUID}") and its private IPv4 addresses, for adapters that are up and have a gateway.
fn adapters() -> Vec<(String, Vec<Ipv4Addr>)> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    // u64 keeps the structures aligned
    let mut buf: Vec<u64> = Vec::new();
    loop {
        buf.resize((size as usize).div_ceil(8), 0);
        let r = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr().cast()), &mut size) };
        match r {
            _ if r == NO_ERROR.0 => break,
            _ if r == ERROR_BUFFER_OVERFLOW.0 => continue,
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !p.is_null() {
        let a = unsafe { &*p };
        if a.OperStatus == IfOperStatusUp && !a.FirstGatewayAddress.is_null() {
            let mut ips = Vec::new();
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let ua = unsafe { &*u };
                let sa = ua.Address.lpSockaddr;
                if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                    let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                    let ip = Ipv4Addr::from(u32::from_be(unsafe { sin.sin_addr.S_un.S_addr }));
                    if ip.is_private() && !ips.contains(&ip) {
                        ips.push(ip);
                    }
                }
                u = ua.Next;
            }
            let name = unsafe { a.AdapterName.to_string() }.unwrap_or_default();
            if !ips.is_empty() {
                out.push((name, ips));
            }
        }
        p = a.Next;
    }
    out
}

/// Ids ("{GUID}") of the adapters whose network Windows counts as private or as a work domain.
fn private_adapters() -> Vec<String> {
    unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED);
        let list = (|| -> windows::core::Result<Vec<String>> {
            let nlm: INetworkListManager = CoCreateInstance(&NetworkListManager, None, CLSCTX_ALL)?;
            let conns = nlm.GetNetworkConnections()?;
            let mut out = Vec::new();
            loop {
                let mut one = [None];
                let mut fetched = 0;
                if conns.Next(&mut one, Some(&mut fetched)).is_err() || fetched == 0 {
                    break;
                }
                let Some(c) = one[0].take() else { break };
                let category = c.GetNetwork().and_then(|n| n.GetCategory());
                if matches!(category, Ok(k) if k == NLM_NETWORK_CATEGORY_PRIVATE || k == NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED)
                    && let Ok(id) = c.GetAdapterId()
                {
                    out.push(braced(&id));
                }
            }
            Ok(out)
        })()
        .unwrap_or_default();
        if init.is_ok() {
            CoUninitialize();
        }
        list
    }
}

/// "{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}", the form adapter names use.
fn braced(g: &GUID) -> String {
    let d4 = g.data4;
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, d4[0], d4[1], d4[2], d4[3], d4[4], d4[5], d4[6], d4[7]
    )
}

/// No other socket may then use this one's port, not even on a narrower address. Without it, while the panel
/// listens on 0.0.0.0:8765, another program can still listen on 127.0.0.1:8765 and Windows gives it this
/// computer's connections, the tunnel's among them. Set before binding.
pub fn exclusive(s: &impl AsRawSocket) -> std::io::Result<()> {
    let on = 1i32.to_ne_bytes();
    // SAFETY: a valid socket handle and a 4-byte option value
    if unsafe { setsockopt(SOCKET(s.as_raw_socket() as usize), SOL_SOCKET, SO_EXCLUSIVEADDRUSE, Some(&on)) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() }.0))
    }
}

/// The process that opened a connection from `peer` to this computer's `server` address (IPv4).
pub fn client_pid(peer: SocketAddr, server: SocketAddr) -> Option<u32> {
    let (SocketAddr::V4(peer), SocketAddr::V4(server)) = (peer, server) else { return None };
    let mut size = 0u32;
    let mut buf: Vec<u64> = Vec::new();
    loop {
        let r = unsafe {
            GetExtendedTcpTable(
                (!buf.is_empty()).then(|| buf.as_mut_ptr().cast()),
                &mut size,
                false,
                AF_INET.0 as u32,
                TCP_TABLE_OWNER_PID_CONNECTIONS,
                0,
            )
        };
        match r {
            _ if r == NO_ERROR.0 && !buf.is_empty() => break,
            _ if r == ERROR_INSUFFICIENT_BUFFER.0 || (r == NO_ERROR.0 && buf.is_empty()) => {
                buf.resize((size as usize).div_ceil(8) + 64, 0)
            }
            _ => return None,
        }
    }
    // MIB_TCPTABLE_OWNER_PID: a count, then the rows
    let count = unsafe { *(buf.as_ptr() as *const u32) } as usize;
    let rows = unsafe {
        let first = (buf.as_ptr() as *const u8).add(size_of::<u32>()) as *const MIB_TCPROW_OWNER_PID;
        std::slice::from_raw_parts(first, count)
    };
    let port = |p: u32| u16::from_be(p as u16);
    let addr = |a: u32| Ipv4Addr::from(u32::from_be(a));
    // The client's side of the connection: its own address is the peer, its remote end is the server
    rows.iter()
        .find(|r| {
            addr(r.dwLocalAddr) == *peer.ip()
                && port(r.dwLocalPort) == peer.port()
                && addr(r.dwRemoteAddr) == *server.ip()
                && port(r.dwRemotePort) == server.port()
        })
        .map(|r| r.dwOwningPid)
}

/// Does the process run in the same Windows session (the same signed-in user at this screen) as StrCu?
pub fn same_session(pid: u32) -> bool {
    let (mut theirs, mut ours) = (u32::MAX, u32::MAX - 1);
    let known = unsafe {
        ProcessIdToSessionId(pid, &mut theirs).is_ok() && ProcessIdToSessionId(GetCurrentProcessId(), &mut ours).is_ok()
    };
    known && theirs == ours
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_ranges() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(is_home_ip(ip("192.168.1.50")) && is_home_ip(ip("10.0.0.7")) && is_home_ip(ip("172.20.1.1")));
        assert!(!is_home_ip(ip("100.101.102.103")) && !is_home_ip(ip("8.8.8.8")) && !is_home_ip(ip("172.32.0.1")));
        assert!(is_home_ip(ip("::ffff:192.168.1.50")) && !is_home_ip(ip("fe80::1")));
    }

    /// A real connection on this computer: the table finds our own process as its client.
    #[test]
    fn finds_the_client_process() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(server.local_addr().unwrap()).unwrap();
        let (_conn, peer) = server.accept().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
        let pid = client_pid(peer, server.local_addr().unwrap());
        assert_eq!(pid, Some(std::process::id()));
        assert!(same_session(std::process::id()));
    }

    #[test]
    fn guid_form() {
        let g = GUID::from_u128(0x0123abcd_0001_0002_0304_05060708090a);
        assert_eq!(braced(&g), "{0123ABCD-0001-0002-0304-05060708090A}");
    }
}
