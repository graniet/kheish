//! KheishStack CLI transport.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use kheish_daemon::{
    StackApplyReport, StackApplyRequest, StackDownReport, StackDownRequest, StackImportReport,
    StackImportRequest, StackManifestRequest, StackPlan, StackPlanRequest, StackValidation,
    StackVerificationReport,
};

/// Handles `stack ...`.
pub(crate) async fn run_stack_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::StackCommand,
) -> Result<()> {
    match command {
        crate::StackCommand::Init(args) => run_stack_init(args).await,
        crate::StackCommand::Validate(args) => {
            reject_state_root_override(args.state_root.as_deref())?;
            let request = stack_manifest_request(&args.file, !args.no_strict_scopes).await?;
            let validation = client
                .post_json::<_, StackValidation>("/v1/stacks/validate", &request)
                .await?;
            let valid = validation.valid;
            printer.print(&validation)?;
            if valid {
                Ok(())
            } else {
                bail!("KheishStack validation failed")
            }
        }
        crate::StackCommand::Plan(args) => {
            reject_state_root_override(args.file.state_root.as_deref())?;
            let request = StackPlanRequest {
                stack: stack_manifest_request(&args.file.file, !args.file.no_strict_scopes).await?,
                only_changes: args.only_changes,
                allow_secret_env: args.allow_secret_env,
            };
            let plan = client
                .post_json::<_, StackPlan>("/v1/stacks/plan", &request)
                .await?;
            printer.print(&plan)
        }
        crate::StackCommand::Diff(args) => {
            reject_state_root_override(args.file.state_root.as_deref())?;
            let request = StackPlanRequest {
                stack: stack_manifest_request(&args.file.file, !args.file.no_strict_scopes).await?,
                only_changes: true,
                allow_secret_env: args.allow_secret_env,
            };
            let plan = client
                .post_json::<_, StackPlan>("/v1/stacks/plan", &request)
                .await?;
            printer.print(&plan)
        }
        crate::StackCommand::Apply(args) => {
            reject_state_root_override(args.file.state_root.as_deref())?;
            if args.dry_run {
                let request = StackApplyRequest {
                    stack: stack_manifest_request(&args.file.file, !args.file.no_strict_scopes)
                        .await?,
                    dry_run: true,
                    allow_secret_env: args.allow_secret_env,
                    force_restart: args.force_restart,
                    prune: args.prune,
                };
                let report = client
                    .post_json::<_, StackApplyReport>("/v1/stacks/apply", &request)
                    .await?;
                return printer.print(&report);
            }
            let request = StackApplyRequest {
                stack: stack_manifest_request(&args.file.file, !args.file.no_strict_scopes).await?,
                dry_run: false,
                allow_secret_env: args.allow_secret_env,
                force_restart: args.force_restart,
                prune: args.prune,
            };
            let report = client
                .post_json::<_, StackApplyReport>("/v1/stacks/apply", &request)
                .await?;
            printer.print(&report)
        }
        crate::StackCommand::Verify(args) => {
            reject_state_root_override(args.state_root.as_deref())?;
            let request = stack_manifest_request(&args.file, !args.no_strict_scopes).await?;
            let report = client
                .post_json::<_, StackVerificationReport>("/v1/stacks/verify", &request)
                .await?;
            let valid = report.valid;
            printer.print(&report)?;
            if valid {
                Ok(())
            } else {
                bail!("KheishStack verification failed")
            }
        }
        crate::StackCommand::Import(args) => {
            reject_state_root_override(args.file.state_root.as_deref())?;
            let request = StackImportRequest {
                stack: stack_manifest_request(&args.file.file, !args.file.no_strict_scopes).await?,
                resources: args.resources,
            };
            let report = client
                .post_json::<_, StackImportReport>("/v1/stacks/import", &request)
                .await?;
            printer.print(&report)
        }
        crate::StackCommand::Down(args) => {
            reject_state_root_override(args.file.state_root.as_deref())?;
            let request = StackDownRequest {
                stack: stack_manifest_request(&args.file.file, !args.file.no_strict_scopes).await?,
                yes: args.yes,
            };
            let report = client
                .post_json::<_, StackDownReport>("/v1/stacks/down", &request)
                .await?;
            printer.print(&report)
        }
    }
}

async fn run_stack_init(args: crate::StackInitArgs) -> Result<()> {
    if args.output_file.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            args.output_file.display()
        );
    }
    let rendered = kheish_daemon::generic_stack_template(&args.name);
    if let Some(parent) = args
        .output_file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    tokio::fs::write(&args.output_file, rendered)
        .await
        .with_context(|| format!("failed to write {}", args.output_file.display()))?;
    println!("{}", args.output_file.display());
    Ok(())
}

async fn stack_manifest_request(file: &Path, strict_scopes: bool) -> Result<StackManifestRequest> {
    let manifest = read_stack_manifest_file(file).await?;
    let root = file_root(file);
    let manifest = resolve_manifest_file_refs(&manifest, &root)?;
    Ok(StackManifestRequest {
        manifest,
        file_root: None,
        strict_scopes: Some(strict_scopes),
    })
}

fn file_root(file: &Path) -> PathBuf {
    let parent = file
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf())
}

fn reject_state_root_override(state_root: Option<&Path>) -> Result<()> {
    if let Some(state_root) = state_root {
        bail!(
            "--state-root={} is no longer accepted for stack commands; the daemon API owns the ledger under its configured state root",
            state_root.display()
        );
    }
    Ok(())
}

async fn read_stack_manifest_file(file: &Path) -> Result<String> {
    let metadata = tokio::fs::metadata(file)
        .await
        .with_context(|| format!("failed to stat {}", file.display()))?;
    ensure_stack_file_size(file, metadata.len())?;
    let raw = tokio::fs::read_to_string(file)
        .await
        .with_context(|| format!("failed to read {}", file.display()))?;
    ensure_stack_content_size(file, &raw)?;
    Ok(raw)
}

fn resolve_manifest_file_refs(raw: &str, root: &Path) -> Result<String> {
    kheish_daemon::validate_stack_manifest_source(raw)?;
    let mut budget = FileRefExpansionBudget::new(raw);
    let mut document =
        serde_yaml::from_str::<serde_yaml::Value>(raw).context("failed to parse KheishStack")?;
    if let Some(spec) = mapping_mut(&mut document, "spec") {
        if let Some(serde_yaml::Value::Sequence(personas)) = spec.get_mut(key("personas")) {
            for persona in personas {
                if let Some(map) = persona.as_mapping_mut()
                    && let Some(path) = take_string(map, "soul_file")?
                {
                    if map.contains_key(key("soul")) {
                        bail!("persona must use either soul or soul_file, not both");
                    }
                    map.insert(
                        key("soul"),
                        serde_yaml::Value::String(read_stack_file(root, &path, &mut budget)?),
                    );
                }
            }
        }
        if let Some(serde_yaml::Value::Sequence(schedules)) = spec.get_mut(key("schedules")) {
            for schedule in schedules {
                let Some(schedule) = schedule.as_mapping_mut() else {
                    continue;
                };
                if let Some(request) = schedule
                    .get_mut(key("request"))
                    .and_then(serde_yaml::Value::as_mapping_mut)
                {
                    resolve_stack_request_file_ref(root, request, &mut budget)?;
                }
                if let Some(request) = schedule
                    .get_mut(key("flow_start"))
                    .and_then(serde_yaml::Value::as_mapping_mut)
                    .and_then(|flow_start| flow_start.get_mut(key("request")))
                    .and_then(serde_yaml::Value::as_mapping_mut)
                {
                    resolve_stack_request_file_ref(root, request, &mut budget)?;
                }
            }
        }
        if let Some(serde_yaml::Value::Sequence(playbooks)) = spec.get_mut(key("playbooks")) {
            for playbook in playbooks {
                if let Some(map) = playbook.as_mapping_mut()
                    && let Some(path) = take_string(map, "manifest_file")?
                {
                    if map.contains_key(key("manifest")) {
                        bail!("playbook must use either manifest or manifest_file");
                    }
                    let raw = read_stack_file(root, &path, &mut budget)?;
                    kheish_daemon::validate_stack_manifest_source(&raw)?;
                    let manifest = serde_yaml::from_str::<serde_yaml::Value>(&raw)
                        .with_context(|| format!("failed to parse playbook manifest {path}"))?;
                    map.insert(key("manifest"), manifest);
                }
            }
        }
    }
    let rendered =
        serde_yaml::to_string(&document).context("failed to render resolved KheishStack")?;
    kheish_daemon::validate_stack_manifest_source(&rendered)?;
    Ok(rendered)
}

fn resolve_stack_request_file_ref(
    root: &Path,
    request: &mut serde_yaml::Mapping,
    budget: &mut FileRefExpansionBudget,
) -> Result<()> {
    if let Some(path) = take_string(request, "content_file")? {
        if request.contains_key(key("content")) {
            bail!("schedule request must use either content or content_file");
        }
        request.insert(
            key("content"),
            serde_yaml::Value::String(read_stack_file(root, &path, budget)?),
        );
    }
    Ok(())
}

struct FileRefExpansionBudget {
    total_bytes: usize,
}

impl FileRefExpansionBudget {
    fn new(raw_manifest: &str) -> Self {
        Self {
            total_bytes: raw_manifest.len(),
        }
    }

    fn add_file(&mut self, path: &Path, bytes: u64) -> Result<()> {
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        let Some(next_total) = self.total_bytes.checked_add(bytes) else {
            bail!(
                "resolved KheishStack file references exceed the {} byte aggregate limit before rendering",
                kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES
            );
        };
        if next_total > kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES {
            bail!(
                "{} would expand resolved KheishStack file references to {} bytes, exceeding the {} byte aggregate limit",
                path.display(),
                next_total,
                kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES
            );
        }
        self.total_bytes = next_total;
        Ok(())
    }
}

fn mapping_mut<'a>(
    value: &'a mut serde_yaml::Value,
    field: &str,
) -> Option<&'a mut serde_yaml::Mapping> {
    value
        .as_mapping_mut()?
        .get_mut(key(field))?
        .as_mapping_mut()
}

fn take_string(map: &mut serde_yaml::Mapping, field: &str) -> Result<Option<String>> {
    let Some(value) = map.remove(key(field)) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(ToOwned::to_owned)
        .map(Some)
        .ok_or_else(|| anyhow::anyhow!("{field} must be a string"))
}

fn key(field: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(field.to_string())
}

fn read_stack_file(root: &Path, path: &str, budget: &mut FileRefExpansionBudget) -> Result<String> {
    let path = Path::new(path);
    if path.is_absolute() {
        bail!(
            "absolute stack file reference {} is not allowed",
            path.display()
        );
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let resolved = root.join(path);
    let canonical = resolved
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", resolved.display()))?;
    if !canonical.starts_with(&root) {
        bail!(
            "stack file reference {} escapes {}",
            path.display(),
            root.display()
        );
    }
    let metadata = std::fs::metadata(&canonical)
        .with_context(|| format!("failed to stat {}", canonical.display()))?;
    ensure_stack_file_size(&canonical, metadata.len())?;
    budget.add_file(&canonical, metadata.len())?;
    let raw = std::fs::read_to_string(&canonical)
        .with_context(|| format!("failed to read {}", canonical.display()))?;
    ensure_stack_content_size(&canonical, &raw)?;
    Ok(raw)
}

fn ensure_stack_file_size(path: &Path, bytes: u64) -> Result<()> {
    if bytes > kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES as u64 {
        bail!(
            "{} exceeds the {} byte KheishStack file limit",
            path.display(),
            kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES
        );
    }
    Ok(())
}

fn ensure_stack_content_size(path: &Path, raw: &str) -> Result<()> {
    if raw.len() > kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES {
        bail!(
            "{} exceeds the {} byte KheishStack file limit",
            path.display(),
            kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_manifest_file_refs_rejects_oversized_reference() {
        let temp = tempfile::tempdir().expect("tempdir");
        let soul = temp.path().join("soul.md");
        std::fs::write(
            &soul,
            "x".repeat(kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES + 1),
        )
        .expect("write soul");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: oversized-ref
spec:
  personas:
    - persona_id: oversized
      display_name: Oversized
      soul_file: soul.md
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;

        let error = resolve_manifest_file_refs(raw, temp.path()).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("KheishStack file limit"), "{message}");
    }

    #[test]
    fn resolve_manifest_file_refs_embeds_flow_start_request_content_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("prompt.md"), "run scheduled flow").expect("write prompt");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: scheduled-flow
spec:
  schedules:
    - name: scheduled-flow
      target_session_id: session-1
      cadence:
        type: once
        fire_at_ms: 4102444800000
      flow_start:
        playbook_ref:
          playbook_id: feature-flow
          version: "1"
        session_id: session-1
        request:
          content_file: prompt.md
"#;

        let rendered = resolve_manifest_file_refs(raw, temp.path()).expect("resolve refs");
        let document = serde_yaml::from_str::<serde_yaml::Value>(&rendered).expect("rendered yaml");
        let content = document["spec"]["schedules"][0]["flow_start"]["request"]["content"]
            .as_str()
            .expect("embedded content");

        assert_eq!(content, "run scheduled flow");
        assert!(
            document["spec"]["schedules"][0]["flow_start"]["request"]["content_file"].is_null()
        );
    }

    #[test]
    fn resolve_manifest_file_refs_revalidates_rendered_manifest_size() {
        let temp = tempfile::tempdir().expect("tempdir");
        let soul = temp.path().join("soul.md");
        std::fs::write(
            &soul,
            "x".repeat(kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES - 32),
        )
        .expect("write soul");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: rendered-too-large
spec:
  personas:
    - persona_id: rendered
      display_name: Rendered
      soul_file: soul.md
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;

        let error = resolve_manifest_file_refs(raw, temp.path()).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("aggregate limit"), "{message}");
    }

    #[test]
    fn resolve_manifest_file_refs_rejects_aggregate_file_refs_before_rendering() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("first.md"),
            "x".repeat(kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES / 2),
        )
        .expect("write first");
        std::fs::write(
            temp.path().join("second.md"),
            "y".repeat(kheish_daemon::STACK_MANIFEST_BODY_LIMIT_BYTES / 2),
        )
        .expect("write second");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: aggregate-too-large
spec:
  personas:
    - persona_id: first
      display_name: First
      soul_file: first.md
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
    - persona_id: second
      display_name: Second
      soul_file: second.md
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
"#;

        let error = resolve_manifest_file_refs(raw, temp.path()).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("aggregate limit") && message.contains("second.md"),
            "{message}"
        );
    }

    #[test]
    fn resolve_manifest_file_refs_rejects_playbook_manifest_anchors() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("playbook.yaml"),
            r#"
playbook_id: anchored
version: "1.0.0"
title: &title Anchored
description: Demo
steps: []
"#,
        )
        .expect("write playbook");
        let raw = r#"
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: anchored-playbook
spec:
  playbooks:
    - manifest_file: playbook.yaml
"#;

        let error = resolve_manifest_file_refs(raw, temp.path()).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("anchors and aliases are disabled"),
            "{message}"
        );
    }
}
