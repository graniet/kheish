use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};

pub fn bounded_workspace_root(default_root: &Path, requested_root: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(default_root)?;
    let requested = if requested_root.is_absolute() {
        normalize_absolute_path(requested_root)?
    } else {
        normalize_absolute_path(&root.join(normalize_relative_path(requested_root)?))?
    };
    let resolved = canonicalize_existing_prefix(&requested)?;
    if resolved == root || resolved.starts_with(&root) {
        Ok(resolved)
    } else {
        bail!(
            "path {} escapes workspace root {}",
            resolved.display(),
            root.display()
        )
    }
}

fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut suffix = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        let Some(name) = cursor.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = cursor.parent() else {
            break;
        };
        cursor = parent;
    }
    let mut resolved = std::fs::canonicalize(cursor)?;
    while let Some(component) = suffix.pop() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn normalize_relative_path(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path {} escapes workspace root", path.display());
                }
            }
            Component::Prefix(_) | Component::RootDir => {
                bail!("expected a relative path, got {}", path.display());
            }
        }
    }
    Ok(normalized)
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("expected an absolute path, got {}", path.display());
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !pop_non_root_component(&mut normalized) {
                    bail!("path {} escapes filesystem root", path.display());
                }
            }
        }
    }
    Ok(normalized)
}

fn pop_non_root_component(path: &mut PathBuf) -> bool {
    let original = path.clone();
    if !path.pop() {
        return false;
    }
    if path.as_os_str().is_empty() {
        *path = original;
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use anyhow::Result;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::bounded_workspace_root;

    fn test_root(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "kheish-workspace-tests-{}-{}-{unique}",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn bounded_workspace_root_keeps_relative_child_inside_root() -> Result<()> {
        let root = test_root("relative-child").join("workspace");
        let child = root.join("subdir");
        std::fs::create_dir_all(&child)?;
        let resolved = bounded_workspace_root(&root, std::path::Path::new("subdir"))?;
        assert_eq!(resolved, std::fs::canonicalize(child)?);
        Ok(())
    }

    #[test]
    fn bounded_workspace_root_rejects_absolute_escape() -> Result<()> {
        let root = test_root("absolute-escape").join("workspace");
        std::fs::create_dir_all(&root)?;
        let error = bounded_workspace_root(&root, std::path::Path::new("/"))
            .expect_err("absolute escape should fail");
        assert!(error.to_string().contains("escapes workspace root"));
        Ok(())
    }

    #[test]
    fn bounded_workspace_root_rejects_relative_escape() -> Result<()> {
        let root = test_root("relative-escape").join("workspace");
        std::fs::create_dir_all(&root)?;
        let error = bounded_workspace_root(&root, std::path::Path::new("../outside"))
            .expect_err("relative escape should fail");
        assert!(error.to_string().contains("escapes workspace root"));
        Ok(())
    }

    #[test]
    fn bounded_workspace_root_rejects_symlink_escape() -> Result<()> {
        let base = test_root("symlink-escape");
        let root = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&outside)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("link"))?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&outside, root.join("link"))?;
        let error = bounded_workspace_root(&root, &root.join("link"))
            .expect_err("symlink escape should fail");
        assert!(error.to_string().contains("escapes workspace root"));
        Ok(())
    }
}
