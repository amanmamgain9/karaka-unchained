//! Bind addresses: one concrete IP literal of a local interface, never the
//! unspecified address. (dsh: `parseIpLiteral`, `isWildcardHost`,
//! `isLoopbackHost`, `normalizeBindAddress` and `bindUrlHost` in
//! `host/webserver/src/index.ts`.)
//!
//! dsh parses with `ipaddr.js`, and rewrites a dotted-quad IPv6 tail first
//! so `::0.0.0.1` reads as `listen` reads it, the loopback `::1`. Rust's
//! `Ipv6Addr` already reads a tail as the address's low 32 bits, so the
//! rewrite isn't needed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// A host split into its address text and its `%zone`, if any.
fn split_zone(host: &str) -> (&str, Option<&str>) {
    match host.split_once('%') {
        Some((address, zone)) => (address, Some(zone)),
        None => (host, None),
    }
}

/// The address a concrete literal names, an IPv4-mapped IPv6 address read
/// as its IPv4 address. A zone is allowed on IPv6 only, as `node:net`'s
/// `isIP` allows it, and must be non-empty. (dsh: `parseIpLiteral`.)
pub(crate) fn parse_ip_literal(host: &str) -> Option<IpAddr> {
    let (address, zone) = split_zone(host);
    if let Ok(v4) = address.parse::<Ipv4Addr>() {
        return zone.is_none().then_some(IpAddr::V4(v4));
    }
    if zone.is_some_and(str::is_empty) {
        return None;
    }
    let v6 = address.parse::<Ipv6Addr>().ok()?;
    Some(match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    })
}

/// dsh's rejection of a host that isn't a literal.
pub(crate) fn not_literal(host: &str) -> String {
    format!(
        "webserver: host {} is not a concrete IPv4 or IPv6 address literal",
        json_string(host)
    )
}

/// `JSON.stringify` of a string, as dsh quotes a host in its errors.
pub(crate) fn json_string(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

fn is_unspecified(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.octets() == [0; 4],
        IpAddr::V6(v6) => v6.segments() == [0; 8],
    }
}

/// Whether `host` asks for every interface at once: IPv4 any, IPv6 any,
/// and an IPv4-mapped form of IPv4 any. (dsh: `isWildcardHost`.)
pub fn is_wildcard_host(host: &str) -> bool {
    parse_ip_literal(host).is_some_and(is_unspecified)
}

/// Whether `host` names the loopback: any address in 127/8, `::1`, and
/// their mapped forms. (dsh: `isLoopbackHost`, `range() === 'loopback'`.)
pub fn is_loopback_host(host: &str) -> bool {
    parse_ip_literal(host).is_some_and(|address| match address {
        IpAddr::V4(v4) => v4.octets()[0] == 127,
        IpAddr::V6(v6) => v6 == Ipv6Addr::LOCALHOST,
    })
}

/// The canonical text of the address `host` names: dotted quad for IPv4
/// and mapped literals, compressed IPv6 otherwise, without its zone.
/// (dsh: `normalizeBindAddress`.)
///
/// # Errors
///
/// `host` isn't a concrete literal, in dsh's words.
pub fn normalize_bind_address(host: &str) -> Result<String, String> {
    parse_ip_literal(host)
        .map(|address| address.to_string())
        .ok_or_else(|| not_literal(host))
}

/// `host` as a URL host: a loopback address in its canonical text, any
/// other as written, IPv6 bracketed; `None` for a non-loopback address with
/// a zone, which no URL can carry. (dsh: `bindUrlHost`.)
pub fn bind_url_host(host: &str) -> Option<String> {
    let (bare, zone) = split_zone(host);
    let loopback = is_loopback_host(bare);
    if !loopback && zone.is_some() {
        return None;
    }
    let text = if loopback {
        normalize_bind_address(bare).ok()?
    } else {
        host.to_owned()
    };
    Some(if text.parse::<Ipv6Addr>().is_ok() {
        format!("[{text}]")
    } else {
        text
    })
}

/// Checks a configured host: a concrete literal, not the unspecified
/// address. (dsh: `WebServer.Config`'s `host` transform.)
///
/// # Errors
///
/// In dsh's words.
pub fn check_host(host: &str) -> Result<(), String> {
    let address = parse_ip_literal(host).ok_or_else(|| not_literal(host))?;
    if is_unspecified(address) {
        return Err(format!(
            "webserver: host {} is an unspecified (wildcard) address, which is not supported: binding every interface would expose remote code execution to the network; bind one concrete IPv4 or IPv6 address of a local interface instead",
            json_string(host)
        ));
    }
    Ok(())
}

/// The socket address to bind for `host` and `port`: a zone becomes the
/// IPv6 scope id, by interface name or number, as `listen` reads it.
pub(crate) fn socket_address(host: &str, port: u16) -> Result<std::net::SocketAddr, String> {
    let (address, zone) = split_zone(host);
    if let Ok(v4) = address.parse::<Ipv4Addr>() {
        return Ok((v4, port).into());
    }
    let v6 = address.parse::<Ipv6Addr>().map_err(|_| not_literal(host))?;
    let scope = match zone {
        None => 0,
        Some(zone) => match zone.parse::<u32>() {
            Ok(index) => index,
            Err(_) => interface_index(zone).ok_or_else(|| {
                format!(
                    "webserver: host {}: no interface named {zone}",
                    json_string(host)
                )
            })?,
        },
    };
    Ok(std::net::SocketAddrV6::new(v6, port, 0, scope).into())
}

fn interface_index(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: `name` is a valid NUL-terminated string for the call's
    // duration, and `if_nametoindex` only reads it.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    (index != 0).then_some(index)
}
