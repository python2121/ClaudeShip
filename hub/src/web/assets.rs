//! The web app's files. Embedded at build time from the repo's `web/`, so
//! the page always matches the hub that serves it; `CLAUDESHIP_WEB` names
//! a directory to read them from instead, per request, for working on the
//! page without rebuilding.

use std::path::Path;

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../web");

/// The content type for a file extension; anything not listed isn't served.
pub fn content_type(extension: &str) -> Option<&'static str> {
    Some(match extension {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        _ => return None,
    })
}

/// The file for a (percent-decoded) request path and its content type.
/// `None` for anything that isn't one of the page's files: an empty or
/// `.`-prefixed component (no `..`, no dotfiles), an unknown type, or a
/// missing file.
pub fn lookup(path: &str) -> Option<(Vec<u8>, &'static str)> {
    let relative = if path == "/" {
        "index.html"
    } else {
        path.strip_prefix('/')?
    };
    if relative
        .split('/')
        .any(|c| c.is_empty() || c.starts_with('.'))
    {
        return None;
    }
    let extension = Path::new(relative).extension()?.to_str()?;
    let kind = content_type(extension)?;
    let data = match std::env::var_os("CLAUDESHIP_WEB").filter(|d| !d.is_empty()) {
        Some(dir) => {
            let file = Path::new(&dir).join(relative);
            if !std::fs::metadata(&file).is_ok_and(|m| m.is_file()) {
                return None;
            }
            std::fs::read(file).ok()?
        }
        None => EMBEDDED.get_file(relative)?.contents().to_vec(),
    };
    Some((data, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_is_embedded() {
        assert!(EMBEDDED.get_file("index.html").is_some());
        assert!(EMBEDDED.get_file("app.js").is_some());
        assert!(EMBEDDED.get_file("vendor/xterm.js").is_some());
    }

    #[test]
    fn paths_that_are_not_the_pages_files() {
        // SAFETY of the test: CLAUDESHIP_WEB is not set under cargo test
        // unless the caller set it; either way these must all be refused.
        for path in ["/.git/config", "/vendor/../app.js", "//app.js", "/app.js/", "/x/.hidden.js", "/README", "/vendor/XTERM-LICENSE"] {
            assert!(lookup(path).is_none(), "{path}");
        }
        assert_eq!(content_type("js"), Some("text/javascript; charset=utf-8"));
        assert_eq!(content_type("txt"), None);
    }
}
