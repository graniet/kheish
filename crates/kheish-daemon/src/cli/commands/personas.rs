//! Persona command handlers.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

/// Handles `personas ...`.
pub(crate) async fn run_personas_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::PersonasCommand,
) -> Result<()> {
    match command {
        crate::PersonasCommand::List { query, pagination } => {
            let mut path = "/v1/personas".to_string();
            let mut params = Vec::new();
            if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
                params.push(format!(
                    "query={}",
                    crate::cli::url_encode_component(&query)
                ));
            }
            pagination.append_query_params(&mut params);
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            if pagination.wants_page() {
                let personas = client
                    .get_list_page_compat::<kheish_daemon::PersonaSummaryView>(&path)
                    .await?;
                printer.print(&personas)
            } else {
                let personas = client
                    .get_json::<Vec<kheish_daemon::PersonaSummaryView>>(&path)
                    .await?;
                printer.print(&personas)
            }
        }
        crate::PersonasCommand::Create(args) => {
            let soul =
                crate::cli::read_text_input(args.soul, args.soul_file.as_deref(), args.stdin)
                    .await?;
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?;
            let capability_scope =
                crate::cli::read_optional_typed_json_input::<kheish_types::CapabilityScope>(
                    args.capability_scope_json.as_deref(),
                    args.capability_scope_file.as_deref(),
                )
                .await?;
            let default_skills = crate::cli::read_optional_typed_json_input::<
                Vec<kheish_types::PersonaSkillAssignment>,
            >(
                args.default_skills_json.as_deref(),
                args.default_skills_file.as_deref(),
            )
            .await?;
            let persona = create_persona_via_api(
                client,
                kheish_daemon::CreatePersonaRequest {
                    persona_id: args.persona_id,
                    display_name: args.display_name,
                    soul,
                    metadata,
                    capability_scope,
                    default_skills,
                },
            )
            .await?;
            printer.print(&persona)
        }
        crate::PersonasCommand::Import(args) => {
            let imported = read_persona_markdown_import(&args.path).await?;
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?;
            let capability_scope =
                crate::cli::read_optional_typed_json_input::<kheish_types::CapabilityScope>(
                    args.capability_scope_json.as_deref(),
                    args.capability_scope_file.as_deref(),
                )
                .await?;
            let default_skills = crate::cli::read_optional_typed_json_input::<
                Vec<kheish_types::PersonaSkillAssignment>,
            >(
                args.default_skills_json.as_deref(),
                args.default_skills_file.as_deref(),
            )
            .await?;
            let persona = create_persona_via_api(
                client,
                kheish_daemon::CreatePersonaRequest {
                    persona_id: args.persona_id,
                    display_name: args.display_name.unwrap_or(imported.display_name),
                    soul: imported.soul,
                    metadata,
                    capability_scope,
                    default_skills,
                },
            )
            .await?;
            printer.print(&persona)
        }
        crate::PersonasCommand::Get { persona_id } => {
            let persona_id = crate::cli::url_encode_path_segment(&persona_id);
            let persona = client
                .get_json::<kheish_daemon::PersonaView>(&format!("/v1/personas/{persona_id}"))
                .await?;
            printer.print(&persona)
        }
        crate::PersonasCommand::Update(args) => {
            let soul = crate::cli::read_optional_text_input(
                args.soul,
                args.soul_file.as_deref(),
                args.stdin,
            )
            .await?;
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?;
            let capability_scope =
                crate::cli::read_optional_typed_json_input::<kheish_types::CapabilityScope>(
                    args.capability_scope_json.as_deref(),
                    args.capability_scope_file.as_deref(),
                )
                .await?;
            let default_skills = crate::cli::read_optional_typed_json_input::<
                Vec<kheish_types::PersonaSkillAssignment>,
            >(
                args.default_skills_json.as_deref(),
                args.default_skills_file.as_deref(),
            )
            .await?;
            let persona_id = crate::cli::url_encode_path_segment(&args.persona_id);
            let persona = client
                .put_json::<_, kheish_daemon::PersonaView>(
                    &format!("/v1/personas/{persona_id}"),
                    &kheish_daemon::UpdatePersonaRequest {
                        display_name: args.display_name,
                        soul,
                        metadata,
                        capability_scope,
                        default_skills,
                    },
                )
                .await?;
            printer.print(&persona)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportedPersonaMarkdown {
    pub(crate) display_name: String,
    pub(crate) soul: String,
}

/// Reads one persona Markdown file and derives a default display name from it.
pub(crate) async fn read_persona_markdown_import(path: &Path) -> Result<ImportedPersonaMarkdown> {
    ensure_markdown_persona_path(path)?;
    let soul = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let trimmed = soul.trim();
    if trimmed.is_empty() {
        bail!("persona markdown file {} is empty", path.display());
    }
    Ok(ImportedPersonaMarkdown {
        display_name: derive_persona_display_name_from_markdown(&soul, path)?,
        soul,
    })
}

pub(crate) fn ensure_markdown_persona_path(path: &Path) -> Result<()> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    if matches!(extension.as_deref(), Some("md" | "markdown")) {
        return Ok(());
    }
    bail!(
        "persona import requires a .md or .markdown file, got {}",
        path.display()
    )
}

pub(crate) fn derive_persona_display_name_from_markdown(
    content: &str,
    path: &Path,
) -> Result<String> {
    for line in content.lines() {
        let trimmed = line.trim_start_matches('\u{feff}').trim();
        if !trimmed.starts_with('#') {
            continue;
        }
        let heading = trimmed.trim_start_matches('#').trim();
        if !heading.is_empty() {
            return Ok(heading.to_string());
        }
    }

    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("persona import path must include a valid file stem"))?;
    Ok(stem.to_string())
}

async fn create_persona_via_api(
    client: &crate::cli::DaemonHttpClient,
    request: kheish_daemon::CreatePersonaRequest,
) -> Result<kheish_daemon::PersonaView> {
    client
        .post_json::<_, kheish_daemon::PersonaView>("/v1/personas", &request)
        .await
}
