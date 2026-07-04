use parking_lot::RwLock;
use std::path::PathBuf;
use std::sync::Arc;

use kheish_types::{
    CompletionRequirement, SessionControlState, SessionGoal, SessionPersonaBinding,
    SystemPromptSection, TaskStatus, ToolDefinition,
};
use serde::{Deserialize, Serialize};

/// Controls how an agent-specific prompt is merged with the runtime default prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMergeMode {
    /// Replaces the default prompt with the provided agent prompt.
    #[default]
    Replace,
    /// Appends the agent prompt after the default prompt.
    Append,
}

/// One agent-specific prompt override persisted with a runtime or sidechain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPromptOverride {
    /// The prompt content to merge into the effective system prompt.
    pub prompt: String,
    /// The merge behavior used for this prompt.
    pub mode: PromptMergeMode,
}

/// Mutable runtime settings that customize the effective Kheish system prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPromptSettings {
    /// Replaces every other prompt source when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub override_prompt: Option<String>,
    /// Replaces the default prompt when no override or agent prompt is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_prompt: Option<String>,
    /// Appends extra instructions after the selected base prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_prompt: Option<String>,
    /// Optional language guidance for user-facing responses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Optional output-style guidance appended as its own section.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_style: Option<String>,
}

/// Immutable environment details injected into default system prompt sections.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemPromptEnvironment {
    /// The primary workspace root available to coding tools.
    pub workspace_root: PathBuf,
    /// The default shell exposed by the coding-tool runtime.
    pub shell: String,
    /// Additional directories that are safe to reference in the prompt.
    #[serde(default)]
    pub additional_working_directories: Vec<PathBuf>,
}

impl SystemPromptEnvironment {
    /// Creates a new environment snapshot for prompt generation.
    pub fn new(workspace_root: impl Into<PathBuf>, shell: impl Into<String>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            shell: shell.into(),
            additional_working_directories: Vec::new(),
        }
    }
}

/// Builds provider-neutral system prompt sections with runtime-configurable overrides.
pub struct SystemPromptBuilder {
    env: SystemPromptEnvironment,
    settings: Arc<RwLock<SystemPromptSettings>>,
}

impl SystemPromptBuilder {
    /// Creates a new builder from fixed environment details and mutable settings.
    pub fn new(env: SystemPromptEnvironment, settings: SystemPromptSettings) -> Self {
        Self {
            env,
            settings: Arc::new(RwLock::new(settings)),
        }
    }

    /// Returns the current prompt settings snapshot.
    pub fn settings(&self) -> SystemPromptSettings {
        self.settings.read().clone()
    }

    /// Returns the immutable environment snapshot used for prompt generation.
    pub fn environment(&self) -> &SystemPromptEnvironment {
        &self.env
    }

    /// Replaces the runtime prompt settings.
    pub fn set_settings(&self, settings: SystemPromptSettings) {
        *self.settings.write() = settings;
    }

    /// Builds the effective system prompt sections for one agent turn.
    pub fn build_sections(
        &self,
        tools: &[ToolDefinition],
        session_persona: Option<&SessionPersonaBinding>,
        agent_prompt: Option<&AgentPromptOverride>,
        completion_requirements: &[CompletionRequirement],
        session_control: &SessionControlState,
        session_goal: Option<&SessionGoal>,
    ) -> Vec<SystemPromptSection> {
        let settings = self.settings();
        let override_prompt = non_empty(&settings.override_prompt);
        let mut sections = if let Some(override_prompt) = override_prompt {
            vec![section("override_prompt", override_prompt.to_string())]
        } else {
            match agent_prompt {
                Some(override_prompt)
                    if override_prompt.mode == PromptMergeMode::Replace
                        && !override_prompt.prompt.trim().is_empty() =>
                {
                    vec![section(
                        "agent_prompt",
                        override_prompt.prompt.trim().to_string(),
                    )]
                }
                _ => match non_empty(&settings.custom_prompt) {
                    Some(custom_prompt) => {
                        vec![section("custom_prompt", custom_prompt.to_string())]
                    }
                    None => self.default_sections(tools, &settings, completion_requirements),
                },
            }
        };

        if override_prompt.is_none()
            && let Some(session_persona) =
                session_persona.filter(|binding| !binding.soul.trim().is_empty())
        {
            sections.push(persona_section(session_persona));
        }

        sections.extend(session_control_sections(session_control));
        if let Some(goal) = session_goal {
            sections.push(session_goal_section(goal));
        }

        if let Some(override_prompt) = agent_prompt.filter(|override_prompt| {
            override_prompt.mode == PromptMergeMode::Append
                && !override_prompt.prompt.trim().is_empty()
        }) {
            sections.push(section(
                "agent_prompt",
                format!(
                    "# Custom Agent Instructions\n{}",
                    override_prompt.prompt.trim()
                ),
            ));
        }

        if let Some(append_prompt) = non_empty(&settings.append_prompt) {
            sections.push(section("append_prompt", append_prompt.to_string()));
        }

        sections
    }

    fn default_sections(
        &self,
        tools: &[ToolDefinition],
        settings: &SystemPromptSettings,
        completion_requirements: &[CompletionRequirement],
    ) -> Vec<SystemPromptSection> {
        let mut sections = vec![
            section("intro", default_intro_section()),
            section("system_reminders", default_system_reminders_section()),
            section("doing_tasks", default_doing_tasks_section()),
            section("actions", default_actions_section()),
            section("using_tools", default_using_tools_section()),
            section("tone_style", default_tone_and_style_section()),
            section("output_efficiency", default_output_efficiency_section()),
            section("environment", self.environment_section()),
        ];

        sections.extend(capability_sections(tools));
        if !completion_requirements.is_empty() {
            sections.push(section(
                "completion_criteria",
                completion_criteria_section(completion_requirements),
            ));
        }

        if let Some(language) = non_empty(&settings.language) {
            sections.push(section(
                "language",
                format!(
                    "Respond to the user in {language} unless they explicitly request another language."
                ),
            ));
        }

        if let Some(output_style) = non_empty(&settings.output_style) {
            sections.push(section(
                "output_style",
                format!("# Output Style\n{output_style}"),
            ));
        }

        if !tools.is_empty() {
            sections.push(section("tools", tools_section(tools)));
        }

        sections
    }

    fn environment_section(&self) -> String {
        let mut lines = vec![
            "# Environment".to_string(),
            "You are running in the following environment:".to_string(),
            bullet(format!(
                "Primary working directory: {}",
                self.env.workspace_root.display()
            )),
            bullet(format!(
                "Is a git repository: {}",
                is_git_repository(&self.env.workspace_root)
            )),
            bullet(format!("Platform: {}", std::env::consts::OS)),
            bullet(format!("Shell: {}", self.env.shell)),
        ];

        if !self.env.additional_working_directories.is_empty() {
            lines.push(bullet("Additional working directories:"));
            lines.extend(
                self.env
                    .additional_working_directories
                    .iter()
                    .map(|path| format!("  - {}", path.display())),
            );
        }

        lines.join("\n")
    }
}

fn session_goal_section(goal: &SessionGoal) -> SystemPromptSection {
    let mut lines = vec![
        "# Session Goal".to_string(),
        "This session has one durable long-running goal. Treat it as the objective to continue across runs until it is achieved, paused, or budget-limited.".to_string(),
        format!("- Goal ID: `{}`", goal.goal_id),
        format!("- Status: `{:?}`", goal.status).to_ascii_lowercase(),
        format!("- Objective: {}", goal.objective.trim()),
        format!("- Tokens used: {}", goal.tokens_used),
        format!("- Time used ms: {}", goal.time_used_ms),
    ];
    if let Some(token_budget) = goal.token_budget {
        lines.push(format!("- Token budget: {token_budget}"));
        lines.push(format!(
            "- Remaining tokens: {}",
            goal.remaining_tokens().unwrap_or(0)
        ));
    }
    lines.extend([
        "- Use `get_goal` to inspect current goal progress when needed.".to_string(),
        "- Use `create_goal` only when the user explicitly starts a new goal and this session has no goal yet.".to_string(),
        "- Use `update_goal` with `status: complete` only when the goal is actually achieved.".to_string(),
        "- If the status is `budget_limited`, wrap up concisely and report what remains instead of starting more work.".to_string(),
    ]);
    SystemPromptSection {
        name: "session_goal".to_string(),
        content: lines.join("\n"),
    }
}

fn persona_section(binding: &SessionPersonaBinding) -> SystemPromptSection {
    SystemPromptSection {
        name: "persona".to_string(),
        content: format!(
            "# Persona\nThe following persona snapshot is bound to this session. Treat it as stable identity and behavior guidance for the current session.\n\n## {}\n{}\n\n- Persona ID: `{}`\n- Persona version: `{}`",
            binding.display_name,
            binding.soul.trim(),
            binding.persona_id,
            binding.persona_version
        ),
    }
}

fn section(name: impl Into<String>, content: impl Into<String>) -> SystemPromptSection {
    SystemPromptSection {
        name: name.into(),
        content: content.into(),
    }
}

fn bullet(content: impl Into<String>) -> String {
    format!("- {}", content.into())
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn is_git_repository(workspace_root: &PathBuf) -> bool {
    workspace_root.join(".git").exists()
}

/// Builds one provider-neutral section describing the effective route pinned to the current run.
/// Builds the prompt section describing a session's structured output
/// contract: first-try conformance is the nominal path, the engine's
/// bounded repair turns are only the backstop.
pub fn output_contract_section(
    schema: &kheish_types::StructuredFieldSchema,
) -> SystemPromptSection {
    let rendered =
        serde_json::to_string_pretty(&schema.to_json_schema()).unwrap_or_else(|_| "{}".to_string());
    SystemPromptSection {
        name: "output_contract".to_string(),
        content: [
            "# Output Contract".to_string(),
            "Your FINAL assistant message must be a single JSON value matching this schema."
                .to_string(),
            "No prose, no Markdown fences around it — the raw JSON is delivered verbatim to the configured outputs."
                .to_string(),
            "You may use tools freely during the run; only the final message is the deliverable."
                .to_string(),
            format!("Schema:\n{rendered}"),
        ]
        .join("\n"),
    }
}

pub fn active_route_section(
    provider: Option<&str>,
    model: Option<&str>,
    fallback_model: Option<&str>,
) -> Option<SystemPromptSection> {
    let provider = provider.map(str::trim).filter(|value| !value.is_empty())?;
    let model = model.map(str::trim).filter(|value| !value.is_empty())?;

    let mut lines = vec![
        "# Active Route".to_string(),
        format!(
            "This run is currently routed to provider `{provider}` using model `{model}`."
        ),
        "Treat this routing information as authoritative whenever the user asks which provider or model is handling the current run. Do not guess from earlier conversation context."
            .to_string(),
    ];

    if let Some(fallback_model) = fallback_model
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != model)
    {
        lines.push(format!(
            "If the primary model cannot complete the turn, the daemon may retry the same provider with fallback model `{fallback_model}`."
        ));
    }

    Some(SystemPromptSection {
        name: "active_route".to_string(),
        content: lines.join("\n\n"),
    })
}

fn default_intro_section() -> String {
    [
        "You are Kheish, a daemon-first autonomous agent.",
        "Use the available tools to complete the user's task end to end.",
        "Complete the task fully without gold-plating, but do not leave it half-done.",
        "Adapt to the task: inspect, act, verify when practical, and then report the outcome concisely.",
    ]
    .join("\n")
}

fn default_system_reminders_section() -> String {
    [
        "# System Reminders",
        "- Conversation history may be compacted automatically. Treat summaries and preserved transcript segments as authoritative context.",
        "- Tool results reflect the real environment. Do not invent tool output, file contents, or command results.",
        "- If a task requires action, prefer doing the work with tools over merely describing the next step.",
        "- Avoid promises about future actions when an available tool can perform the action now.",
        "- Do not narrate what you are about to do when a tool can do it now. Just do the work.",
    ]
    .join("\n")
}

fn default_doing_tasks_section() -> String {
    [
        "# Doing Tasks",
        "- Default to direct execution whenever the requested task clearly requires action.",
        "- Keep momentum: inspect, act, validate when practical, and only then summarize.",
        "- Do not stop after analysis when the user asked for execution.",
        "- Before reporting success, confirm that the requested outcome actually exists when you can verify it directly.",
        "- If direct verification is not possible, say that explicitly instead of implying the task was verified.",
        "- When you genuinely need user clarification before you can continue, use the structured clarification tool that is available to you instead of asking an unstructured free-text question in the transcript.",
    ]
    .join("\n")
}

fn default_actions_section() -> String {
    [
        "# Actions",
        "- You may take local, reversible actions without asking first.",
        "- For destructive, irreversible, or external-impact actions, respect the active permission mode and approval flow.",
        "- Match the scope of your actions to the user's request.",
    ]
    .join("\n")
}

fn default_using_tools_section() -> String {
    [
        "# Using Your Tools",
        "- Use tools by their registered names and respect their schemas.",
        "- Prefer parallel tool calls only when the actions are independent and their outputs do not conflict.",
        "- Keep explanatory text between tool calls brief.",
        "- When a tool can complete the next step directly, use it instead of narrating intent.",
        "- Prefer dedicated filesystem tools over shell redirection for local file writes when they are available.",
    ]
    .join("\n")
}

fn default_tone_and_style_section() -> String {
    [
        "# Tone And Style",
        "- Be direct, factual, and concise.",
        "- Prefer high-signal updates over long narration.",
        "- State important assumptions and blockers clearly.",
    ]
    .join("\n")
}

fn default_output_efficiency_section() -> String {
    [
        "# Output Efficiency",
        "- Keep tool-adjacent text short.",
        "- Keep final responses focused on the outcome, what was verified, and any remaining risk.",
        "- Avoid duplicating information the user can already see in the environment.",
    ]
    .join("\n")
}

fn capability_sections(tools: &[ToolDefinition]) -> Vec<SystemPromptSection> {
    let capabilities = detect_capabilities(tools);
    let mut sections = Vec::new();

    if capabilities.uses_local_workspace() {
        sections.push(section(
            "local_workspace",
            local_workspace_section(&capabilities),
        ));
    }

    if capabilities.has_web {
        sections.push(section("web_research", web_research_section()));
    }

    if capabilities.has_user_questions || capabilities.has_parent_clarification {
        sections.push(section("user_questions", user_question_section()));
    }

    if capabilities.has_scheduling {
        sections.push(section("scheduling", scheduling_section()));
    }

    sections
}

fn local_workspace_section(capabilities: &ToolCapabilities) -> String {
    let mut lines = vec![
        "# Local Workspace Tasks".to_string(),
        "- You can inspect and modify artifacts in the local workspace.".to_string(),
        "- When the user asks for a local change, perform the change with tools instead of only stating intent.".to_string(),
        "- After a local change, verify the result with the most direct available tool when practical.".to_string(),
    ];

    if capabilities.has_shell {
        lines.push(
            "- Use shell commands for direct environment inspection or execution, but keep them scoped to the task."
                .to_string(),
        );
    }

    if capabilities.has_filesystem {
        lines.push(
            "- Prefer filesystem tools for deterministic file reads and writes when they are sufficient."
                .to_string(),
        );
    }

    lines.join("\n")
}

fn web_research_section() -> String {
    [
        "# Web Research",
        "- Use web tools when current external information is required.",
        "- Prefer web_search to discover current sources, then use web_fetch only when you need page-level details.",
        "- Distinguish clearly between fetched facts and your own inference.",
        "- When you rely on web_search results or fetched web pages, include a `Sources:` section with relevant markdown hyperlinks in the form `- [Title](URL)`.",
        "- Do not list bare URLs in the `Sources:` section when a title is available.",
    ]
    .join("\n")
}

fn user_question_section() -> String {
    [
        "# User Clarification",
        "- Use `ask_user_question` when you need structured input from the user before continuing.",
        "- If `ask_user_question` is unavailable but `request_parent_clarification` is available, use it to surface the structured question through the parent session and wait for the answer to arrive later through mailbox.",
        "- Ask only when the missing information is genuinely blocking. Do not use it for routine status updates or confirmations you can infer yourself.",
        "- Ask between 1 and 4 concise questions, each with 2 to 4 clear options.",
        "- After the user answers, continue the task directly instead of re-asking the same question in free text.",
        "- `ask_user_question` is intended for the main agent. Subagents should use `request_parent_clarification` when it is available instead of inventing their own mailbox convention.",
    ]
    .join("\n")
}

fn scheduling_section() -> String {
    [
        "# Scheduling And Wakeups",
        "- Use `wake_after` or `wake_at` when work must resume later instead of promising that you will wait or sleep in place.",
        "- Use `schedule_create` only for durable recurring or one-shot jobs that should continue independently of the current run.",
        "- A wake-up or schedule creates new work later; it does not keep the current run alive in memory.",
        "- Keep scheduled messages concrete and self-contained so the later run can continue without guessing hidden context.",
        "- Prefer the smallest durable mechanism that matches the need: one wake-up for one follow-up, recurring schedules only when repetition is genuinely required.",
    ]
    .join("\n")
}

fn completion_criteria_section(requirements: &[CompletionRequirement]) -> String {
    let mut lines = vec![
        "# Completion Criteria".to_string(),
        "You must satisfy the following conditions before treating the task as complete:"
            .to_string(),
    ];
    lines.extend(
        requirements
            .iter()
            .map(|requirement| requirement.prompt_instruction()),
    );
    lines.push(
        "- If the task requires a file output, use the appropriate filesystem tool before sending a final answer."
            .to_string(),
    );
    lines.push(
        "- Do not stop on a sentence such as 'I will create the file now' or 'I am compiling the report' without actually materializing the result."
            .to_string(),
    );
    lines.join("\n")
}

fn session_control_sections(session_control: &SessionControlState) -> Vec<SystemPromptSection> {
    let mut sections = Vec::new();
    if session_control.plan_mode {
        let restore_line = session_control
            .pre_plan_mode
            .as_deref()
            .map(|mode| format!("When plan mode exits, restore the previous permission mode: {mode}."))
            .unwrap_or_else(|| {
                "When plan mode exits, restore the previous permission mode instead of staying in plan mode.".to_string()
            });
        sections.push(section(
            "plan_mode",
            [
                "# Plan Mode",
                "The current session is in planning mode.",
                "Do not make destructive or write-oriented changes until plan mode is explicitly exited.",
                "Prefer analysis, decomposition, todo/task updates, and information gathering while plan mode is active.",
                "When planning is blocked on missing user requirements, use `ask_user_question` if available. Do not use it to ask whether the plan itself should be approved.",
                &restore_line,
            ]
            .join("\n"),
        ));
    }
    if !session_control.todos.is_empty() {
        sections.push(section("todos", todo_section(session_control)));
    }
    if !session_control.tasks.is_empty() || !session_control.archived_tasks.is_empty() {
        sections.push(section("tasks", task_section(session_control)));
    }
    if let Some(plan_artifact) = session_control.plan_artifact.as_ref() {
        sections.push(section(
            "plan_artifact",
            plan_artifact_section(plan_artifact),
        ));
    }
    sections
}

fn plan_artifact_section(plan_artifact: &kheish_types::PlanArtifact) -> String {
    let mut lines = vec!["# Latest Plan".to_string()];
    if let Some(summary) = plan_artifact.summary.as_deref() {
        lines.push(format!("- summary: {}", truncate_inline(summary, 160)));
    }
    lines.push("```text".to_string());
    lines.push(plan_artifact.content.clone());
    lines.push("```".to_string());
    lines.join("\n")
}

fn todo_section(session_control: &SessionControlState) -> String {
    let mut lines = vec!["# Todos".to_string()];
    lines.extend(session_control.todos.iter().map(|todo| {
        format!(
            "- [{}] {} ({})",
            if todo.completed { "x" } else { " " },
            todo.content,
            todo.id
        )
    }));
    lines.join("\n")
}

fn task_section(session_control: &SessionControlState) -> String {
    let mut lines = vec!["# Tasks".to_string()];
    lines.extend(session_control.tasks.iter().map(|task| {
        let owner = task
            .owner_agent_id
            .as_deref()
            .map(|owner| format!(" owner={owner}"))
            .unwrap_or_default();
        let blocked_by = if task.blocked_by.is_empty() {
            String::new()
        } else {
            format!(" blocked_by={}", task.blocked_by.join(","))
        };
        let output = task
            .output
            .as_deref()
            .filter(|output| !output.trim().is_empty())
            .map(|output| format!(" output={}", truncate_inline(output, 120)))
            .unwrap_or_default();
        format!(
            "- {} [{}]{}{}{}: {}",
            task.id,
            render_task_status(&task.status),
            owner,
            blocked_by,
            output,
            task.title
        )
    }));
    // Terminal tasks are archived out of the hot state so the prompt stays
    // bounded by the work in flight; one summary line keeps them discoverable.
    let archived = &session_control.archived_tasks;
    if !archived.is_empty() {
        lines.push(format!(
            "- {} terminal task(s) archived ({} completed, {} failed, {} cancelled) — use task_get or task_list for details.",
            archived.total(),
            archived.completed,
            archived.failed,
            archived.cancelled
        ));
    }
    lines.join("\n")
}

fn render_task_status(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn truncate_inline(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{truncated}…")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ToolCapabilities {
    has_filesystem: bool,
    has_shell: bool,
    has_web: bool,
    has_user_questions: bool,
    has_parent_clarification: bool,
    has_scheduling: bool,
}

impl ToolCapabilities {
    fn uses_local_workspace(self) -> bool {
        self.has_filesystem || self.has_shell
    }
}

fn detect_capabilities(tools: &[ToolDefinition]) -> ToolCapabilities {
    let mut capabilities = ToolCapabilities::default();

    for tool in tools {
        let name = tool.name.as_str();
        match name {
            "read_file" | "write_file" | "edit_file" | "list_files" | "glob_search"
            | "grep_search" => capabilities.has_filesystem = true,
            "bash" => capabilities.has_shell = true,
            "web_fetch" | "web_search" => capabilities.has_web = true,
            "ask_user_question" => capabilities.has_user_questions = true,
            "request_parent_clarification" => capabilities.has_parent_clarification = true,
            "wake_after"
            | "wake_at"
            | "schedule_create"
            | "schedule_list"
            | "schedule_get"
            | "schedule_cancel"
            | "schedule_pause"
            | "schedule_resume"
            | "schedule_trigger_now" => capabilities.has_scheduling = true,
            _ => {
                let lowered = tool.description.to_ascii_lowercase();
                if lowered.contains("file")
                    || lowered.contains("workspace")
                    || lowered.contains("directory")
                {
                    capabilities.has_filesystem = true;
                }
                if lowered.contains("shell") || lowered.contains("command") {
                    capabilities.has_shell = true;
                }
                if lowered.contains("http") || lowered.contains("web") || lowered.contains("url") {
                    capabilities.has_web = true;
                }
                if lowered.contains("structured clarification")
                    || lowered.contains("user input before continuing")
                {
                    capabilities.has_user_questions = true;
                }
                if lowered.contains("parent session")
                    || lowered.contains("parent clarification")
                    || lowered.contains("answer will arrive later through mailbox")
                {
                    capabilities.has_parent_clarification = true;
                }
                if lowered.contains("wake-up")
                    || lowered.contains("wake up later")
                    || lowered.contains("schedule")
                    || lowered.contains("recurring")
                    || lowered.contains("cron")
                {
                    capabilities.has_scheduling = true;
                }
            }
        }
    }

    capabilities
}

fn tools_section(tools: &[ToolDefinition]) -> String {
    let mut lines = vec![
        "# Available Tools".to_string(),
        "Use the following tools when they are the most direct way to complete the task:"
            .to_string(),
    ];
    lines.extend(
        tools
            .iter()
            .map(|tool| bullet(format!("`{}`: {}", tool.name, tool.description))),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        AgentPromptOverride, PromptMergeMode, SystemPromptBuilder, SystemPromptEnvironment,
        SystemPromptSettings, active_route_section,
    };
    use kheish_types::{
        CapabilityScope, CompletionRequirement, SessionControlState, SessionPersonaBinding,
        TaskRecord, TaskStatus, TodoItem, ToolDefinition,
    };
    use serde_json::json;

    fn sample_tools() -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "read_file".to_string(),
            description: "Reads one file".to_string(),
            input_schema: json!({"type": "object"}),
            allows_parallel: true,
        }]
    }

    #[test]
    fn builder_emits_default_sections() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        let sections = builder.build_sections(
            &sample_tools(),
            Some(&SessionPersonaBinding {
                persona_id: "persona-1".to_string(),
                persona_version: 3,
                display_name: "Analyst".to_string(),
                soul: "Reply as Analyst.".to_string(),
                soul_sha256: "hash".to_string(),
                bound_at_ms: 42,
                capability_scope: CapabilityScope::default(),
                default_inline_skills: Vec::new(),
            }),
            None,
            &[],
            &SessionControlState::default(),
            None,
        );

        assert!(sections.iter().any(|section| section.name == "intro"));
        assert!(sections.iter().any(|section| section.name == "environment"));
        assert!(
            sections
                .iter()
                .any(|section| section.name == "local_workspace")
        );
        assert!(sections.iter().any(|section| section.name == "tools"));
    }

    #[test]
    fn builder_uses_general_default_prompt_without_tool_specific_sections() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        let sections =
            builder.build_sections(&[], None, None, &[], &SessionControlState::default(), None);
        let intro = sections
            .iter()
            .find(|section| section.name == "intro")
            .expect("intro section missing");

        assert!(intro.content.contains("autonomous agent"));
        assert!(!intro.content.contains("coding agent"));
        assert!(
            !sections
                .iter()
                .any(|section| section.name == "local_workspace")
        );
        assert!(
            !sections
                .iter()
                .any(|section| section.name == "web_research")
        );
    }

    #[test]
    fn active_route_section_marks_route_as_authoritative() {
        let section = active_route_section(
            Some("xai"),
            Some("grok-4.20-0309-reasoning"),
            Some("grok-4-fast-reasoning"),
        )
        .expect("route section should exist");

        assert_eq!(section.name, "active_route");
        assert!(section.content.contains("provider `xai`"));
        assert!(section.content.contains("model `grok-4.20-0309-reasoning`"));
        assert!(section.content.contains("authoritative"));
        assert!(
            section
                .content
                .contains("fallback model `grok-4-fast-reasoning`")
        );
    }

    #[test]
    fn builder_adds_web_research_section_when_web_tools_are_available() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        let sections = builder.build_sections(
            &[ToolDefinition {
                name: "web_fetch".to_string(),
                description: "Fetches URLs".to_string(),
                input_schema: json!({"type": "object"}),
                allows_parallel: true,
            }],
            None,
            None,
            &[],
            &SessionControlState::default(),
            None,
        );

        assert!(
            sections
                .iter()
                .any(|section| section.name == "web_research")
        );
        assert!(
            sections
                .iter()
                .find(|section| section.name == "web_research")
                .map(|section| section.content.contains("Sources:"))
                .unwrap_or(false)
        );
        assert!(
            !sections
                .iter()
                .any(|section| section.name == "local_workspace")
        );
    }

    #[test]
    fn builder_adds_user_question_section_when_tool_is_available() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        let sections = builder.build_sections(
            &[ToolDefinition {
                name: "ask_user_question".to_string(),
                description: "Ask the user one structured clarification request.".to_string(),
                input_schema: json!({"type": "object"}),
                allows_parallel: false,
            }],
            None,
            None,
            &[],
            &SessionControlState::default(),
            None,
        );

        let user_questions = sections
            .iter()
            .find(|section| section.name == "user_questions")
            .expect("user question section missing");
        assert!(user_questions.content.contains("ask_user_question"));
        assert!(user_questions.content.contains("main agent"));
    }

    #[test]
    fn builder_uses_override_prompt_exclusively() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings {
                override_prompt: Some("override".to_string()),
                ..SystemPromptSettings::default()
            },
        );

        let sections = builder.build_sections(
            &sample_tools(),
            None,
            None,
            &[],
            &SessionControlState::default(),
            None,
        );

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "override_prompt");
        assert_eq!(sections[0].content, "override");
    }

    #[test]
    fn builder_appends_agent_prompt_when_requested() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        let sections = builder.build_sections(
            &sample_tools(),
            None,
            Some(&AgentPromptOverride {
                prompt: "Focus on review.".to_string(),
                mode: PromptMergeMode::Append,
            }),
            &[],
            &SessionControlState::default(),
            None,
        );

        assert!(
            sections
                .iter()
                .any(|section| section.name == "agent_prompt")
        );
        assert!(sections.iter().any(|section| section.name == "intro"));
    }

    #[test]
    fn builder_keeps_persona_when_agent_prompt_replaces_base_prompt() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        let sections = builder.build_sections(
            &sample_tools(),
            Some(&SessionPersonaBinding {
                persona_id: "persona-1".to_string(),
                persona_version: 2,
                display_name: "Reviewer".to_string(),
                soul: "Always review carefully.".to_string(),
                soul_sha256: "abc123".to_string(),
                bound_at_ms: 7,
                capability_scope: CapabilityScope::default(),
                default_inline_skills: Vec::new(),
            }),
            Some(&AgentPromptOverride {
                prompt: "Focus on code review.".to_string(),
                mode: PromptMergeMode::Replace,
            }),
            &[],
            &SessionControlState::default(),
            None,
        );

        assert_eq!(
            sections.first().map(|section| section.name.as_str()),
            Some("agent_prompt")
        );
        assert!(sections.iter().any(|section| section.name == "persona"));
    }

    #[test]
    fn builder_adds_completion_criteria_for_workspace_file_runs() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        let sections = builder.build_sections(
            &sample_tools(),
            None,
            None,
            &[CompletionRequirement::WorkspaceFile {
                path: Some("reports/summary.txt".to_string()),
            }],
            &SessionControlState::default(),
            None,
        );

        let completion = sections
            .iter()
            .find(|section| section.name == "completion_criteria")
            .expect("completion criteria section missing");
        assert!(completion.content.contains("reports/summary.txt"));
        assert!(completion.content.contains("Do not stop on a sentence"));
    }

    #[test]
    fn builder_adds_session_control_sections() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        let sections = builder.build_sections(
            &sample_tools(),
            None,
            None,
            &[],
            &SessionControlState {
                plan_mode: true,
                pre_plan_mode: Some("default".to_string()),
                todos: vec![TodoItem {
                    id: "todo-1".to_string(),
                    content: "Inspect the repo".to_string(),
                    completed: false,
                }],
                tasks: vec![TaskRecord {
                    id: "task-1".to_string(),
                    title: "Write report".to_string(),
                    description: "Produce the report".to_string(),
                    status: TaskStatus::InProgress,
                    owner_agent_id: Some("agent-2".to_string()),
                    blocked_by: Vec::new(),
                    blocks: Vec::new(),
                    output: None,
                    metadata: serde_json::Value::Null,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                }],
                ..SessionControlState::default()
            },
            None,
        );

        assert!(sections.iter().any(|section| section.name == "plan_mode"));
        assert!(sections.iter().any(|section| section.name == "todos"));
        assert!(sections.iter().any(|section| section.name == "tasks"));
    }

    #[test]
    fn builder_summarizes_archived_tasks_instead_of_rendering_them() {
        let builder = SystemPromptBuilder::new(
            SystemPromptEnvironment::new("/workspace", "/bin/bash"),
            SystemPromptSettings::default(),
        );

        // Only archived work: the section still appears, as one summary line.
        let sections = builder.build_sections(
            &sample_tools(),
            None,
            None,
            &[],
            &SessionControlState {
                archived_tasks: kheish_types::ArchivedTaskCounts {
                    completed: 12,
                    failed: 2,
                    cancelled: 1,
                },
                ..SessionControlState::default()
            },
            None,
        );

        let tasks = sections
            .iter()
            .find(|section| section.name == "tasks")
            .expect("archived work must keep the tasks section");
        assert!(
            tasks
                .content
                .contains("15 terminal task(s) archived (12 completed, 2 failed, 1 cancelled)"),
            "unexpected tasks section: {}",
            tasks.content
        );
        assert!(tasks.content.contains("task_get or task_list"));
    }
}
