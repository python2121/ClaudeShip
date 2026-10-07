//! Hub settings, persisted as JSON (`config.json` in the hub's home).
//! Parsing is tolerant: a missing or malformed key falls back to its
//! default rather than failing the load.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString, OsString};
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubConfig {
    /// The directory whose children are the web app's project list.
    pub root: String,
    pub port: u16,
    /// `--permission-mode` for sessions launched from the web app.
    pub default_permission_mode: String,
    /// Extra names the web server answers to, matched exactly. Empty by
    /// default: see `security::is_allowed_host` for why names are risky.
    pub allowed_hosts: Vec<String>,
    /// Prefixes of the network interfaces Tailscale's addresses live on.
    /// The tailnet gate requires the address a connection arrived on to be
    /// held by one of these: on macOS Tailscale is a `utun` device, on
    /// Linux it is `tailscale0`.
    pub tunnel_interfaces: Vec<String>,
}

/// What `claude --permission-mode` accepts (2.1.289).
pub const PERMISSION_MODES: [&str; 6] = [
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "manual",
    "plan",
    "dontAsk",
];

#[cfg(target_os = "linux")]
pub const DEFAULT_TUNNEL_INTERFACES: [&str; 1] = ["tailscale"];
#[cfg(not(target_os = "linux"))]
pub const DEFAULT_TUNNEL_INTERFACES: [&str; 1] = ["utun"];

impl Default for HubConfig {
    fn default() -> Self {
        Self::fallback()
    }
}

impl HubConfig {
    #[allow(dead_code)] // the web settings API (phase 4)
    pub fn permission_modes() -> &'static [&'static str] {
        &PERMISSION_MODES
    }

    pub fn fallback() -> HubConfig {
        HubConfig {
            root: home_dir()
                .join("Documents/code")
                .to_string_lossy()
                .into_owned(),
            port: 7433,
            default_permission_mode: "auto".into(),
            allowed_hosts: Vec::new(),
            tunnel_interfaces: DEFAULT_TUNNEL_INTERFACES
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    pub fn parse(data: &[u8]) -> HubConfig {
        let mut config = Self::fallback();
        let Ok(Value::Object(obj)) = serde_json::from_slice::<Value>(data) else {
            return config;
        };
        if let Some(root) = obj.get("root").and_then(Value::as_str)
            && !root.is_empty()
        {
            config.root = expand_tilde(root);
        }
        if let Some(port) = obj.get("port").and_then(integer)
            && (1..=65535).contains(&port)
        {
            config.port = port as u16;
        }
        if let Some(mode) = obj.get("defaultPermissionMode").and_then(Value::as_str)
            && PERMISSION_MODES.contains(&mode)
        {
            config.default_permission_mode = mode.to_string();
        }
        if let Some(hosts) = string_list(obj.get("allowedHosts")) {
            config.allowed_hosts = hosts
                .iter()
                .map(|h| h.to_lowercase())
                .filter(|h| !h.is_empty())
                .collect();
        }
        if let Some(names) = string_list(obj.get("tunnelInterfaces")) {
            config.tunnel_interfaces = names.into_iter().filter(|n| !n.is_empty()).collect();
        }
        config
    }

    /// The config at `path`, or the defaults when it is missing or unreadable.
    pub fn load(path: &Path) -> HubConfig {
        std::fs::read(path)
            .map(|data| Self::parse(&data))
            .unwrap_or_else(|_| Self::fallback())
    }

    /// Pretty-printed with sorted keys, written atomically.
    #[allow(dead_code)] // the web settings API (phase 4)
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut obj: BTreeMap<&str, Value> = BTreeMap::new();
        obj.insert("root", self.root.clone().into());
        obj.insert("port", self.port.into());
        obj.insert(
            "defaultPermissionMode",
            self.default_permission_mode.clone().into(),
        );
        obj.insert("allowedHosts", self.allowed_hosts.clone().into());
        obj.insert("tunnelInterfaces", self.tunnel_interfaces.clone().into());
        let data = serde_json::to_vec_pretty(&obj).map_err(std::io::Error::other)?;
        let mut temp = path.as_os_str().to_owned();
        temp.push(format!(".tmp-{}", std::process::id()));
        let temp = PathBuf::from(temp);
        let result = (|| {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&data)?;
            file.sync_all()?;
            std::fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

/// A JSON number that is a whole number (`9000` or `9000.0`, as Foundation
/// would bridge it to `Int`).
fn integer(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    let f = value.as_f64()?;
    (f.fract() == 0.0 && f.abs() < 9.0e15).then_some(f as i64)
}

/// An array of strings; `None` if it is anything else, including an array
/// with a non-string in it (Swift's `as? [String]`).
fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// The user's home directory: `$HOME`, else the password database.
pub fn home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    // SAFETY: getpwuid returns a pointer into static storage or null; the
    // string is copied out before anything else can call it on this thread.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_dir.is_null() {
            return PathBuf::from(OsString::from_vec(
                CStr::from_ptr((*pw).pw_dir).to_bytes().to_vec(),
            ));
        }
    }
    PathBuf::from("/")
}

fn home_of(user: &str) -> Option<String> {
    let name = CString::new(user).ok()?;
    // SAFETY: as in `home_dir`.
    unsafe {
        let pw = libc::getpwnam(name.as_ptr());
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return None;
        }
        Some(CStr::from_ptr((*pw).pw_dir).to_string_lossy().into_owned())
    }
}

/// `NSString.expandingTildeInPath`: `~` and `~/…` are the home directory,
/// `~user/…` that user's; an unknown user leaves the path alone.
pub fn expand_tilde(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let expanded = match path.strip_prefix('~') {
        Some(rest) => {
            let (user, tail) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            let home = if user.is_empty() {
                Some(home_dir().to_string_lossy().into_owned())
            } else {
                home_of(user)
            };
            match home {
                Some(home) => format!("{}{}", home.trim_end_matches('/'), tail),
                None => path.to_string(),
            }
        }
        None => path.to_string(),
    };
    // Repeated slashes collapse and a trailing one goes, as they do there.
    let parts: Vec<&str> = expanded.split('/').filter(|p| !p.is_empty()).collect();
    let joined = parts.join("/");
    if expanded.starts_with('/') {
        format!("/{joined}")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_keys() {
        let c = HubConfig::parse(
            br#"{"port": 9000, "defaultPermissionMode": "plan", "root": "/tmp/projects"}"#,
        );
        assert_eq!(c.port, 9000, "config: port");
        assert_eq!(c.default_permission_mode, "plan", "config: permission mode");
        assert_eq!(c.root, "/tmp/projects", "config: root");
        assert!(
            c.allowed_hosts.is_empty(),
            "config: no extra host names by default"
        );
        assert_eq!(
            c.tunnel_interfaces,
            DEFAULT_TUNNEL_INTERFACES.to_vec(),
            "config: tunnel interfaces default"
        );
    }

    #[test]
    fn sloppy_values_fall_back() {
        let c = HubConfig::parse(br#"{"port": 99999, "defaultPermissionMode": "yolo"}"#);
        assert_eq!(
            c.port,
            HubConfig::fallback().port,
            "config: out-of-range port falls back"
        );
        assert_eq!(
            c.default_permission_mode, "auto",
            "config: unknown mode falls back to auto"
        );
        let c = HubConfig::parse(
            br#"{"port": "80", "root": "", "allowedHosts": "x", "tunnelInterfaces": [1]}"#,
        );
        assert_eq!(c, HubConfig::fallback(), "wrong types fall back key by key");
        assert_eq!(
            HubConfig::parse(br#"{"port": 8080.0}"#).port,
            8080,
            "a whole float is a port"
        );
        assert_eq!(HubConfig::parse(br#"{"port": 8080.5}"#).port, 7433);
        assert_eq!(HubConfig::parse(br#"{"port": 0}"#).port, 7433);
    }

    #[test]
    fn garbage_falls_back_whole() {
        assert_eq!(HubConfig::parse(b"not json"), HubConfig::fallback());
        assert_eq!(HubConfig::parse(b"[1]"), HubConfig::fallback());
    }

    #[test]
    fn extra_host_names_lowercased() {
        let c = HubConfig::parse(br#"{"allowedHosts": ["Mac.Tail1.ts.net", ""]}"#);
        assert_eq!(c.allowed_hosts, vec!["mac.tail1.ts.net"]);
    }

    #[test]
    fn tunnel_interfaces_configurable() {
        let c = HubConfig::parse(br#"{"tunnelInterfaces": ["wg", ""]}"#);
        assert_eq!(c.tunnel_interfaces, vec!["wg"]);
        let c = HubConfig::parse(br#"{"tunnelInterfaces": []}"#);
        assert!(
            c.tunnel_interfaces.is_empty(),
            "an empty list turns tailnet access off"
        );
    }

    #[test]
    fn root_tilde_expanded() {
        let home = home_dir().to_string_lossy().into_owned();
        assert_eq!(
            HubConfig::parse(br#"{"root": "~/src/"}"#).root,
            format!("{home}/src")
        );
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~no-such-user-xyz/x"), "~no-such-user-xyz/x");
        assert_eq!(expand_tilde("/a/b/"), "/a/b");
        assert_eq!(expand_tilde("/"), "/");
        assert_eq!(expand_tilde("//a//b"), "/a/b", "repeated slashes collapse");
        assert_eq!(expand_tilde("~//x"), format!("{home}/x"));
        assert_eq!(expand_tilde("a/"), "a", "a relative path stays relative");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir =
            std::env::temp_dir().join(format!("claudeship-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let config = HubConfig {
            root: "/x/y".into(),
            port: 9001,
            default_permission_mode: "plan".into(),
            allowed_hosts: vec!["a.b".into()],
            tunnel_interfaces: vec!["utun".into(), "wg".into()],
        };
        config.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let keys: Vec<usize> = [
            "allowedHosts",
            "defaultPermissionMode",
            "port",
            "root",
            "tunnelInterfaces",
        ]
        .iter()
        .map(|k| text.find(k).unwrap())
        .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "keys sorted");
        assert!(text.contains('\n'), "pretty-printed");
        assert_eq!(HubConfig::load(&path), config);
        assert_eq!(
            HubConfig::load(&dir.join("missing.json")),
            HubConfig::fallback()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn permission_modes_listed() {
        assert!(HubConfig::permission_modes().contains(&"auto"));
        assert_eq!(HubConfig::permission_modes().len(), 6);
    }
}
