//! Embedded web console served at the daemon HTTP root.
//!
//! The `console-dist/` bundle (produced by `scripts/bundle-console.sh`) is
//! compiled into the binary the same way [`crate::docs`] embeds the docs tree,
//! so a single `kheish-daemon` binary serves both its control-plane API and the
//! console that drives it — no separate static host, no CORS.
//!
//! Lookups fall back to `index.html` for extension-less paths so the console's
//! client-side router (`/playground`, `/sessions/:id/chat`, …) resolves on a
//! hard refresh. Missing files that look like assets (they carry an extension)
//! return `None` so the caller emits a real 404 instead of masking a broken
//! bundle with the SPA shell.

use include_dir::{Dir, include_dir};

static CONSOLE_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/console-dist");

const INDEX_HTML: &str = "index.html";

/// Resolves one console asset for a request path.
///
/// Returns the raw bytes plus a `Content-Type` value. Extension-less paths and
/// unknown routes fall back to the SPA shell; unknown files that carry an
/// extension return `None`.
pub(crate) fn lookup(path: &str) -> Option<(&'static [u8], &'static str)> {
    let normalized = path.trim_start_matches('/');
    // Reject traversal outright; embedded lookups only ever hit the bundle, but
    // keep the guard so a crafted path can never escape it.
    if normalized.contains("..") {
        return None;
    }
    if normalized.is_empty() {
        return index();
    }
    if let Some(file) = CONSOLE_DIR.get_file(normalized) {
        return Some((file.contents(), content_type_for(normalized)));
    }
    // A concrete asset (has a file extension in its last segment) that is not in
    // the bundle is a genuine 404, not a client-side route.
    if last_segment_has_extension(normalized) {
        return None;
    }
    index()
}

fn index() -> Option<(&'static [u8], &'static str)> {
    CONSOLE_DIR
        .get_file(INDEX_HTML)
        .map(|file| (file.contents(), "text/html; charset=utf-8"))
}

fn last_segment_has_extension(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|segment| segment.contains('.'))
}

fn content_type_for(path: &str) -> &'static str {
    let extension = path
        .rsplit('/')
        .next()
        .and_then(|segment| segment.rsplit_once('.'))
        .map(|(_, extension)| extension)
        .unwrap_or_default();
    match extension {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_empty_path_serve_index_html() {
        let (root, mime) = lookup("/").expect("root serves index");
        assert!(mime.starts_with("text/html"));
        assert!(root.starts_with(b"<!doctype html>") || root.starts_with(b"<!DOCTYPE html>"));
        let (empty, _) = lookup("").expect("empty path serves index");
        assert_eq!(empty, root);
    }

    #[test]
    fn client_side_routes_fall_back_to_the_spa_shell() {
        let (index, _) = lookup("/").expect("root");
        let (route, mime) = lookup("/playground").expect("spa route serves index");
        assert!(mime.starts_with("text/html"));
        assert_eq!(route, index, "extension-less routes serve the shell");
        let (nested, _) = lookup("/sessions/abc/chat").expect("nested spa route");
        assert_eq!(nested, index);
    }

    #[test]
    fn missing_asset_files_are_not_masked_by_the_shell() {
        assert!(
            lookup("/assets/does-not-exist.js").is_none(),
            "missing asset should 404 rather than serve the shell"
        );
    }

    #[test]
    fn traversal_paths_are_rejected() {
        assert!(lookup("/../Cargo.toml").is_none());
    }
}
