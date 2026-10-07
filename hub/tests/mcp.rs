//! `claudeship mcp`, the real binary: an initialize, the tool list, and a
//! `ship_hosts` call `printf`'d into its stdin, with `CLAUDESHIP_HOME` a
//! scratch home whose `config.json` and `token` point at a stand-in hub (a
//! thread here answering `/api/state`). The tools themselves are covered
//! against a richer stand-in in `src/mcp.rs`'s unit tests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

const CLAUDESHIP: &str = env!("CARGO_BIN_EXE_claudeship");
const TOKEN: &str = "feedfacefeedfacefeedfacefeedface";

/// Each request line and its cookie.
type Log = Arc<Mutex<Vec<(String, String)>>>;

/// A stand-in hub: answers `/api/state` with one host, records each
/// request line and cookie.
fn stand_in_hub() -> (u16, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let seen = log.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut cookie = String::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some(value) = header.strip_prefix("Cookie:") {
                    cookie = value.trim().to_string();
                }
            }
            seen.lock().unwrap().push((request.trim().to_string(), cookie));
            let body = json!({"hosts": [{"id": "1b1b1b1b-0000-4000-8000-000000000000", "name": "stand-in",
                "local": true, "reachable": true, "protocol": 3, "root": "/r", "projects": []}]})
            .to_string();
            let mut stream = stream;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.flush();
            let _ = reader.read(&mut [0; 1]);
        }
    });
    (port, log)
}

#[test]
fn the_binary_lists_its_tools_and_reaches_the_hub_from_its_home() {
    let (port, log) = stand_in_hub();
    let home = PathBuf::from(format!("/tmp/cs-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.json"), json!({"port": port}).to_string()).unwrap();
    std::fs::write(home.join("token"), format!("{TOKEN}\n")).unwrap();

    let lines = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "smoke", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "ship_hosts", "arguments": {}}}),
    ];
    let output = Command::new("sh")
        .arg("-c")
        .arg(r#"printf '%s\n' "$1" "$2" "$3" "$4" | "$0" mcp"#)
        .arg(CLAUDESHIP)
        .args(lines.iter().map(Value::to_string))
        .env("CLAUDESHIP_HOME", &home)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8(output.stdout).unwrap();
    let answers: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("every stdout line is a JSON-RPC message"))
        .collect();
    assert_eq!(answers.len(), 3, "{stdout}");
    assert_eq!(answers[0]["result"]["protocolVersion"], "2024-11-05");
    let mut names: Vec<&str> = answers[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["ship_ask", "ship_hosts", "ship_kill", "ship_output", "ship_run", "ship_sessions", "ship_wait"]
    );
    let text = answers[2]["result"]["content"][0]["text"].as_str().unwrap();
    let hosts: Value = serde_json::from_str(text).unwrap();
    assert_eq!(hosts["hosts"][0]["name"], "stand-in");

    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].0, "GET /api/state HTTP/1.1");
    assert_eq!(log[0].1, format!("claude_ship={TOKEN}"), "the token comes from CLAUDESHIP_HOME");
}
