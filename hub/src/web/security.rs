//! Who may connect. The web app is a remote terminal, so three independent
//! things must all hold before a request does anything: the connection came
//! over loopback or the Tailscale interface, the browser addressed us by a
//! name that DNS cannot be made to lie about, and the request carries this
//! hub's pairing secret.
//!
//! The socket-facing half (the addresses a tunnel interface holds, a
//! socket's local and peer address) is `net::tunnel_addresses` and the
//! accept loop in `server`.

use std::net::IpAddr;

/// IPv4-mapped IPv6 (::ffff:a.b.c.d) as the IPv4 address; anything else
/// unchanged.
pub fn canonical(addr: IpAddr) -> IpAddr {
    match addr {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(addr, IpAddr::V4),
        IpAddr::V4(_) => addr,
    }
}

/// Dotted or colon form of an address for the log. IPv6 is written as all
/// eight groups, uncompressed, as the Swift hub logged it.
pub fn describe(addr: IpAddr) -> String {
    match canonical(addr) {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => v6
            .segments()
            .iter()
            .map(|g| format!("{g:x}"))
            .collect::<Vec<_>>()
            .join(":"),
    }
}

pub fn is_loopback(addr: IpAddr) -> bool {
    match canonical(addr) {
        IpAddr::V4(v4) => v4.octets()[0] == 127,
        IpAddr::V6(v6) => v6.octets() == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
    }
}

/// Tailscale's address ranges: 100.64.0.0/10 and fd7a:115c:a1e0::/48.
pub fn is_tailnet(addr: IpAddr) -> bool {
    match canonical(addr) {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && o[1] & 0xc0 == 64
        }
        IpAddr::V6(v6) => v6.octets()[..6] == [0xfd, 0x7a, 0x11, 0x5c, 0xa1, 0xe0],
    }
}

/// Whether a connection between these two addresses may be served.
/// Loopback talks only to loopback. Otherwise the address we were
/// reached on must be one a tunnel interface actually holds — the
/// 100.64/10 range alone proves nothing, since carriers and some Wi-Fi
/// networks hand out the same range on the LAN — and the peer must be
/// in the tailnet ranges too. `tunnel` is the addresses held by the
/// interfaces named in the config's `tunnelInterfaces`.
pub fn is_allowed_pair(local: IpAddr, remote: IpAddr, tunnel: &[IpAddr]) -> bool {
    if is_loopback(local) {
        return is_loopback(remote);
    }
    let local = canonical(local);
    is_tailnet(local) && tunnel.contains(&local) && is_tailnet(remote)
}

/// An address literal (`inet_pton` of either family), without brackets.
pub fn address(literal: &str) -> Option<IpAddr> {
    literal.parse().ok()
}

/// The name the browser used for us, from the Host header. Only names
/// that never go through DNS are accepted — `localhost` and loopback or
/// tailnet address literals — because over plain HTTP any other name
/// can be answered by whoever runs the network's DNS, first with their
/// own server and then with 127.0.0.1 (DNS rebinding), and the page
/// they served would then be "same-origin" with us, cookie and all.
/// `extra` is the config's exact-match escape hatch (`allowedHosts`),
/// for a name the user vouches for, e.g. behind `tailscale serve`.
pub fn is_allowed_host(header: &str, extra: &[String]) -> bool {
    let lowered = header.to_lowercase();
    let (host, port): (&str, &str) = if let Some(inner) = lowered.strip_prefix('[') {
        let Some(end) = inner.find(']') else {
            return false;
        };
        let rest = &inner[end + 1..];
        if !(rest.is_empty() || rest.starts_with(':')) {
            return false;
        }
        (&inner[..end], rest.get(1..).unwrap_or(""))
    } else if let Some(colon) = lowered.rfind(':') {
        (&lowered[..colon], &lowered[colon + 1..])
    } else {
        (&lowered, "")
    };
    // Nothing but a port number may follow the name: the header is
    // echoed into a response header later.
    if host.is_empty() || port.chars().count() > 5 || !port.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if host == "localhost" {
        return true;
    }
    if let Some(addr) = address(host) {
        return is_loopback(addr) || is_tailnet(addr);
    }
    extra.iter().any(|e| e == host)
}

/// A browser states the page a request came from in Origin. One that
/// isn't us is another site reaching in — WebSockets in particular are
/// not covered by the same-origin policy, so this check is the guard.
/// No Origin at all means not a browser (curl, a script); those still
/// need the pairing secret.
pub fn is_same_origin(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    let Some(host) = host else { return false };
    let Some((mut name, port)) = origin_host_port(origin) else {
        return false;
    };
    if name.contains(':') {
        name = format!("[{name}]");
    }
    let authority = match port {
        Some(port) => format!("{name}:{port}"),
        None => name,
    };
    authority.to_lowercase() == host.to_lowercase()
}

/// The host (percent-decoded, IPv6 without brackets) and explicit port of
/// an absolute URL — what Foundation's `URL(string:)?.host` / `.port` give.
/// `None` for anything without an authority (`null`, `data:…`) or that
/// isn't a well-formed URL.
fn origin_host_port(url: &str) -> Option<(String, Option<u64>)> {
    if url
        .bytes()
        .any(|b| b <= 0x20 || b >= 0x7f || b"\"<>\\^`{|}".contains(&b))
    {
        return None;
    }
    let (scheme, rest) = url.split_once(':')?;
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic()
        || !chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    {
        return None;
    }
    let rest = rest.strip_prefix("//")?;
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let (host, after) = inner.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (percent_decode(host)?, port)
    } else {
        let (host, port) = match hostport.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        };
        (percent_decode(host)?, port)
    };
    // Foundation has no host for `http:///x` and an empty one for
    // `http://:1`; neither names us.
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None | Some("") => None,
        Some(p) if p.bytes().all(|b| b.is_ascii_digit()) => Some(p.parse().ok()?),
        Some(_) => return None,
    };
    Some((host, port))
}

pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            // `from_str_radix` would take `%+f` as 0x0f.
            if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The value of cookie `name` in a Cookie header.
pub fn cookie(name: &str, header: Option<&str>) -> Option<String> {
    for pair in header.unwrap_or("").split(';').filter(|p| !p.is_empty()) {
        let kv = split_once_nonempty(pair.trim_matches(is_horizontal_space), b'=');
        if kv.len() == 2 && kv[0] == name {
            return Some(kv[1].to_string());
        }
    }
    None
}

/// Swift's `CharacterSet.whitespaces`: spaces and tab, not line breaks.
fn is_horizontal_space(c: char) -> bool {
    c == '\t' || (c.is_whitespace() && !c.is_control() && c != '\u{2028}' && c != '\u{2029}')
}

/// Swift's `split(separator:, maxSplits: 1)` (empty pieces dropped): the
/// first non-empty piece, then everything after the separator that ends it.
fn split_once_nonempty(text: &str, separator: u8) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b != separator {
            continue;
        }
        if i > start {
            let mut out = vec![&text[start..i]];
            if i + 1 < bytes.len() {
                out.push(&text[i + 1..]);
            }
            return out;
        }
        start = i + 1;
    }
    if start < bytes.len() {
        vec![&text[start..]]
    } else {
        vec![]
    }
}

/// Comparison that takes the same time wherever the strings differ.
pub fn constant_time_equals(a: &str, b: &str) -> bool {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    let mut difference: u8 = if x.len() == y.len() { 0 } else { 1 };
    for i in 0..x.len().max(y.len()) {
        difference |= x.get(i).copied().unwrap_or(0) ^ y.get(i).copied().unwrap_or(0);
    }
    difference == 0
}

/// The loopback address, for tests.
#[cfg(test)]
pub const LOOPBACK_V4: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        address(s).unwrap()
    }

    #[test]
    fn who_may_connect() {
        let ts = ip("100.101.102.103");
        let tunnel = [ts, ip("fd7a:115c:a1e0::1")];
        let pair = |l: &str, r: &str| is_allowed_pair(ip(l), ip(r), &tunnel);
        assert!(pair("127.0.0.1", "127.0.0.1"), "gate: loopback to loopback");
        assert!(pair("::1", "::1"), "gate: IPv6 loopback");
        assert!(
            pair("100.101.102.103", "100.64.0.9"),
            "gate: tailnet peer on our tunnel address"
        );
        assert!(
            pair("::ffff:100.101.102.103", "::ffff:100.127.255.254"),
            "gate: IPv4-mapped tailnet pair"
        );
        assert!(
            pair("fd7a:115c:a1e0::1", "fd7a:115c:a1e0::beef"),
            "gate: tailnet IPv6 pair"
        );
        assert!(
            !pair("100.70.1.1", "100.70.1.2"),
            "gate: CGNAT range on a non-tunnel interface refused"
        );
        assert!(!pair("192.168.1.20", "192.168.1.30"), "gate: LAN refused");
        assert!(
            !pair("100.101.102.103", "192.168.1.30"),
            "gate: non-tailnet peer refused"
        );
        assert!(
            !pair("100.101.102.103", "100.128.0.1"),
            "gate: peer just past the tailnet range refused"
        );
        assert!(
            !pair("100.101.102.103", "100.63.255.255"),
            "gate: peer just before the tailnet range refused"
        );
        assert!(
            !pair("127.0.0.1", "100.64.0.9"),
            "gate: loopback only talks to loopback"
        );
        assert!(!pair("fe80::1", "fe80::2"), "gate: link-local refused");
        assert!(
            !is_allowed_pair(ts, ip("100.64.0.9"), &[]),
            "gate: no tunnel, no tailnet access"
        );
    }

    #[test]
    fn mapped_loopback_and_odd_loopbacks() {
        assert!(
            is_allowed_pair(ip("::ffff:127.0.0.1"), ip("127.0.0.1"), &[]),
            "mapped loopback is loopback"
        );
        assert!(is_loopback(ip("127.255.0.1")), "all of 127/8");
        assert!(!is_loopback(ip("::")), "unspecified is not loopback");
        assert!(
            !is_loopback(ip("::127.0.0.1")),
            "IPv4-compatible form is not mapped"
        );
        assert!(!pair_with_no_tunnel("::1", "100.64.0.1"));
    }

    fn pair_with_no_tunnel(l: &str, r: &str) -> bool {
        is_allowed_pair(ip(l), ip(r), &[])
    }

    #[test]
    fn describe_and_canonical() {
        assert_eq!(describe(ip("::ffff:100.101.102.103")), "100.101.102.103");
        assert_eq!(
            describe(ip("fd7a:115c:a1e0::1")),
            "fd7a:115c:a1e0:0:0:0:0:1"
        );
        assert_eq!(canonical(ip("::ffff:1.2.3.4")), ip("1.2.3.4"));
        assert_eq!(canonical(ip("::1")), ip("::1"));
        assert_eq!(describe(LOOPBACK_V4), "127.0.0.1");
    }

    #[test]
    fn host_header() {
        let none: &[String] = &[];
        assert!(is_allowed_host("localhost:7433", none), "host: localhost");
        assert!(
            is_allowed_host("127.0.0.1:7433", none),
            "host: loopback literal"
        );
        assert!(
            is_allowed_host("100.101.102.103:7433", none),
            "host: tailnet literal"
        );
        assert!(
            is_allowed_host("[fd7a:115c:a1e0::1]:7433", none),
            "host: tailnet IPv6 literal"
        );
        assert!(
            is_allowed_host("[::1]:7433", none),
            "host: IPv6 loopback literal"
        );
        assert!(
            !is_allowed_host("my-mac:7433", none),
            "host: a bare name is whatever DNS says it is"
        );
        assert!(
            !is_allowed_host("mac.tail1234.ts.net", none),
            "host: so is a tailnet name, over plain HTTP"
        );
        assert!(
            is_allowed_host(
                "Mac.Tail1234.ts.net:443",
                &["mac.tail1234.ts.net".to_string()]
            ),
            "host: unless the config vouches for it"
        );
        assert!(
            !is_allowed_host("evil.example.com:7433", none),
            "host: rebinding domain refused"
        );
        assert!(
            !is_allowed_host("192.168.1.20:7433", none),
            "host: LAN literal refused"
        );
        assert!(
            !is_allowed_host("localhost.evil.com", none),
            "host: lookalike refused"
        );
        assert!(!is_allowed_host("", none), "host: empty refused");
        assert!(
            !is_allowed_host("localhost:1; script-src *", none),
            "host: only digits may follow the colon"
        );
        assert!(
            !is_allowed_host("[::1]x:7433", none),
            "host: nothing between the bracket and the port"
        );
        assert!(
            is_allowed_host("[::1]", none),
            "host: bracketed literal without a port"
        );
        assert!(is_allowed_host("localhost:", none), "host: empty port");
        assert!(
            !is_allowed_host("127.0.0.1:123456", none),
            "host: overlong port"
        );
    }

    #[test]
    fn host_header_edges() {
        let none: &[String] = &[];
        assert!(is_allowed_host("LOCALHOST", none), "case-insensitive");
        assert!(!is_allowed_host("[::1", none), "unclosed bracket");
        assert!(!is_allowed_host(":7433", none), "port with no name");
        assert!(!is_allowed_host("[]:7433", none), "empty brackets");
        assert!(
            !is_allowed_host("::1", none),
            "unbracketed IPv6 splits at the last colon"
        );
        assert!(
            !is_allowed_host("127.0.0.1:٧٤٣٣", none),
            "non-ASCII digits are not a port"
        );
        assert!(
            !is_allowed_host("0177.0.0.1", none),
            "octal-looking literal is not an address"
        );
        assert!(
            !is_allowed_host("[fe80::1%en0]:7433", none),
            "zone ids are not literals"
        );
        assert!(
            !is_allowed_host("my-mac", &["my-mac:7433".to_string()]),
            "extra names match the name, not name:port"
        );
    }

    #[test]
    fn cookies() {
        assert_eq!(
            cookie("claude_ship", Some("a=1; claude_ship=abc123; b=2")).as_deref(),
            Some("abc123"),
            "cookie: found among others"
        );
        assert_eq!(
            cookie("claude_ship", Some("xclaude_ship=abc; other=1")),
            None,
            "cookie: name must match whole"
        );
        assert_eq!(cookie("claude_ship", None), None, "cookie: no header");
        assert_eq!(
            cookie("k", Some("k=a=b")).as_deref(),
            Some("a=b"),
            "value keeps its own ="
        );
        assert_eq!(cookie("k", Some("k=")), None, "empty value is no cookie");
        assert_eq!(
            cookie("k", Some("  k=v  ;")).as_deref(),
            Some("v"),
            "whitespace around the pair trimmed"
        );
        assert_eq!(
            cookie("k", Some("j=1;k=2")).as_deref(),
            Some("2"),
            "no space after the semicolon"
        );
    }

    #[test]
    fn tokens() {
        assert!(constant_time_equals("abcdef", "abcdef"), "token: equal");
        assert!(!constant_time_equals("abcdef", "abcdeg"), "token: differs");
        assert!(
            !constant_time_equals("abcdef", "abcde"),
            "token: prefix is not equal"
        );
        assert!(
            !constant_time_equals("", "abc"),
            "token: empty is not equal"
        );
        assert!(constant_time_equals("", ""));
        assert!(!constant_time_equals("a\0", "a"), "a NUL is not padding");
    }

    #[test]
    fn origins() {
        assert!(
            is_same_origin(None, Some("localhost:7433")),
            "origin: absent (not a browser)"
        );
        assert!(
            is_same_origin(Some("http://localhost:7433"), Some("localhost:7433")),
            "origin: our own page"
        );
        assert!(
            is_same_origin(
                Some("http://[fd7a:115c:a1e0::1]:7433"),
                Some("[fd7a:115c:a1e0::1]:7433")
            ),
            "origin: IPv6 page"
        );
        assert!(
            !is_same_origin(Some("http://evil.example.com"), Some("localhost:7433")),
            "origin: another site"
        );
        assert!(
            !is_same_origin(Some("http://localhost:9999"), Some("localhost:7433")),
            "origin: another port"
        );
        assert!(
            !is_same_origin(Some("null"), Some("localhost:7433")),
            "origin: opaque origin"
        );
    }

    #[test]
    fn origin_edges() {
        assert!(
            !is_same_origin(Some("http://localhost:7433"), None),
            "an Origin with no Host is refused"
        );
        assert!(
            is_same_origin(Some("http://LocalHost:7433"), Some("localhost:7433")),
            "case-insensitive"
        );
        assert!(
            is_same_origin(Some("http://localhost"), Some("localhost")),
            "no port on either side"
        );
        assert!(
            !is_same_origin(Some("http://localhost"), Some("localhost:80")),
            "a default port is not filled in"
        );
        assert!(
            is_same_origin(Some("http://localhost:07433"), Some("localhost:7433")),
            "port is a number"
        );
        assert!(!is_same_origin(
            Some("http://localhost:7433.evil.com"),
            Some("localhost:7433")
        ));
        assert!(
            !is_same_origin(
                Some("http://localhost:7433@evil.com"),
                Some("localhost:7433")
            ),
            "userinfo is not the host"
        );
        assert!(
            !is_same_origin(Some("http://local host:7433"), Some("local host:7433")),
            "not a URL"
        );
        assert!(
            !is_same_origin(Some("localhost:7433"), Some("localhost:7433")),
            "no authority"
        );
        assert!(
            is_same_origin(Some("http://localhost:"), Some("localhost")),
            "empty port is no port"
        );
        assert!(
            is_same_origin(Some("https://localhost:7433/"), Some("localhost:7433")),
            "scheme and path ignored"
        );
        assert!(
            !is_same_origin(Some("http:///x"), Some("")),
            "no host is no origin"
        );
        assert!(
            !is_same_origin(Some("http://:7433"), Some(":7433")),
            "an empty host is no origin"
        );
        assert!(
            !is_same_origin(Some("http://loc%+fhost"), Some("loc\u{f}host")),
            "a percent escape takes two hex digits"
        );
        assert!(
            is_same_origin(Some("http://local%68ost:7433"), Some("localhost:7433")),
            "the host is percent-decoded"
        );
        assert!(
            is_same_origin(
                Some("http://[fe80::1%25en0]:7433"),
                Some("[fe80::1%en0]:7433")
            ),
            "a bracketed host is percent-decoded too"
        );
        assert!(
            is_same_origin(Some("http://a@b@localhost:7433"), Some("localhost:7433")),
            "the host follows the last @"
        );
        assert!(
            !is_same_origin(
                Some("http://evil.com#@localhost:7433"),
                Some("localhost:7433")
            ),
            "an @ after the authority is not userinfo"
        );
        assert!(
            !is_same_origin(
                Some("http://evil.com\\@localhost:7433"),
                Some("localhost:7433")
            ),
            "a backslash is refused outright"
        );
        assert!(
            is_same_origin(Some("HTTP://LOCALHOST:7433"), Some("localhost:7433")),
            "upper-case scheme"
        );
        assert!(
            !is_same_origin(Some("http://localhost.:7433"), Some("localhost:7433")),
            "a trailing dot is another name"
        );
        assert!(
            !is_same_origin(Some("http://localhost:+7433"), Some("localhost:7433")),
            "a signed port is not a port"
        );
    }
}
