//! `claudeship hub pair | peers | unpair`: the swarm from the command line.
//!
//! `pair <link>` does what the phone does: pair with the member the link
//! names (`GET /auth?k=` → the cookie), fetch its swarm (`POST /api/swarm`
//! → `{secret, peers}`), and hand that to this machine's hub as
//! `POST /api/swarm/join` over loopback (`http://127.0.0.1:<port>`, with
//! this hub's own pairing secret as the cookie, from the socket's `link`
//! op) — the same route a phone or browser uses, so there is one join path.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::client::{self, fail, request};
use super::hub_cmd::compact_age;
use crate::swarm::client::{Answer, parse_response};
use crate::token::COOKIE_NAME;

/// One blocking HTTP/1 request to `authority` (`host:port`), answer read
/// to EOF.
fn http(authority: &str, method: &str, target: &str, cookie: Option<&str>, body: Option<&Value>, wait: Duration) -> Result<Answer, String> {
    let address = authority
        .to_socket_addrs()
        .map_err(|e| format!("{authority}: {e}"))?
        .next()
        .ok_or_else(|| format!("{authority}: no address"))?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|e| format!("cannot reach {authority}: {e}"))?;
    let _ = stream.set_read_timeout(Some(wait));
    let payload = body.map(Value::to_string).unwrap_or_default();
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    if let Some(cookie) = cookie {
        head.push_str(&format!("Cookie: {COOKIE_NAME}={cookie}\r\n"));
    }
    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(payload.as_bytes()))
        .map_err(|e| format!("{authority}: {e}"))?;
    let mut raw = Vec::new();
    stream
        .take(32 << 20)
        .read_to_end(&mut raw)
        .map_err(|e| format!("{authority}: {e}"))?;
    parse_response(&raw).ok_or_else(|| format!("{authority} did not answer like a hub"))
}

fn json_of(answer: &Answer) -> Value {
    serde_json::from_slice(&answer.body).unwrap_or(Value::Null)
}

/// `http://host:port/auth?k=<secret>` → (`host:port`, secret).
pub fn parse_link(link: &str) -> Option<(String, String)> {
    let rest = link.trim().strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/')?;
    let query = path.strip_prefix("auth?")?;
    let secret = query
        .split('&')
        .find_map(|p| p.strip_prefix("k="))
        .filter(|k| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric()))?;
    if authority.is_empty() {
        return None;
    }
    // `host:port` or `[v6]:port` as given; a bare host gets port 80.
    let has_port = if authority.starts_with('[') {
        authority.contains("]:")
    } else {
        authority.contains(':')
    };
    let authority = if has_port {
        authority.to_string()
    } else {
        format!("{authority}:80")
    };
    Some((authority, secret.to_string()))
}

pub fn pair(link: Option<&String>) -> ! {
    let Some((member, key)) = link.and_then(|l| parse_link(l)) else {
        fail("usage: claudeship hub pair <link>   (the http://…/auth?k=… link `claudeship hub link` prints on a member)");
    };
    // 1. Pair with the member, as a browser would.
    let answer = http(&member, "GET", &format!("/auth?k={key}"), None, None, Duration::from_secs(10))
        .unwrap_or_else(|e| fail(&e));
    let cookie = answer
        .header("set-cookie")
        .and_then(|c| c.split(';').next())
        .and_then(|c| c.strip_prefix(&format!("{COOKIE_NAME}=")))
        .map(str::to_string);
    let Some(cookie) = cookie.filter(|_| answer.status == 303) else {
        fail(&format!(
            "{member} refused the link (HTTP {}): {}",
            answer.status,
            String::from_utf8_lossy(&answer.body).trim()
        ));
    };
    // 2. Its swarm.
    let answer = http(&member, "POST", "/api/swarm", Some(&cookie), Some(&json!({})), Duration::from_secs(10))
        .unwrap_or_else(|e| fail(&e));
    let swarm = json_of(&answer);
    if answer.status != 200 || swarm.get("secret").and_then(Value::as_str).is_none() {
        fail(&format!(
            "{member} has no swarm to share (HTTP {}; is it an older hub?)",
            answer.status
        ));
    }
    // 3. Our hub joins it.
    drop(client::connect(true));
    let link = request(json!({"op": "link"}));
    let port = link.get("port").and_then(Value::as_i64).unwrap_or(0);
    let token = link.get("token").and_then(Value::as_str).unwrap_or("").to_string();
    let status = request(json!({"op": "status"}));
    if token.is_empty() || status.get("webListening").and_then(Value::as_bool) != Some(true) {
        fail("this machine's hub has no web server listening; see claudeship hub status");
    }
    let body = json!({"secret": swarm["secret"], "peers": swarm["peers"]});
    let answer = http(
        &format!("127.0.0.1:{port}"),
        "POST",
        "/api/swarm/join",
        Some(&token),
        Some(&body),
        Duration::from_secs(20),
    )
    .unwrap_or_else(|e| fail(&e));
    let joined = json_of(&answer);
    if answer.status != 200 {
        fail(&format!(
            "this hub could not join: {}",
            joined.get("error").and_then(Value::as_str).unwrap_or("refused")
        ));
    }
    println!("This hub joined the swarm.");
    for hello in joined["hello"].as_array().cloned().unwrap_or_default() {
        let name = hello["name"].as_str().unwrap_or("?");
        if hello["ok"] == true {
            println!("  {name}: paired");
        } else {
            println!(
                "  {name}: not reached ({}); gossip keeps trying",
                hello["error"].as_str().unwrap_or("no answer")
            );
        }
    }
    for moved in joined["rotated"].as_array().cloned().unwrap_or_default() {
        let name = moved["name"].as_str().unwrap_or("?");
        if moved["ok"] == true {
            println!("  {name}: came along from this hub's old swarm");
        } else {
            println!("  {name}: from this hub's old swarm, not reached; it must pair again");
        }
    }
    std::process::exit(0);
}

fn age(ms: u64) -> String {
    if ms == 0 {
        return "never".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    format!("{} ago", compact_age((now.saturating_sub(ms) / 1000) as i64))
}

pub fn peers() -> ! {
    let view = request(json!({"op": "peers"}));
    let me = &view["self"];
    let list = view["peers"].as_array().cloned().unwrap_or_default();
    let mut rows: Vec<[String; 6]> = vec![[
        "NAME".into(),
        "ADDRESSES".into(),
        "REACHABLE".into(),
        "LAST SEEN".into(),
        "PROTOCOL".into(),
        "TOMBSTONED".into(),
    ]];
    let addresses = |r: &Value| {
        let list: Vec<&str> = r["addresses"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        if list.is_empty() { "-".into() } else { list.join(" ") }
    };
    rows.push([
        format!("{} (this hub)", me["name"].as_str().unwrap_or("?")),
        addresses(me),
        "-".into(),
        "-".into(),
        me["protocol"].to_string(),
        "-".into(),
    ]);
    for peer in &list {
        let r = &peer["record"];
        let tombstone = r["tombstone"].as_u64();
        let reachable = if tombstone.is_some() {
            "-"
        } else if peer["reachable"] == true {
            "yes"
        } else if peer["refused"] == true {
            "no (refused)"
        } else {
            "no"
        };
        rows.push([
            r["name"].as_str().unwrap_or("?").to_string(),
            addresses(r),
            reachable.into(),
            age(r["lastSeen"].as_u64().unwrap_or(0)),
            r["protocol"].to_string(),
            tombstone.map_or("-".into(), age),
        ]);
    }
    let widths: Vec<usize> = (0..6)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
    if list.is_empty() {
        println!("\nNo peers. Join a swarm with: claudeship hub pair <link from a member's claudeship hub link>");
    }
    std::process::exit(0);
}

pub fn unpair(target: Option<&String>) -> ! {
    let Some(target) = target else {
        fail("usage: claudeship hub unpair <name | id>");
    };
    let reply = request(json!({"op": "unpair", "target": target}));
    let record = &reply["record"];
    println!(
        "{} ({}) is unpaired. Every hub in the swarm drops it within a few polls,",
        record["name"].as_str().unwrap_or("?"),
        record["id"].as_str().unwrap_or("?")
    );
    println!("and it leaves the swarm itself when it next hears from one of them.");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::parse_link;

    #[test]
    fn links() {
        assert_eq!(
            parse_link("http://100.64.0.1:7433/auth?k=abc123"),
            Some(("100.64.0.1:7433".into(), "abc123".into()))
        );
        assert_eq!(
            parse_link(" http://localhost:9/auth?x=1&k=ff "),
            Some(("localhost:9".into(), "ff".into()))
        );
        assert_eq!(
            parse_link("http://[fd7a::1]:7433/auth?k=ab"),
            Some(("[fd7a::1]:7433".into(), "ab".into()))
        );
        assert_eq!(parse_link("http://host/auth?k=ab"), Some(("host:80".into(), "ab".into())));
        assert_eq!(parse_link("https://x:1/auth?k=ab"), None);
        assert_eq!(parse_link("http://x:1/other?k=ab"), None);
        assert_eq!(parse_link("http://x:1/auth?k="), None);
        assert_eq!(parse_link("http://x:1/auth?k=a%20b"), None);
    }
}
