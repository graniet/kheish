//! Embedded daemon documentation served over the control plane.
//!
//! The `docs/` tree of this repository is compiled into the binary so every
//! daemon serves exactly the documentation matching its own version — the
//! console (or any client) renders it without a separate docs deployment.

use std::sync::OnceLock;

use include_dir::{Dir, include_dir};
use serde::{Deserialize, Serialize};

static DOCS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../docs");

/// The documentation manifest: the site identity plus the ordered
/// navigation groups from `docs.json`, enriched with page frontmatter.
#[derive(Clone, Debug, Serialize)]
pub struct DocsManifestView {
    pub name: String,
    pub description: String,
    pub groups: Vec<DocsGroupView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DocsGroupView {
    pub group: String,
    pub pages: Vec<DocsPageSummaryView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DocsPageSummaryView {
    /// The stable page path, e.g. `welcome/quickstart`.
    pub path: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// One documentation page: frontmatter fields plus the markdown body.
#[derive(Clone, Debug, Serialize)]
pub struct DocsPageView {
    pub path: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub content: String,
}

#[derive(Deserialize)]
struct DocsJson {
    name: String,
    description: String,
    navigation: DocsNavigation,
}

#[derive(Default, Deserialize)]
struct DocsNavigation {
    #[serde(default)]
    groups: Vec<DocsJsonGroup>,
    #[serde(default)]
    tabs: Vec<DocsJsonTab>,
}

impl DocsNavigation {
    /// Flattens the navigation into one ordered group list: top-level groups
    /// first, then each tab's groups. The console renders a flat sidebar, so
    /// Mintlify tabs only affect the hosted site's layout.
    fn into_groups(self) -> Vec<DocsJsonGroup> {
        let mut groups = self.groups;
        groups.extend(self.tabs.into_iter().flat_map(|tab| tab.groups));
        groups
    }
}

#[derive(Deserialize)]
struct DocsJsonTab {
    #[serde(default)]
    groups: Vec<DocsJsonGroup>,
}

#[derive(Deserialize)]
struct DocsJsonGroup {
    group: String,
    pages: Vec<String>,
}

/// Returns the documentation manifest, built once from the embedded tree.
pub fn docs_manifest() -> &'static DocsManifestView {
    static MANIFEST: OnceLock<DocsManifestView> = OnceLock::new();
    MANIFEST.get_or_init(|| {
        let raw = DOCS_DIR
            .get_file("docs.json")
            .and_then(|file| file.contents_utf8())
            .unwrap_or("{}");
        let parsed = serde_json::from_str::<DocsJson>(raw).unwrap_or(DocsJson {
            name: "Kheish daemon".to_string(),
            description: String::new(),
            navigation: DocsNavigation::default(),
        });
        let groups = parsed
            .navigation
            .into_groups()
            .into_iter()
            .map(|group| DocsGroupView {
                group: group.group,
                pages: group
                    .pages
                    .iter()
                    .filter_map(|path| {
                        docs_page(path).map(|page| DocsPageSummaryView {
                            path: path.clone(),
                            title: page.title,
                            description: page.description,
                        })
                    })
                    .collect(),
            })
            .filter(|group| !group.pages.is_empty())
            .collect();
        DocsManifestView {
            name: parsed.name,
            description: parsed.description,
            groups,
        }
    })
}

/// Loads one documentation page by its manifest path (`concepts/personas`).
/// Returns `None` for unknown paths — including any traversal attempt, since
/// lookups only hit the embedded tree.
pub fn docs_page(path: &str) -> Option<DocsPageView> {
    let normalized = path.trim().trim_matches('/');
    if normalized.is_empty() || normalized.contains("..") {
        return None;
    }
    let file = DOCS_DIR
        .get_file(format!("{normalized}.mdx"))
        .or_else(|| DOCS_DIR.get_file(format!("{normalized}.md")))?;
    let raw = file.contents_utf8()?;
    let (front, body) = split_frontmatter(raw);
    Some(DocsPageView {
        path: normalized.to_string(),
        title: frontmatter_field(front, "title").unwrap_or_else(|| {
            normalized
                .rsplit('/')
                .next()
                .unwrap_or(normalized)
                .to_string()
        }),
        description: frontmatter_field(front, "description").unwrap_or_default(),
        content: body.trim_start().to_string(),
    })
}

/// Loads one embedded static asset (page images, logos) by its docs-relative
/// path, returning the bytes and content type. Only image extensions are
/// served; everything else — including traversal attempts — returns `None`.
pub fn docs_asset(path: &str) -> Option<(&'static [u8], &'static str)> {
    let normalized = path.trim().trim_matches('/');
    if normalized.is_empty() || normalized.contains("..") {
        return None;
    }
    let mime = match normalized.rsplit('.').next() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        _ => return None,
    };
    let file = DOCS_DIR.get_file(normalized)?;
    Some((file.contents(), mime))
}

/// Splits a leading `---` YAML frontmatter block from the markdown body.
fn split_frontmatter(raw: &str) -> (&str, &str) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return ("", raw);
    };
    match rest.split_once("\n---") {
        Some((front, body)) => (front, body.trim_start_matches(['-', '\n'])),
        None => ("", raw),
    }
}

/// Extracts one simple `key: value` frontmatter field (values may contain
/// colons; surrounding quotes are stripped).
fn frontmatter_field(front: &str, key: &str) -> Option<String> {
    front.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.trim_start().strip_prefix(':')?;
        let value = value.trim().trim_matches('"').trim_matches('\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_exposes_navigation_groups_with_titled_pages() {
        let manifest = docs_manifest();
        assert!(!manifest.groups.is_empty(), "docs.json groups embedded");
        let quickstart = manifest
            .groups
            .iter()
            .flat_map(|group| &group.pages)
            .find(|page| page.path == "welcome/quickstart")
            .expect("quickstart page listed");
        assert_eq!(quickstart.title, "Quickstart");
        assert!(!quickstart.description.is_empty());
    }

    #[test]
    fn pages_load_with_frontmatter_stripped() {
        let page = docs_page("welcome/quickstart").expect("page exists");
        assert_eq!(page.title, "Quickstart");
        assert!(page.content.starts_with("# Quickstart"));
        assert!(!page.content.contains("---\ntitle"));
    }

    #[test]
    fn unknown_and_traversal_paths_are_rejected() {
        assert!(docs_page("nope/missing").is_none());
        assert!(docs_page("../Cargo").is_none());
        assert!(docs_page("").is_none());
    }

    #[test]
    fn assets_serve_images_and_reject_everything_else() {
        let (bytes, mime) = docs_asset("logo.png").expect("embedded logo");
        assert!(!bytes.is_empty());
        assert_eq!(mime, "image/png");
        assert!(docs_asset("docs.json").is_none(), "non-image rejected");
        assert!(docs_asset("../Cargo.toml").is_none());
        assert!(docs_asset("nope/missing.png").is_none());
    }
}
