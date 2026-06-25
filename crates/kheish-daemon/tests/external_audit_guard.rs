use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[test]
fn external_action_boundaries_use_fail_closed_observer_api() -> Result<()> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .context("failed to resolve repository root")?
        .to_path_buf();
    let mut offenders = Vec::new();
    for crate_name in ["kheish-runtime", "kheish-daemon", "kheish-mcp"] {
        collect_direct_external_action_records(
            &repo_root.join("crates").join(crate_name).join("src"),
            &mut offenders,
        )?;
    }

    assert!(
        offenders.is_empty(),
        "external action audit traces must use record_external_action(...), not record(...):\n{}",
        offenders.join("\n")
    );
    Ok(())
}

fn collect_direct_external_action_records(root: &Path, offenders: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_direct_external_action_records(&path, offenders)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("external_audit_guard.rs") {
            continue;
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for (index, line) in content.lines().enumerate() {
            if line.contains(".record(external_action_trace(") {
                offenders.push(format!("{}:{}", path.display(), index + 1));
            }
            if line.contains("audit.record_success(") && !line.trim_end().ends_with("?;") {
                offenders.push(format!("{}:{}", path.display(), index + 1));
            }
            if line.contains("audit.record_failure(") && !line.trim_end().ends_with("?;") {
                offenders.push(format!("{}:{}", path.display(), index + 1));
            }
        }
    }
    Ok(())
}
