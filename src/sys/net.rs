//! The computer's addresses on the home network.

use std::net::Ipv4Addr;

use windows::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_SUCCESS};
use windows::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

/// Private IPv4 addresses of connected adapters that have a gateway (Wi-Fi, Ethernet). Virtual switches
/// (Hyper-V, WSL) have none and are left out.
pub fn lan_ipv4() -> Vec<Ipv4Addr> {
    let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut size: u32 = 16 * 1024;
    // u64 keeps the structures aligned
    let mut buf: Vec<u64> = Vec::new();
    loop {
        buf.resize((size as usize).div_ceil(8), 0);
        let r = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(buf.as_mut_ptr().cast()), &mut size) };
        match r {
            _ if r == ERROR_SUCCESS.0 => break,
            _ if r == ERROR_BUFFER_OVERFLOW.0 => continue,
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !p.is_null() {
        let a = unsafe { &*p };
        if a.OperStatus == IfOperStatusUp && !a.FirstGatewayAddress.is_null() {
            let mut u = a.FirstUnicastAddress;
            while !u.is_null() {
                let ua = unsafe { &*u };
                let sa = ua.Address.lpSockaddr;
                if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
                    let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
                    let ip = Ipv4Addr::from(u32::from_be(unsafe { sin.sin_addr.S_un.S_addr }));
                    if ip.is_private() && !out.contains(&ip) {
                        out.push(ip);
                    }
                }
                u = ua.Next;
            }
        }
        p = a.Next;
    }
    out
}
