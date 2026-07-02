use std::fmt;
use std::io::IsTerminal as _;
use std::net::SocketAddr;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

#[cfg(test)]
use anyhow::Result;
#[cfg(test)]
use anyhow::anyhow;
use clap::{Args, Parser, Subcommand, ValueEnum};
use kheish_agent::ChildRetentionPolicy;
#[cfg(test)]
#[allow(unused_imports)]
use kheish_auth::{
    AnthropicAuthBackend, AuthManager, AuthProvider, AuthSlotId, DEFAULT_ANTHROPIC_OAUTH_TOKEN_URL,
    DEFAULT_CLAUDE_CODE_CLIENT_ID, DEFAULT_OPENAI_CODEX_API_BASE_URL, FileAuthStore,
    OpenAiAuthBackend, default_claude_code_credentials_path,
};
#[cfg(test)]
#[allow(unused_imports)]
use kheish_daemon::{
    AdditionalImageBackendConfig, AdditionalTranscriptionBackendConfig, ConfiguredModelRoute,
    ControlPlaneAuthConfig, ControlPlaneCorsConfig, ModelRouteConfig,
};
use kheish_daemon::{
    CaptureOsProfile, ChannelMemberDisplayNameMode, ChannelMemberKind, ChannelParticipationMode,
    ChannelStimulusKind, ChannelStimulusScope, ChannelStimulusState, ChannelStimulusVisibilityHint,
    DEFAULT_EVENT_HISTORY_CAPACITY, DaemonConfig, DaemonEvent, DerivationProfile,
    ObservationRawAssetPolicy, ObservationSensitivity, ObservationSourceKind, ProjectStatus,
    SchedulerPolicyConfig, SubagentPolicyConfig,
};
use kheish_mcp::{default_codex_credentials_path, default_codex_mcp_config_path};
#[cfg(test)]
#[allow(unused_imports)]
use kheish_runtime::{
    AnthropicProviderConfig, GoogleProviderConfig, OpenAiProviderConfig, XAiProviderConfig,
    resolve_google_image_model, resolve_openai_image_model, resolve_xai_image_model,
};
use kheish_types::{ApprovalResolution, TaskStatus};
use serde::Deserialize;
use serde::Serialize;

#[cfg(test)]
#[allow(unused_imports)]
use kheish_auth::{AUTH_STORE_MASTER_KEY_ENV, generate_auth_store_master_key_base64};
#[cfg(test)]
#[allow(unused_imports)]
use kheish_daemon::{
    AssetSummaryView, AssetView, BoardListQuery, BoardRevisionView, BoardView, ChannelListQuery,
    ChannelMemberRequest, ChannelMessageListQuery, ChannelStimulusListQuery, ChannelStimulusView,
    ChannelThreadWorkListQuery, ChannelThreadWorkStateView, ChannelTurnLeaseView, ChannelView,
    ConnectorView, CreateAssetRequest, CreateBoardRequest, CreateBoardRevisionRequest,
    CreateChannelRequest, CreateChannelStimulusRequest, CreateDerivationRequest,
    CreatePersonaRequest, CreateProjectRequest, CreateProjectTaskRequest, CreateScheduleRequest,
    CreateSessionRequest, DerivationSubject, DerivationView, EndSessionRequest, InlineAssetUpload,
    InputAttachmentRequest, InterruptSessionResponse, ObservationMaterializationRequest,
    ObservationSelection, ObservationSourceView, ObservationView, PendingQuestionView,
    PersonaListQuery, PersonaSummaryView, PersonaView, PostChannelMessageRequest,
    ProjectChannelLinkRequest, ProjectListQuery, ProjectMemberRequest,
    ProjectTaskAssignmentRequest, ProjectTaskListQuery, ProjectTaskView, ProjectView,
    PutExternalConnectorRequest, PutHttpConnectorRequest, PutSlackConnectorRequest,
    PutTelegramConnectorRequest, ResolveApprovalsRequest, RunView, ScheduleCadence,
    ScheduleMisfirePolicy, ScheduleOverlapPolicy, SessionEventLogView, SessionView,
    SessionViewSummary, SetChannelReactionRequest, SetSessionCapabilityScopeRequest,
    SetSessionPersonaRequest, SetSessionReplyTargetsRequest, SetSessionRoutePolicyRequest,
    SidechainSubtaskRequest, StartProjectTaskRequest, UpdateBoardRequest, UpdateChannelRequest,
    UpdatePersonaRequest, UpdateProjectRequest, UpdateProjectTaskRequest,
};
#[cfg(test)]
#[allow(unused_imports)]
use kheish_runtime::{DebugCaptureLevel, ModelGenerationConfig, PermissionMode};
#[cfg(test)]
#[allow(unused_imports)]
use kheish_types::{
    ApprovalRequest, ApprovalResolutionBehavior, CapabilityScope, PersonaSkillAssignment,
    ResponseFormat, SessionRoutePolicy, StructuredFieldSchema, ToolChoice, UserQuestionAnswer,
};

mod cli;
mod logging;
mod route_file;

#[cfg(test)]
#[allow(unused_imports)]
use cli::{
    default_codex_auth_path, ensure_secret_manager_master_key_configured, global_auth_store_path,
    read_secret_arg,
};
use logging::{LogFormat, LogLevel};
#[cfg(test)]
#[allow(unused_imports)]
use route_file::{
    RouteFileAnthropicAuthSource, RouteFileDriver, RouteFileEntry, RouteFileOpenAiAuthSource,
    RoutesFileConfig,
};

const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:4000";
const DEFAULT_BIND: &str = "127.0.0.1:4000";
const DEFAULT_STATE_ROOT: &str = ".kheish-daemon";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-4-6";
const DEFAULT_GOOGLE_MODEL: &str = "gemini-2.5-flash";
const DEFAULT_OPENAI_MODEL: &str = "gpt-5.4";
const DEFAULT_OPENROUTER_MODEL: &str = "openai/gpt-5.4-mini";
const DEFAULT_XAI_MODEL: &str = "grok-4-fast-reasoning";

#[derive(Parser, Debug)]
#[command(
    name = "kheish-daemon",
    version,
    about = "Daemon control plane and operator CLI for Kheish"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "KHEISH_DAEMON_URL",
        default_value = DEFAULT_DAEMON_URL,
        help = "Base URL used by control-plane commands"
    )]
    base_url: String,
    #[arg(
        long,
        global = true,
        env = "KHEISH_DAEMON_TOKEN",
        hide_env_values = true,
        help = "Bearer token used by control-plane commands"
    )]
    token: Option<String>,
    #[arg(
        long,
        global = true,
        env = "KHEISH_DAEMON_TOKEN_FILE",
        help = "File containing the bearer token used by control-plane commands"
    )]
    token_file: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = OutputFormat::Pretty,
        help = "Output encoding used by non-stream commands"
    )]
    output: OutputFormat,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the daemon HTTP server.
    #[command(visible_alias = "start")]
    Serve(ServeArgs),
    /// Fetch a combined capabilities and runtime snapshot.
    Status,
    /// Run connectivity and control-plane diagnostics.
    Doctor {
        /// Probe whether one browser Origin can call the daemon API.
        #[arg(long)]
        cors_origin: Option<String>,
        #[command(subcommand)]
        command: Option<DoctorCommand>,
    },
    /// Fetch daemon capabilities.
    Capabilities,
    /// Inspect or mutate runtime configuration.
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    /// Inspect built-in MCP catalog entries and profiles.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Follow the daemon SSE event stream.
    Events {
        #[command(subcommand)]
        command: EventsCommand,
    },
    /// Manage daemon-owned assets and documents.
    Assets {
        #[command(subcommand)]
        command: AssetsCommand,
    },
    /// Manage daemon-owned boards and immutable visual revisions.
    #[command(visible_alias = "board")]
    Boards {
        #[command(subcommand)]
        command: BoardsCommand,
    },
    /// Manage daemon-owned shared channels and public message logs.
    #[command(visible_alias = "channel")]
    Channels {
        #[command(subcommand)]
        command: ChannelsCommand,
    },
    /// Manage daemon-owned projects, members, linked channels, and project tasks.
    #[command(visible_alias = "project")]
    Projects {
        #[command(subcommand)]
        command: ProjectsCommand,
    },
    /// Manage daemon-owned Playbook definitions.
    #[command(visible_alias = "playbook")]
    Playbooks {
        #[command(subcommand)]
        command: PlaybooksCommand,
    },
    /// Reconcile daemon resources from a KheishStack file.
    #[command(visible_alias = "kheishfile")]
    Stack {
        #[command(subcommand)]
        command: StackCommand,
    },
    /// Start and inspect Flow projections over normal daemon runs.
    #[command(visible_alias = "flow")]
    Flows {
        #[command(subcommand)]
        command: FlowsCommand,
    },
    /// Manage daemon-owned derived artifacts.
    #[command(visible_alias = "derivation")]
    Derivations {
        #[command(subcommand)]
        command: DerivationsCommand,
    },
    /// Manage daemon-owned learning candidates and durable published learnings.
    #[command(visible_alias = "learning")]
    Learnings {
        #[command(subcommand)]
        command: LearningsCommand,
    },
    /// Manage daemon-owned observation sources and captured records.
    #[command(visible_alias = "observation")]
    Observations {
        #[command(subcommand)]
        command: ObservationsCommand,
    },
    /// Provision host-local capture agents.
    Capture {
        #[command(subcommand)]
        command: CaptureCommand,
    },
    /// Manage daemon sessions.
    #[command(visible_alias = "session")]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// Manage daemon personas.
    #[command(visible_alias = "persona")]
    Personas {
        #[command(subcommand)]
        command: PersonasCommand,
    },
    /// Manage daemon-owned auth secrets and imported account credentials.
    #[command(visible_alias = "secret")]
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// Manage runtime ingress and egress connectors.
    #[command(visible_alias = "connector")]
    Connectors {
        #[command(subcommand)]
        command: ConnectorsCommand,
    },
    /// Inspect and replay queued output deliveries.
    #[command(visible_alias = "delivery")]
    Deliveries {
        #[command(subcommand)]
        command: DeliveriesCommand,
    },
    /// Resolve pending approvals.
    #[command(visible_alias = "approval")]
    Approvals {
        #[command(subcommand)]
        command: ApprovalsCommand,
    },
    /// Resolve pending structured user questions.
    #[command(visible_alias = "question")]
    Questions {
        #[command(subcommand)]
        command: QuestionsCommand,
    },
    /// Inspect or control detached background runs.
    Run {
        #[command(subcommand)]
        command: RunsCommand,
    },
    /// Inspect or control detached background runs.
    Runs {
        #[command(subcommand)]
        command: RunsCommand,
    },
    /// Manage agents and sidechains.
    #[command(visible_alias = "agent")]
    Agents {
        #[command(subcommand)]
        command: AgentsCommand,
    },
    /// Inspect or mutate durable schedules and wakeups.
    #[command(visible_alias = "schedule")]
    Schedules {
        #[command(subcommand)]
        command: SchedulesCommand,
    },
    /// Inspect or control session tasks.
    #[command(visible_alias = "task")]
    Tasks {
        #[command(subcommand)]
        command: TasksCommand,
    },
    /// Post cross-agent mailbox messages.
    #[command(visible_alias = "mailbox")]
    Mailboxes {
        #[command(subcommand)]
        command: MailboxesCommand,
    },
}

#[derive(Args, Debug, Clone)]
struct ServeArgs {
    #[arg(long, env = "KHEISH_BIND", default_value = DEFAULT_BIND)]
    bind: SocketAddr,
    #[arg(long, env = "KHEISH_STATE_ROOT", default_value = DEFAULT_STATE_ROOT)]
    state_root: PathBuf,
    #[arg(long, env = "KHEISH_WORKSPACE_ROOT")]
    workspace_root: Option<PathBuf>,
    #[arg(long, env = "KHEISH_MCP_CONFIG")]
    mcp_config: Option<PathBuf>,
    #[arg(long, env = "KHEISH_MCP_CREDENTIALS")]
    mcp_credentials: Option<PathBuf>,
    #[arg(
        long = "mcp-profile",
        env = "KHEISH_MCP_PROFILES",
        value_delimiter = ','
    )]
    mcp_profiles: Vec<String>,
    #[arg(
        long,
        env = "KHEISH_MCP_DISCOVERY",
        value_enum,
        default_value_t = McpDiscoveryArg::Auto
    )]
    mcp_discovery: McpDiscoveryArg,
    #[arg(long, env = "KHEISH_CONNECTORS_CONFIG")]
    connectors_config: Option<PathBuf>,
    #[arg(long = "skill-root", env = "KHEISH_SKILL_ROOTS", value_delimiter = ',')]
    skill_roots: Vec<PathBuf>,
    #[arg(
        long,
        env = "KHEISH_LOG_FORMAT",
        value_enum,
        default_value_t = LogFormatArg::Auto
    )]
    log_format: LogFormatArg,
    #[arg(
        long,
        env = "KHEISH_LOG_LEVEL",
        value_enum,
        default_value_t = LogLevelArg::Info
    )]
    log_level: LogLevelArg,
    #[arg(
        long,
        env = "KHEISH_EVENT_HISTORY_CAPACITY",
        default_value_t = DEFAULT_EVENT_HISTORY_CAPACITY
    )]
    event_history_capacity: usize,
    #[arg(
        long,
        env = "KHEISH_HTTP_AUTH_MODE",
        value_enum,
        default_value_t = HttpAuthModeArg::Auto
    )]
    http_auth_mode: HttpAuthModeArg,
    #[arg(long, env = "KHEISH_DAEMON_ADMIN_TOKEN", hide_env_values = true)]
    http_admin_token: Option<String>,
    #[arg(long, env = "KHEISH_DAEMON_ADMIN_TOKEN_FILE")]
    http_admin_token_file: Option<PathBuf>,
    #[arg(long, env = "KHEISH_DAEMON_READONLY_TOKEN", hide_env_values = true)]
    http_readonly_token: Option<String>,
    #[arg(long, env = "KHEISH_DAEMON_READONLY_TOKEN_FILE")]
    http_readonly_token_file: Option<PathBuf>,
    #[arg(
        long = "http-cors-allow-origin",
        env = "KHEISH_HTTP_CORS_ALLOW_ORIGINS",
        value_delimiter = ','
    )]
    http_cors_allow_origins: Vec<String>,
    #[arg(long, env = "KHEISH_PROVIDER", value_enum, default_value_t = ProviderKind::Anthropic)]
    provider: ProviderKind,
    #[arg(long, env = "KHEISH_ROUTES_FILE")]
    routes_file: Option<PathBuf>,
    #[arg(long, env = "KHEISH_DEFAULT_ROUTE")]
    default_route: Option<String>,
    #[arg(long, env = "KHEISH_MODEL")]
    model: Option<String>,
    #[arg(long, env = "KHEISH_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
    #[arg(long, env = "GOOGLE_API_KEY", hide_env_values = true)]
    google_api_key: Option<String>,
    #[arg(long, env = "ANTHROPIC_BASE_URL")]
    anthropic_base_url: Option<String>,
    #[arg(long, env = "ANTHROPIC_VERSION")]
    anthropic_version: Option<String>,
    #[arg(
        long = "anthropic-beta-header",
        env = "ANTHROPIC_BETA_HEADERS",
        value_delimiter = ','
    )]
    anthropic_beta_headers: Vec<String>,
    #[arg(long, env = "GOOGLE_BASE_URL")]
    google_base_url: Option<String>,
    #[arg(long, env = "KHEISH_IMAGE_PROVIDER", value_enum)]
    image_provider: Option<ProviderKind>,
    #[arg(long, env = "KHEISH_IMAGE_MODEL")]
    image_model: Option<String>,
    #[arg(long, env = "KHEISH_IMAGE_API_KEY", hide_env_values = true)]
    image_api_key: Option<String>,
    #[arg(long, env = "GOOGLE_IMAGE_MODEL")]
    google_image_model: Option<String>,
    #[arg(long, env = "KHEISH_TRANSCRIPTION_PROVIDER", value_enum)]
    transcription_provider: Option<ProviderKind>,
    #[arg(long, env = "KHEISH_TRANSCRIPTION_MODEL")]
    transcription_model: Option<String>,
    #[arg(long, env = "KHEISH_TRANSCRIPTION_API_KEY", hide_env_values = true)]
    transcription_api_key: Option<String>,
    #[arg(long, env = "KHEISH_TRANSCRIPTION_BASE_URL")]
    transcription_base_url: Option<String>,
    #[arg(long, env = "OPENAI_BASE_URL")]
    openai_base_url: Option<String>,
    #[arg(long, env = "OPENROUTER_BASE_URL")]
    openrouter_base_url: Option<String>,
    #[arg(long, env = "XAI_BASE_URL")]
    xai_base_url: Option<String>,
    #[arg(long, env = "OPENAI_ORGANIZATION")]
    openai_organization: Option<String>,
    #[arg(long, env = "OPENAI_PROJECT")]
    openai_project: Option<String>,
    #[arg(long, env = "KHEISH_OPENAI_AUTH_SOURCE", value_enum)]
    openai_auth_source: Option<OpenAiAuthSourceArg>,
    #[arg(long, env = "KHEISH_OPENAI_AUTH_FILE")]
    openai_auth_file: Option<PathBuf>,
    #[arg(long, env = "KHEISH_ANTHROPIC_AUTH_SOURCE", value_enum)]
    anthropic_auth_source: Option<AnthropicAuthSourceArg>,
    #[arg(long, env = "KHEISH_ANTHROPIC_CREDENTIALS_FILE")]
    anthropic_credentials_file: Option<PathBuf>,
    #[arg(long, env = "KHEISH_MAX_CHILD_DEPTH", default_value_t = 4)]
    max_child_depth: usize,
    #[arg(long, env = "KHEISH_MAX_LIVE_CHILDREN_PER_PARENT", default_value_t = 6)]
    max_live_children_per_parent: usize,
    #[arg(
        long,
        env = "KHEISH_MAX_LIVE_DESCENDANTS_PER_ROOT",
        default_value_t = 24
    )]
    max_live_descendants_per_root: usize,
    #[arg(long, env = "KHEISH_MAX_SPAWNS_PER_RUN", default_value_t = 6)]
    max_spawns_per_run: usize,
    #[arg(long, env = "KHEISH_SUBAGENT_POLICY_FILE")]
    subagent_policy_file: Option<PathBuf>,
    #[arg(long, env = "KHEISH_MAX_LIVE_SIDECHAINS_GLOBAL", default_value_t = 128)]
    max_live_sidechains_global: usize,
    #[arg(
        long,
        env = "KHEISH_MAX_LIVE_SIDECHAINS_PER_SESSION",
        default_value_t = 24
    )]
    max_live_sidechains_per_session: usize,
    #[arg(long, env = "KHEISH_SPAWN_RATE_WINDOW_MS", default_value_t = 60_000)]
    spawn_rate_window_ms: u64,
    #[arg(
        long,
        env = "KHEISH_MAX_SPAWNS_PER_SESSION_WINDOW",
        default_value_t = 60
    )]
    max_spawns_per_session_window: usize,
    #[arg(
        long,
        env = "KHEISH_MAX_SPAWNS_PER_PROFILE_WINDOW",
        default_value_t = 60
    )]
    max_spawns_per_profile_window: usize,
    #[arg(
        long,
        env = "KHEISH_MAX_SPAWNS_PER_PROJECT_WINDOW",
        default_value_t = 120
    )]
    max_spawns_per_project_window: usize,
    #[arg(long, env = "KHEISH_MAX_SPAWNS_GLOBAL_WINDOW", default_value_t = 240)]
    max_spawns_global_window: usize,
    #[arg(
        long,
        env = "KHEISH_MAX_SPAWN_INPUT_TOKENS_PER_REQUEST",
        default_value_t = 128_000
    )]
    max_spawn_input_tokens_per_request: u64,
    #[arg(
        long,
        env = "KHEISH_MAX_SPAWN_OUTPUT_TOKENS_PER_REQUEST",
        default_value_t = 64_000
    )]
    max_spawn_output_tokens_per_request: u64,
    #[arg(
        long,
        env = "KHEISH_MODEL_BUDGET_MAX_TOTAL_OUTPUT_TOKENS",
        default_value_t = 1_000_000
    )]
    model_budget_max_total_output_tokens: u64,
    #[arg(
        long,
        env = "KHEISH_MODEL_BUDGET_MAX_TOTAL_COST_USD",
        default_value_t = 500.0
    )]
    model_budget_max_total_cost_usd: f64,
    #[arg(
        long,
        env = "KHEISH_MAX_SPAWN_COST_MICROUSD_PER_WINDOW",
        default_value_t = 1_000_000
    )]
    max_spawn_cost_microusd_per_window: u64,
    #[arg(
        long,
        env = "KHEISH_MAX_SPAWN_CPU_MS_PER_WINDOW",
        default_value_t = 240_000
    )]
    max_spawn_cpu_ms_per_window: u64,
    #[arg(
        long,
        env = "KHEISH_ESTIMATED_SPAWN_COST_MICROUSD",
        default_value_t = 1_000
    )]
    estimated_spawn_cost_microusd: u64,
    #[arg(long, env = "KHEISH_ESTIMATED_SPAWN_CPU_MS", default_value_t = 1_000)]
    estimated_spawn_cpu_ms: u64,
    #[arg(
        long,
        env = "KHEISH_SCHEDULER_RETRY_BASE_DELAY_MS",
        default_value_t = 500
    )]
    scheduler_retry_base_delay_ms: u64,
    #[arg(
        long,
        env = "KHEISH_SCHEDULER_RETRY_MAX_DELAY_MS",
        default_value_t = 30_000
    )]
    scheduler_retry_max_delay_ms: u64,
    #[arg(long, env = "KHEISH_SCHEDULER_RETRY_JITTER_MS", default_value_t = 250)]
    scheduler_retry_jitter_ms: u64,
    #[arg(long, env = "KHEISH_SCHEDULER_RETRY_MAX_ATTEMPTS", default_value_t = 0)]
    scheduler_retry_max_attempts: u32,
}

#[derive(Subcommand, Debug)]
enum DoctorCommand {
    /// Diagnose the daemon route inventory or a routes TOML file.
    Routes(DoctorRoutesArgs),
}

#[derive(Args, Debug)]
struct DoctorRoutesArgs {
    /// Show only one route id.
    #[arg(long)]
    route: Option<String>,
    /// Apply the same default-route override used by `serve --default-route` while validating a routes file.
    #[arg(long)]
    default_route: Option<String>,
    /// Check daemon auth slot presence for routes that use auth_ref.
    #[arg(long)]
    check_auth: bool,
    /// Check persisted sessions, schedules, and non-terminal runs for route ids that are no longer configured.
    #[arg(long)]
    check_references: bool,
    /// Submit a tiny real run against the selected runtime route(s).
    #[arg(long)]
    canary: bool,
    /// Maximum time to wait for one route canary run.
    #[arg(long, default_value_t = 90_000)]
    canary_timeout_ms: u64,
    /// Validate this routes TOML file instead of the running daemon inventory.
    #[arg(long)]
    routes_file: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum RuntimeCommand {
    /// Show the current runtime settings.
    Get,
    /// Dry-run and explain runtime permission decisions.
    Permissions {
        #[command(subcommand)]
        command: RuntimePermissionsCommand,
    },
    /// Inspect or revoke brokered auth subjects and leases.
    Auth {
        #[command(subcommand)]
        command: RuntimeAuthCommand,
    },
    /// Inspect or update daemon learning automation settings.
    LearningPolicy {
        #[command(subcommand)]
        command: RuntimeLearningPolicyCommand,
    },
    /// Inspect or update recovered run-memory settings.
    RunMemoryPolicy {
        #[command(subcommand)]
        command: RuntimeRunMemoryPolicyCommand,
    },
    /// Inspect or update runtime tool execution limits.
    ToolLimits {
        #[command(subcommand)]
        command: RuntimeToolLimitsCommand,
    },
    /// Inspect daemon subagent spawn-policy quotas.
    SubagentPolicy {
        #[command(subcommand)]
        command: RuntimeSubagentPolicyCommand,
    },
    /// Inspect or update daemon hook settings.
    Hooks {
        #[command(subcommand)]
        command: RuntimeHooksCommand,
    },
    /// List durable runtime configuration revisions.
    Revisions,
    /// Roll back runtime configuration to a previous revision.
    Rollback {
        /// Revision to restore. Defaults to the previous revision.
        #[arg(long)]
        target_revision: Option<u64>,
        /// Require the current revision to match before rolling back.
        #[arg(long)]
        expected_revision: Option<u64>,
        /// Skip config_change hooks for operator recovery from a bad hook revision.
        #[arg(long)]
        skip_hooks: bool,
    },
    /// Swap the active model without restarting the daemon.
    SetModel {
        model: String,
        /// Require the current revision to match before changing the model.
        #[arg(long)]
        expected_revision: Option<u64>,
    },
    /// Change the active permission mode without restarting the daemon.
    SetPermissionMode {
        mode: PermissionModeArg,
        /// Require the current revision to match before changing permission mode.
        #[arg(long)]
        expected_revision: Option<u64>,
    },
    /// Change the active debug capture level without restarting the daemon.
    SetDebugLevel {
        level: DebugLevelArg,
        /// Require the current revision to match before changing debug level.
        #[arg(long)]
        expected_revision: Option<u64>,
    },
    /// Update the effective system prompt without restarting the daemon.
    SetSystemPrompt(RuntimeSetSystemPromptArgs),
}

#[derive(Subcommand, Debug)]
enum RuntimePermissionsCommand {
    /// Explain the permission decision for one tool call without executing it.
    Check(RuntimePermissionCheckArgs),
    /// Explain every registered tool under every runtime permission mode without mutating state.
    Matrix(RuntimePermissionMatrixArgs),
}

#[derive(Args, Debug)]
struct RuntimePermissionCheckArgs {
    /// Tool name to evaluate, such as bash, write_file, or read_file.
    tool_name: String,
    /// JSON object passed as the tool input for permission matching and audit preview.
    #[arg(long, default_value = "{}")]
    input_json: String,
    /// Optional session id used for session-scoped permission mode and hook updates.
    #[arg(long)]
    session_id: Option<String>,
    /// Optional tool call id shown in the dry-run output.
    #[arg(long)]
    tool_call_id: Option<String>,
    /// Optional runtime mode used only for this dry-run.
    #[arg(long)]
    mode: Option<PermissionModeArg>,
}

#[derive(Args, Debug)]
struct RuntimePermissionMatrixArgs {
    /// Optional session id used for session-scoped hook updates.
    #[arg(long)]
    session_id: Option<String>,
}

#[derive(Subcommand, Debug)]
enum McpCommand {
    /// Inspect built-in MCP catalog entries.
    Catalog {
        #[command(subcommand)]
        command: McpCatalogCommand,
    },
    /// Inspect built-in MCP profiles.
    Profiles {
        #[command(subcommand)]
        command: McpProfilesCommand,
    },
    /// Manage built-in MCP catalog credentials in the daemon secret store.
    Auth {
        #[command(subcommand)]
        command: McpAuthCommand,
    },
    /// Invoke daemon-loaded MCP tools explicitly through the operator API.
    Tools {
        #[command(subcommand)]
        command: McpToolsCommand,
    },
    /// Run MCP OAuth login, status, refresh, and logout flows.
    Oauth {
        #[command(subcommand)]
        command: McpOAuthCommand,
    },
}

#[derive(Subcommand, Debug)]
enum McpCatalogCommand {
    /// List built-in MCP catalog entries.
    List {
        #[arg(long)]
        profile: Option<String>,
        #[arg(long)]
        supported_only: bool,
    },
    /// Get one built-in MCP catalog entry.
    Get { id: String },
}

#[derive(Subcommand, Debug)]
enum McpProfilesCommand {
    /// List built-in MCP profiles.
    List,
    /// Get one built-in MCP profile.
    Get { id: String },
}

#[derive(Subcommand, Debug)]
enum McpAuthCommand {
    /// List the secret-store slots used by one built-in catalog entry.
    Slots { id: String },
    /// Store or rotate one credential for a built-in catalog entry.
    Set(McpAuthSetArgs),
}

#[derive(Args, Debug)]
struct McpAuthSetArgs {
    /// Built-in MCP catalog entry id.
    id: String,
    /// Credential environment key when the catalog entry declares more than one credential.
    #[arg(long)]
    credential_env: Option<String>,
    #[arg(long)]
    value: Option<String>,
    #[arg(long)]
    from_env: Option<String>,
    #[arg(long)]
    from_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[command(flatten)]
    store: SecretStoreArgs,
}

#[derive(Subcommand, Debug)]
enum McpToolsCommand {
    /// Call one qualified MCP tool with JSON arguments.
    Call(McpToolCallArgs),
}

#[derive(Args, Debug)]
struct McpToolCallArgs {
    /// Qualified MCP tool name, for example `mcp__github__get_me`.
    tool_name: String,
    /// JSON object passed to the MCP tool as arguments.
    #[arg(long)]
    input_json: Option<String>,
    /// Read MCP tool arguments from a JSON file instead of --input-json.
    #[arg(long)]
    input_file: Option<PathBuf>,
    /// Read MCP tool arguments JSON from stdin instead of --input-json.
    #[arg(long)]
    stdin: bool,
}

#[derive(Subcommand, Debug)]
enum McpOAuthCommand {
    /// Show the OAuth account status for one MCP catalog entry or slot.
    Status(McpOAuthSlotArgs),
    /// Login to an OAuth-backed HTTP MCP server and store the account in the daemon auth store.
    Login(McpOAuthLoginArgs),
    /// Force-refresh one MCP OAuth account.
    Refresh(McpOAuthSlotArgs),
    /// Delete one local MCP OAuth account.
    Logout(McpOAuthSlotArgs),
}

#[derive(Args, Debug)]
struct McpOAuthSlotArgs {
    /// Built-in MCP catalog entry id, server name, or explicit slot when --slot is omitted.
    id: String,
    /// Explicit auth-store slot id.
    #[arg(long)]
    slot: Option<String>,
}

#[derive(Args)]
struct McpOAuthLoginArgs {
    /// Built-in MCP catalog entry id or server name.
    id: String,
    /// Explicit HTTP MCP resource URL for custom servers.
    #[arg(long)]
    url: Option<String>,
    /// Explicit auth-store slot id.
    #[arg(long)]
    slot: Option<String>,
    /// OAuth client id. If omitted, Kheish uses DCR when the server advertises it.
    #[arg(long)]
    client_id: Option<String>,
    /// OAuth client secret for pre-registered confidential clients. Prefer --client-secret-env, --client-secret-file, or --client-secret-stdin.
    #[arg(long, hide_env_values = true)]
    client_secret: Option<String>,
    /// Environment variable containing the OAuth client secret.
    #[arg(long)]
    client_secret_env: Option<String>,
    /// File containing the OAuth client secret.
    #[arg(long)]
    client_secret_file: Option<PathBuf>,
    /// Read the OAuth client secret from stdin.
    #[arg(long)]
    client_secret_stdin: bool,
    /// Requested scopes. Defaults to server-discovered scopes.
    #[arg(long, value_delimiter = ',')]
    scopes: Vec<String>,
    /// Do not open a browser; print the authorization URL only.
    #[arg(long)]
    no_open: bool,
    /// Local loopback callback port. Defaults to an OS-assigned port.
    #[arg(long)]
    callback_port: Option<u16>,
    /// Login timeout in seconds.
    #[arg(long, default_value_t = 600)]
    timeout_sec: u64,
    /// Allow http URLs for loopback test MCP servers.
    #[arg(long)]
    allow_http_for_loopback: bool,
}

impl fmt::Debug for McpOAuthLoginArgs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthLoginArgs")
            .field("id", &self.id)
            .field("url", &self.url)
            .field("slot", &self.slot)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("client_secret_env", &self.client_secret_env)
            .field("client_secret_file", &self.client_secret_file)
            .field("client_secret_stdin", &self.client_secret_stdin)
            .field("scopes", &self.scopes)
            .field("no_open", &self.no_open)
            .field("callback_port", &self.callback_port)
            .field("timeout_sec", &self.timeout_sec)
            .field("allow_http_for_loopback", &self.allow_http_for_loopback)
            .finish()
    }
}

#[derive(Subcommand, Debug)]
enum RuntimeAuthCommand {
    /// Inspect, refresh, or revoke daemon-managed OAuth account slots.
    Accounts {
        #[command(subcommand)]
        command: RuntimeAuthAccountsCommand,
    },
    /// Show one brokered auth subject status.
    Subject {
        #[arg(allow_hyphen_values = true)]
        subject_id: String,
    },
    /// Revoke one brokered auth subject and all of its active leases.
    RevokeSubject {
        #[arg(allow_hyphen_values = true)]
        subject_id: String,
    },
    /// Show one brokered credential lease status.
    Lease {
        #[arg(allow_hyphen_values = true)]
        lease_id: String,
    },
    /// Revoke one brokered credential lease.
    RevokeLease {
        #[arg(allow_hyphen_values = true)]
        lease_id: String,
    },
    /// Revoke every active route, connector, and MCP lease for one auth slot.
    RevokeSlot {
        #[arg(allow_hyphen_values = true)]
        slot_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum RuntimeAuthAccountsCommand {
    /// List daemon-managed OAuth account slots.
    List,
    /// Show one daemon-managed OAuth account slot.
    Get { slot_id: String },
    /// Force-refresh one daemon-managed OAuth account slot.
    Refresh { slot_id: String },
    /// Delete one local daemon-managed OAuth account slot.
    Revoke { slot_id: String },
}

#[derive(Subcommand, Debug)]
enum RuntimeHooksCommand {
    /// Show the current hook configuration.
    Get,
    /// List hook dead-letter records.
    DeadLetter,
    /// Mark one hook dead-letter record as operator-resolved.
    ResolveDeadLetter {
        dead_letter_id: String,
        /// Operator reason stored in the resolved-DLQ ledger with secret-looking spans redacted.
        #[arg(long)]
        reason: String,
    },
    /// Replace the current hook configuration.
    Set(RuntimeSetHooksArgs),
}

#[derive(Subcommand, Debug)]
enum RuntimeLearningPolicyCommand {
    /// Show the current learning automation policy.
    Get,
    /// Replace the current learning automation policy.
    Set(RuntimeSetLearningPolicyArgs),
}

#[derive(Subcommand, Debug)]
enum RuntimeRunMemoryPolicyCommand {
    /// Show the current recovered run-memory policy.
    Get,
    /// Replace the current recovered run-memory policy.
    Set(RuntimeSetRunMemoryPolicyArgs),
}

#[derive(Subcommand, Debug)]
enum RuntimeToolLimitsCommand {
    /// Show the current tool runtime limits.
    Get,
    /// Replace the current tool runtime limits.
    Set(RuntimeSetToolLimitsArgs),
}

#[derive(Subcommand, Debug)]
enum RuntimeSubagentPolicyCommand {
    /// Show current subagent quota usage and policy counters.
    Quotas,
}

#[derive(Args, Debug)]
struct RuntimeSetHooksArgs {
    #[arg(long)]
    file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    reset: bool,
    /// Require the current revision to match before changing hooks.
    #[arg(long)]
    expected_revision: Option<u64>,
    /// Skip config_change hooks for operator recovery from a bad hook revision.
    #[arg(long)]
    skip_hooks: bool,
}

#[derive(Args, Debug)]
struct RuntimeSetLearningPolicyArgs {
    #[arg(long)]
    file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    reset: bool,
    /// Require the current revision to match before changing learning policy.
    #[arg(long)]
    expected_revision: Option<u64>,
}

#[derive(Args, Debug)]
struct RuntimeSetRunMemoryPolicyArgs {
    #[arg(long)]
    file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    reset: bool,
    /// Require the current revision to match before changing run-memory policy.
    #[arg(long)]
    expected_revision: Option<u64>,
}

#[derive(Args, Debug)]
struct RuntimeSetToolLimitsArgs {
    #[arg(long)]
    file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    reset: bool,
    /// Require the current revision to match before changing tool limits.
    #[arg(long)]
    expected_revision: Option<u64>,
}

#[derive(Args, Debug)]
struct RuntimeSetSystemPromptArgs {
    #[arg(long, value_enum)]
    mode: Option<SystemPromptModeArg>,
    content: Option<String>,
    #[arg(long)]
    content_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    language: Option<String>,
    #[arg(long)]
    output_style: Option<String>,
    #[arg(long)]
    clear_text: bool,
    #[arg(long)]
    clear_language: bool,
    #[arg(long)]
    clear_output_style: bool,
    #[arg(long)]
    reset: bool,
    #[arg(long)]
    expected_revision: Option<u64>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum SystemPromptModeArg {
    Default,
    Custom,
    Append,
    Override,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum DebugLevelArg {
    Off,
    On,
    Redacted,
    Full,
}

#[derive(Subcommand, Debug)]
enum EventsCommand {
    /// Follow the global daemon event stream.
    #[command(visible_alias = "watch", visible_alias = "follow")]
    Stream {
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        cursor: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
enum AssetsCommand {
    /// List daemon-owned assets.
    List {
        #[arg(long)]
        query: Option<String>,
        #[command(flatten)]
        pagination: ListPaginationArgs,
    },
    /// Fetch one asset by identifier.
    Get { asset_id: String },
    /// Inspect durable references that currently point at one asset.
    References { asset_id: String },
    /// Delete one asset when it has no hard references.
    Delete {
        asset_id: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Plan or execute garbage collection for unreferenced assets.
    Gc {
        #[arg(long)]
        execute: bool,
    },
    /// Import one local file into the daemon-owned asset store.
    Import(AssetImportArgs),
}

#[derive(Subcommand, Debug)]
enum BoardsCommand {
    /// List daemon-owned boards.
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        owner_session_id: Option<String>,
    },
    /// Fetch one board by identifier.
    Get { board_id: String },
    /// Create one board.
    Create(CreateBoardArgs),
    /// Update one board.
    Update(UpdateBoardArgs),
    /// List immutable revisions for one board.
    Revisions { board_id: String },
    /// Fetch one immutable revision by identifier.
    GetRevision {
        board_id: String,
        revision_id: String,
    },
    /// Create one immutable revision for one board.
    CreateRevision(CreateBoardRevisionArgs),
}

#[derive(Args, Debug)]
struct CreateBoardArgs {
    display_name: String,
    #[arg(long)]
    board_id: Option<String>,
    #[arg(long)]
    owner_session_id: Option<String>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct UpdateBoardArgs {
    board_id: String,
    #[arg(long)]
    display_name: Option<String>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct CreateBoardRevisionArgs {
    board_id: String,
    #[arg(long)]
    previous_revision_id: Option<String>,
    #[arg(long)]
    client_revision_id: Option<String>,
    #[arg(long)]
    render_asset_id: String,
    #[arg(long)]
    state_asset_id: Option<String>,
    #[arg(long)]
    note: Option<String>,
    #[arg(long)]
    source_session_id: Option<String>,
    #[arg(long)]
    source_run_id: Option<String>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum ChannelsCommand {
    /// List daemon-owned channels.
    List {
        #[arg(long)]
        query: Option<String>,
    },
    /// Fetch one channel by identifier.
    Get { channel_id: String },
    /// Create one channel.
    Create(CreateChannelArgs),
    /// Update one channel.
    Update(UpdateChannelArgs),
    /// Delete one channel and its public message log.
    Delete { channel_id: String },
    /// Manage channel members.
    Members {
        #[command(subcommand)]
        command: ChannelMembersCommand,
    },
    /// Manage channel messages and reactions.
    Messages {
        #[command(subcommand)]
        command: ChannelMessagesCommand,
    },
    /// Manage channel stimuli and autonomous wake-ups.
    Stimuli {
        #[command(subcommand)]
        command: ChannelStimuliCommand,
    },
    /// Inspect canonical thread work-state projections for one channel.
    ThreadWork {
        channel_id: String,
        #[arg(long)]
        thread_root_message_id: Option<String>,
    },
    /// List active public turn leases for one channel.
    Leases { channel_id: String },
}

#[derive(Subcommand, Debug)]
enum ProjectsCommand {
    /// List daemon-owned projects.
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        member_session_id: Option<String>,
        #[arg(long)]
        channel_id: Option<String>,
        #[arg(long, value_enum)]
        status: Option<ProjectStatusArg>,
    },
    /// Fetch one project by identifier.
    Get { project_id: String },
    /// Create one project.
    Create(CreateProjectArgs),
    /// Update one project.
    Update(UpdateProjectArgs),
    /// Delete one project.
    Delete { project_id: String },
    /// Manage project members.
    Members {
        #[command(subcommand)]
        command: ProjectMembersCommand,
    },
    /// Manage linked project channels.
    Channels {
        #[command(subcommand)]
        command: ProjectChannelsCommand,
    },
    /// Manage project tasks.
    Tasks {
        #[command(subcommand)]
        command: ProjectTasksCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectMembersCommand {
    /// List project members.
    List { project_id: String },
    /// Create or replace one project member.
    Upsert(UpsertProjectMemberArgs),
    /// Remove one project member.
    Remove {
        project_id: String,
        member_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectChannelsCommand {
    /// List linked project channels.
    List { project_id: String },
    /// Create or replace one project-channel link.
    Link(LinkProjectChannelArgs),
    /// Remove one project-channel link.
    Unlink {
        project_id: String,
        channel_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ProjectTasksCommand {
    /// List project tasks.
    List {
        project_id: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long, value_enum)]
        status: Option<TaskStatusArg>,
        #[arg(long)]
        assignee_member_id: Option<String>,
    },
    /// Fetch one project task.
    Get { project_id: String, task_id: String },
    /// Create one project task.
    Create(CreateProjectTaskArgs),
    /// Update one project task.
    Update(UpdateProjectTaskArgs),
    /// Start one project task on its assigned session.
    Start(StartProjectTaskArgs),
    /// Delete one project task.
    Delete { project_id: String, task_id: String },
}

#[derive(Subcommand, Debug)]
enum PlaybooksCommand {
    /// List Playbook definitions.
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(long, value_enum)]
        status: Option<PlaybookStatusArg>,
    },
    /// Fetch one Playbook.
    Get { playbook_id: String },
    /// Validate one Playbook manifest without storing it.
    Validate(PlaybookManifestInputArgs),
    /// Create one immutable Playbook version.
    Create(PlaybookManifestInputArgs),
    /// Publish one immutable Playbook version.
    Publish(PublishPlaybookArgs),
    /// Revoke one immutable Playbook version.
    Revoke(RevokePlaybookArgs),
}

#[derive(Subcommand, Debug)]
enum StackCommand {
    /// Write a starter KheishStack file.
    Init(StackInitArgs),
    /// Validate a KheishStack file without contacting mutable endpoints.
    Validate(StackFileArgs),
    /// Compute daemon drift and the apply order.
    Plan(StackPlanArgs),
    /// Print only non-noop drift actions.
    Diff(StackPlanArgs),
    /// Apply daemon resources in reconciliation order.
    Apply(StackApplyArgs),
    /// Verify that live daemon resources match the stack.
    Verify(StackFileArgs),
    /// Adopt existing daemon resources into the local apply ledger.
    Import(StackImportArgs),
    /// Plan or execute teardown for ledger-owned stack resources.
    Down(StackDownArgs),
}

#[derive(Subcommand, Debug)]
enum FlowsCommand {
    /// List Flow projections.
    List {
        #[arg(long)]
        playbook_id: Option<String>,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(long, value_enum)]
        status: Option<FlowStatusArg>,
    },
    /// Start one Flow by scheduling a normal session run.
    Start(StartFlowArgs),
    /// Fetch one Flow projection.
    Get { flow_id: String },
    /// Cancel one Flow by delegating to its run when available.
    Cancel { flow_id: String },
    /// Append evidence refs to one Flow.
    Evidence(AppendFlowEvidenceArgs),
    /// Verify a product-view Flow from daemon/workspace evidence.
    VerifyProductView(VerifyProductViewFlowArgs),
    /// Stream the referenced run events for one Flow.
    Stream { flow_id: String },
}

#[derive(Subcommand, Debug)]
enum ChannelMembersCommand {
    /// List channel members.
    List { channel_id: String },
    /// Create or replace one channel member.
    Upsert(UpsertChannelMemberArgs),
    /// Remove one channel member.
    Remove {
        channel_id: String,
        member_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ChannelMessagesCommand {
    /// List public channel messages.
    List {
        channel_id: String,
        #[arg(long)]
        thread_root_message_id: Option<String>,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Post one public channel message.
    Post(PostChannelMessageArgs),
    /// Apply one public reaction.
    React(ChannelReactionArgs),
    /// Remove one public reaction.
    Unreact(ChannelReactionArgs),
}

#[derive(Subcommand, Debug)]
enum ChannelStimuliCommand {
    /// List durable autonomous stimuli for one channel.
    List {
        channel_id: String,
        #[arg(long)]
        thread_root_message_id: Option<String>,
        #[arg(long, value_enum)]
        state: Option<CliChannelStimulusState>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Create one durable autonomous stimulus.
    Create(CreateChannelStimulusArgs),
}

#[derive(Args, Debug)]
struct CreateChannelArgs {
    title: String,
    #[arg(long)]
    channel_id: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    purpose: Option<String>,
    #[arg(long)]
    created_by: Option<String>,
    #[command(flatten)]
    autonomy_policy: ChannelAutonomyPolicyArgs,
    #[arg(long = "pinned-asset-id")]
    pinned_asset_ids: Vec<String>,
    #[arg(long)]
    members_json: Option<String>,
    #[arg(long)]
    members_file: Option<PathBuf>,
    #[arg(long, value_enum)]
    default_participation_mode: Option<CliChannelParticipationMode>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct UpdateChannelArgs {
    channel_id: String,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    purpose: Option<String>,
    #[arg(long = "pinned-asset-id")]
    pinned_asset_ids: Vec<String>,
    #[arg(long)]
    replace_pinned_assets: bool,
    #[arg(long)]
    paused: Option<bool>,
    #[command(flatten)]
    autonomy_policy: ChannelAutonomyPolicyArgs,
    #[arg(long, value_enum)]
    default_participation_mode: Option<CliChannelParticipationMode>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug, Default)]
struct ChannelAutonomyPolicyArgs {
    #[arg(long)]
    autonomy_policy_json: Option<String>,
    #[arg(long)]
    autonomy_policy_file: Option<PathBuf>,
    #[arg(long)]
    max_parallel_public_speakers: Option<u32>,
    #[arg(long)]
    max_agent_replies_per_human_message: Option<u32>,
    #[arg(long)]
    member_cooldown_ms: Option<u64>,
    #[arg(long)]
    lease_timeout_ms: Option<u64>,
    #[arg(long)]
    max_pending_stimuli: Option<u32>,
    #[arg(long)]
    max_autonomous_root_posts_per_hour: Option<u32>,
    #[arg(long)]
    max_active_autonomous_roots: Option<u32>,
    #[arg(long)]
    quiet_period_ms_after_root_post: Option<u64>,
}

#[derive(Args, Debug)]
struct UpsertChannelMemberArgs {
    channel_id: String,
    member_id: String,
    display_name: String,
    #[arg(long, value_enum)]
    member_kind: CliChannelMemberKind,
    #[arg(long, value_enum)]
    display_name_mode: Option<CliChannelMemberDisplayNameMode>,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    actor_id: Option<String>,
    #[arg(long)]
    role: Option<String>,
    #[arg(long = "expertise-tag")]
    expertise_tags: Vec<String>,
    #[arg(long, value_enum)]
    participation_mode: Option<CliChannelParticipationMode>,
    #[arg(long)]
    muted: Option<bool>,
}

#[derive(Args, Debug)]
struct PostChannelMessageArgs {
    channel_id: String,
    #[arg(long)]
    sender_actor_id: String,
    #[arg(long)]
    sender_display_name: Option<String>,
    #[arg(long)]
    sender_session_id: Option<String>,
    #[arg(long)]
    thread_root_message_id: Option<String>,
    #[arg(long)]
    reply_to_message_id: Option<String>,
    #[arg(long = "addressed-member-id")]
    addressed_member_ids: Vec<String>,
    #[arg(long)]
    input_items_json: Option<String>,
    #[arg(long)]
    input_items_file: Option<PathBuf>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg()]
    content: Vec<String>,
}

#[derive(Args, Debug)]
struct ChannelReactionArgs {
    channel_id: String,
    message_id: String,
    actor_id: String,
    emoji: String,
}

#[derive(Args, Debug)]
struct CreateChannelStimulusArgs {
    channel_id: String,
    #[arg(long, value_enum, default_value_t = CliChannelStimulusScope::Channel)]
    scope: CliChannelStimulusScope,
    #[arg(long)]
    thread_root_message_id: Option<String>,
    #[arg(long, value_enum)]
    kind: CliChannelStimulusKind,
    #[arg(long, value_enum)]
    visibility_hint: Option<CliChannelStimulusVisibilityHint>,
    #[arg(long = "addressed-member-id")]
    addressed_member_ids: Vec<String>,
    #[arg(long)]
    sender_session_id: Option<String>,
    #[arg(long)]
    sender_actor_id: Option<String>,
    #[arg(long)]
    sender_display_name: Option<String>,
    #[arg(long)]
    source_kind: Option<String>,
    #[arg(long)]
    source_ref: Option<String>,
    #[arg(long)]
    dedupe_key: Option<String>,
    #[arg(long)]
    progress_key: Option<String>,
    #[arg(long)]
    available_at_ms: Option<u64>,
    #[arg(long)]
    expires_at_ms: Option<u64>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg()]
    content: Vec<String>,
}

#[derive(Args, Debug)]
struct CreateProjectArgs {
    display_name: String,
    #[arg(long)]
    project_id: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long, value_enum)]
    status: Option<ProjectStatusArg>,
    #[arg(long)]
    members_json: Option<String>,
    #[arg(long)]
    members_file: Option<PathBuf>,
    #[arg(long)]
    channel_links_json: Option<String>,
    #[arg(long)]
    channel_links_file: Option<PathBuf>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct UpdateProjectArgs {
    project_id: String,
    #[arg(long)]
    display_name: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long, value_enum)]
    status: Option<ProjectStatusArg>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct UpsertProjectMemberArgs {
    project_id: String,
    member_id: String,
    display_name: String,
    #[arg(long, value_enum)]
    member_kind: CliChannelMemberKind,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    agent_id: Option<String>,
    #[arg(long)]
    actor_id: Option<String>,
    #[arg(long)]
    role: Option<String>,
    #[arg(long = "expertise-tag")]
    expertise_tags: Vec<String>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct LinkProjectChannelArgs {
    project_id: String,
    channel_id: String,
    #[arg(long)]
    role: Option<String>,
    #[arg(long)]
    default_for_new_tasks: Option<bool>,
    #[arg(long)]
    mirror_members: Option<bool>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct CreateProjectTaskArgs {
    project_id: String,
    title: String,
    #[arg(long)]
    project_task_id: Option<String>,
    #[arg(long, default_value = "")]
    description: String,
    #[arg(long, value_enum)]
    status: Option<TaskStatusArg>,
    #[arg(long)]
    assignee_member_id: Option<String>,
    #[arg(long)]
    assignee_session_id: Option<String>,
    #[arg(long)]
    assignee_agent_id: Option<String>,
    #[arg(long)]
    discussion_channel_id: Option<String>,
    #[arg(long)]
    discussion_thread_root_message_id: Option<String>,
    #[arg(long = "blocked-by")]
    blocked_by: Vec<String>,
    #[arg(long)]
    latest_run_id: Option<String>,
    #[arg(long = "task-output")]
    task_output: Option<String>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct UpdateProjectTaskArgs {
    project_id: String,
    task_id: String,
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    description: Option<String>,
    #[arg(long, value_enum)]
    status: Option<TaskStatusArg>,
    #[arg(long)]
    assignee_member_id: Option<String>,
    #[arg(long)]
    assignee_session_id: Option<String>,
    #[arg(long)]
    assignee_agent_id: Option<String>,
    #[arg(long)]
    clear_assignment: bool,
    #[arg(long)]
    discussion_channel_id: Option<String>,
    #[arg(long)]
    discussion_thread_root_message_id: Option<String>,
    #[arg(long)]
    clear_discussion: bool,
    #[arg(long = "blocked-by")]
    blocked_by: Vec<String>,
    #[arg(long)]
    replace_blocked_by: bool,
    #[arg(long)]
    latest_run_id: Option<String>,
    #[arg(long = "task-output")]
    task_output: Option<String>,
    #[arg(long)]
    clear_output: bool,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct StartProjectTaskArgs {
    project_id: String,
    task_id: String,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    kickoff_message: Option<String>,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 1000)]
    poll_interval_ms: u64,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct PlaybookManifestInputArgs {
    #[arg(long)]
    manifest_json: Option<String>,
    #[arg(long)]
    manifest_file: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
struct StackFileArgs {
    #[arg(short = 'f', long, default_value = "Kheishfile.yaml")]
    file: PathBuf,
    #[arg(
        long,
        help = "State root used for the apply ledger when it cannot be derived from the daemon"
    )]
    state_root: Option<PathBuf>,
    #[arg(
        long,
        help = "Deprecated no-op; KheishStack v1alpha1 always enforces fail-closed scope validation"
    )]
    no_strict_scopes: bool,
}

#[derive(Args, Debug, Clone)]
struct StackPlanArgs {
    #[command(flatten)]
    file: StackFileArgs,
    #[arg(long, help = "Omit noop actions from the rendered plan")]
    only_changes: bool,
    #[arg(
        long,
        help = "Allow fingerprints for secret values provided through value_env entries"
    )]
    allow_secret_env: bool,
}

#[derive(Args, Debug, Clone)]
struct StackApplyArgs {
    #[command(flatten)]
    file: StackFileArgs,
    #[arg(long, help = "Only print the plan; do not mutate the daemon or ledger")]
    dry_run: bool,
    #[arg(
        long,
        help = "Deprecated compatibility flag; startup-only config still blocks apply"
    )]
    force_restart: bool,
    #[arg(
        long,
        help = "Allow ledger-owned resources omitted from the stack to be pruned"
    )]
    prune: bool,
    #[arg(
        long,
        help = "Allow fingerprints for secret values provided through value_env entries"
    )]
    allow_secret_env: bool,
}

#[derive(Args, Debug, Clone)]
struct StackInitArgs {
    #[arg(short = 'o', long = "output-file", default_value = "Kheishfile.yaml")]
    output_file: PathBuf,
    #[arg(long, default_value = "kheish-stack")]
    name: String,
    #[arg(long, help = "Overwrite the output file if it already exists")]
    force: bool,
}

#[derive(Args, Debug, Clone)]
struct StackImportArgs {
    #[command(flatten)]
    file: StackFileArgs,
    #[arg(
        long = "resource",
        help = "Resource to adopt, formatted as kind/id; may be repeated"
    )]
    resources: Vec<String>,
    #[arg(
        long,
        help = "Allow imported value_env secrets to be fingerprinted from the daemon environment"
    )]
    allow_secret_env: bool,
}

#[derive(Args, Debug, Clone)]
struct StackDownArgs {
    #[command(flatten)]
    file: StackFileArgs,
    #[arg(
        long,
        help = "Execute supported destructive actions; default is plan-only"
    )]
    yes: bool,
}

#[derive(Args, Debug)]
struct PublishPlaybookArgs {
    playbook_id: String,
    #[arg(long)]
    version: String,
    #[arg(long)]
    digest: String,
    #[arg(long, value_enum)]
    status: Option<PlaybookStatusArg>,
    #[arg(long)]
    evidence_refs_json: Option<String>,
    #[arg(long)]
    evidence_refs_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct RevokePlaybookArgs {
    playbook_id: String,
    #[arg(long)]
    version: String,
    #[arg(long)]
    digest: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long)]
    evidence_refs_json: Option<String>,
    #[arg(long)]
    evidence_refs_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct StartFlowArgs {
    #[arg(long)]
    flow_id: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    #[arg(long)]
    playbook_id: String,
    #[arg(long)]
    version: String,
    #[arg(long)]
    digest: String,
    #[arg(long)]
    session_id: String,
    #[arg(long)]
    request_json: Option<String>,
    #[arg(long)]
    request_file: Option<PathBuf>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg(long)]
    evidence_refs_json: Option<String>,
    #[arg(long)]
    evidence_refs_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct AppendFlowEvidenceArgs {
    flow_id: String,
    #[arg(long)]
    evidence_refs_json: Option<String>,
    #[arg(long)]
    evidence_refs_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct VerifyProductViewFlowArgs {
    flow_id: String,
    #[arg(long)]
    report_path: String,
    #[arg(long)]
    required_section: Vec<String>,
    #[arg(long)]
    forbidden_tool: Vec<String>,
    #[arg(long)]
    output_file: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliChannelMemberKind {
    HumanActor,
    Session,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliChannelStimulusScope {
    Channel,
    Thread,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliChannelStimulusKind {
    AgentIdea,
    ScheduleFire,
    ScheduleResult,
    ReviewCompleted,
    TaskCompleted,
    ObservationMaterialized,
    ThreadIdleFollowup,
    ResultSummary,
    SupersessionNotice,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliChannelStimulusVisibilityHint {
    Auto,
    Main,
    Thread,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliChannelStimulusState {
    Pending,
    Claimed,
    Dispatched,
    Coalesced,
    Superseded,
    Cancelled,
}

impl From<CliChannelMemberKind> for ChannelMemberKind {
    fn from(value: CliChannelMemberKind) -> Self {
        match value {
            CliChannelMemberKind::HumanActor => Self::HumanActor,
            CliChannelMemberKind::Session => Self::Session,
        }
    }
}

impl From<CliChannelStimulusScope> for ChannelStimulusScope {
    fn from(value: CliChannelStimulusScope) -> Self {
        match value {
            CliChannelStimulusScope::Channel => Self::Channel,
            CliChannelStimulusScope::Thread => Self::Thread,
        }
    }
}

impl From<CliChannelStimulusKind> for ChannelStimulusKind {
    fn from(value: CliChannelStimulusKind) -> Self {
        match value {
            CliChannelStimulusKind::AgentIdea => Self::AgentIdea,
            CliChannelStimulusKind::ScheduleFire => Self::ScheduleFire,
            CliChannelStimulusKind::ScheduleResult => Self::ScheduleResult,
            CliChannelStimulusKind::ReviewCompleted => Self::ReviewCompleted,
            CliChannelStimulusKind::TaskCompleted => Self::TaskCompleted,
            CliChannelStimulusKind::ObservationMaterialized => Self::ObservationMaterialized,
            CliChannelStimulusKind::ThreadIdleFollowup => Self::ThreadIdleFollowUp,
            CliChannelStimulusKind::ResultSummary => Self::ResultSummary,
            CliChannelStimulusKind::SupersessionNotice => Self::SupersessionNotice,
        }
    }
}

impl From<CliChannelStimulusVisibilityHint> for ChannelStimulusVisibilityHint {
    fn from(value: CliChannelStimulusVisibilityHint) -> Self {
        match value {
            CliChannelStimulusVisibilityHint::Auto => Self::Auto,
            CliChannelStimulusVisibilityHint::Main => Self::Main,
            CliChannelStimulusVisibilityHint::Thread => Self::Thread,
        }
    }
}

impl From<CliChannelStimulusState> for ChannelStimulusState {
    fn from(value: CliChannelStimulusState) -> Self {
        match value {
            CliChannelStimulusState::Pending => Self::Pending,
            CliChannelStimulusState::Claimed => Self::Claimed,
            CliChannelStimulusState::Dispatched => Self::Dispatched,
            CliChannelStimulusState::Coalesced => Self::Coalesced,
            CliChannelStimulusState::Superseded => Self::Superseded,
            CliChannelStimulusState::Cancelled => Self::Cancelled,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliChannelParticipationMode {
    ManualOnly,
    SelectedOnly,
    PreferSelected,
    AlwaysListen,
}

impl From<CliChannelParticipationMode> for ChannelParticipationMode {
    fn from(value: CliChannelParticipationMode) -> Self {
        match value {
            CliChannelParticipationMode::ManualOnly => Self::ManualOnly,
            CliChannelParticipationMode::SelectedOnly => Self::SelectedOnly,
            CliChannelParticipationMode::PreferSelected => Self::PreferSelected,
            CliChannelParticipationMode::AlwaysListen => Self::AlwaysListen,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum CliChannelMemberDisplayNameMode {
    Manual,
    FollowAgent,
}

impl From<CliChannelMemberDisplayNameMode> for ChannelMemberDisplayNameMode {
    fn from(value: CliChannelMemberDisplayNameMode) -> Self {
        match value {
            CliChannelMemberDisplayNameMode::Manual => Self::Manual,
            CliChannelMemberDisplayNameMode::FollowAgent => Self::FollowAgent,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ProjectStatusArg {
    Active,
    Paused,
    Completed,
    Archived,
}

impl From<ProjectStatusArg> for ProjectStatus {
    fn from(value: ProjectStatusArg) -> Self {
        match value {
            ProjectStatusArg::Active => Self::Active,
            ProjectStatusArg::Paused => Self::Paused,
            ProjectStatusArg::Completed => Self::Completed,
            ProjectStatusArg::Archived => Self::Archived,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PlaybookStatusArg {
    Draft,
    Verified,
    Canary,
    Active,
    Revoked,
}

impl From<PlaybookStatusArg> for kheish_daemon::PlaybookReleaseStatus {
    fn from(value: PlaybookStatusArg) -> Self {
        match value {
            PlaybookStatusArg::Draft => Self::Draft,
            PlaybookStatusArg::Verified => Self::Verified,
            PlaybookStatusArg::Canary => Self::Canary,
            PlaybookStatusArg::Active => Self::Active,
            PlaybookStatusArg::Revoked => Self::Revoked,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FlowStatusArg {
    Pending,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    Unknown,
}

impl From<FlowStatusArg> for kheish_daemon::FlowStatus {
    fn from(value: FlowStatusArg) -> Self {
        match value {
            FlowStatusArg::Pending => Self::Pending,
            FlowStatusArg::Running => Self::Running,
            FlowStatusArg::Waiting => Self::Waiting,
            FlowStatusArg::Succeeded => Self::Succeeded,
            FlowStatusArg::Failed => Self::Failed,
            FlowStatusArg::Cancelled => Self::Cancelled,
            FlowStatusArg::Interrupted => Self::Interrupted,
            FlowStatusArg::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningScopeKindArg {
    Session,
    Persona,
    Project,
    Workspace,
}

impl From<LearningScopeKindArg> for kheish_types::LearningScopeKind {
    fn from(value: LearningScopeKindArg) -> Self {
        match value {
            LearningScopeKindArg::Session => Self::Session,
            LearningScopeKindArg::Persona => Self::Persona,
            LearningScopeKindArg::Project => Self::Project,
            LearningScopeKindArg::Workspace => Self::Workspace,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningKindArg {
    RunSummary,
    Fact,
    Preference,
    Decision,
    Procedure,
}

impl From<LearningKindArg> for kheish_types::LearningKind {
    fn from(value: LearningKindArg) -> Self {
        match value {
            LearningKindArg::RunSummary => Self::RunSummary,
            LearningKindArg::Fact => Self::Fact,
            LearningKindArg::Preference => Self::Preference,
            LearningKindArg::Decision => Self::Decision,
            LearningKindArg::Procedure => Self::Procedure,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningSensitivityArg {
    Scoped,
    Sensitive,
}

impl From<LearningSensitivityArg> for kheish_types::LearningSensitivity {
    fn from(value: LearningSensitivityArg) -> Self {
        match value {
            LearningSensitivityArg::Scoped => Self::Scoped,
            LearningSensitivityArg::Sensitive => Self::Sensitive,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningCandidateStateArg {
    Pending,
    Escalated,
    Published,
    Rejected,
}

impl From<LearningCandidateStateArg> for kheish_daemon::LearningCandidateState {
    fn from(value: LearningCandidateStateArg) -> Self {
        match value {
            LearningCandidateStateArg::Pending => Self::Pending,
            LearningCandidateStateArg::Escalated => Self::Escalated,
            LearningCandidateStateArg::Published => Self::Published,
            LearningCandidateStateArg::Rejected => Self::Rejected,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningStatusArg {
    Provisional,
    Active,
    Superseded,
    Revoked,
}

impl From<LearningStatusArg> for kheish_types::LearningStatus {
    fn from(value: LearningStatusArg) -> Self {
        match value {
            LearningStatusArg::Provisional => Self::Provisional,
            LearningStatusArg::Active => Self::Active,
            LearningStatusArg::Superseded => Self::Superseded,
            LearningStatusArg::Revoked => Self::Revoked,
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum LearningPolicyDecisionArg {
    Manual,
    Automatic,
    Escalated,
}

impl From<LearningPolicyDecisionArg> for kheish_types::LearningPolicyDecision {
    fn from(value: LearningPolicyDecisionArg) -> Self {
        match value {
            LearningPolicyDecisionArg::Manual => Self::Manual,
            LearningPolicyDecisionArg::Automatic => Self::Automatic,
            LearningPolicyDecisionArg::Escalated => Self::Escalated,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningSkillStatusArg {
    Draft,
    Verified,
    Canary,
    Active,
    Revoked,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningSkillRolloutKindArg {
    Verification,
    Canary,
}

impl From<LearningSkillRolloutKindArg> for kheish_daemon::LearningSkillRolloutKind {
    fn from(value: LearningSkillRolloutKindArg) -> Self {
        match value {
            LearningSkillRolloutKindArg::Verification => Self::Verification,
            LearningSkillRolloutKindArg::Canary => Self::Canary,
        }
    }
}

impl From<LearningSkillStatusArg> for kheish_daemon::LearningSkillStatus {
    fn from(value: LearningSkillStatusArg) -> Self {
        match value {
            LearningSkillStatusArg::Draft => Self::Draft,
            LearningSkillStatusArg::Verified => Self::Verified,
            LearningSkillStatusArg::Canary => Self::Canary,
            LearningSkillStatusArg::Active => Self::Active,
            LearningSkillStatusArg::Revoked => Self::Revoked,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningPublishTierArg {
    Provisional,
    Active,
}

impl From<LearningPublishTierArg> for kheish_types::LearningPublishTier {
    fn from(value: LearningPublishTierArg) -> Self {
        match value {
            LearningPublishTierArg::Provisional => Self::Provisional,
            LearningPublishTierArg::Active => Self::Active,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LearningSkillContextArg {
    Fork,
}

impl From<LearningSkillContextArg> for kheish_types::SkillExecutionContext {
    fn from(value: LearningSkillContextArg) -> Self {
        match value {
            LearningSkillContextArg::Fork => Self::Fork,
        }
    }
}

#[derive(Subcommand, Debug)]
enum DerivationsCommand {
    /// List daemon-owned derivations.
    List {
        #[arg(long)]
        query: Option<String>,
    },
    /// Fetch one derivation by identifier.
    Get { derivation_id: String },
    /// Create or fetch one derivation for an existing subject.
    Create(DerivationCreateArgs),
}

#[derive(Subcommand, Debug)]
enum LearningsCommand {
    /// Manage reviewable learning candidates.
    Candidates {
        #[command(subcommand)]
        command: LearningCandidatesCommand,
    },
    /// Manage promoted procedural skills derived from reviewed learnings.
    Skills {
        #[command(subcommand)]
        command: LearningSkillsCommand,
    },
    /// List published learnings.
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(long, value_enum)]
        scope_kind: Option<LearningScopeKindArg>,
        #[arg(long)]
        scope_id: Option<String>,
        #[arg(long, value_enum)]
        kind: Option<LearningKindArg>,
        #[arg(long, value_enum)]
        status: Option<LearningStatusArg>,
        #[arg(long, value_enum)]
        policy_decision: Option<LearningPolicyDecisionArg>,
        #[arg(long)]
        policy_actor: Option<String>,
        #[arg(long)]
        matched_rule_name: Option<String>,
    },
    /// Fetch one published learning by identifier.
    Get { learning_id: String },
    /// Revoke one published learning by identifier.
    Revoke {
        learning_id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Revoke every published learning that matches the provided filters.
    RevokeMatching {
        #[arg(long)]
        query: Option<String>,
        #[arg(long, value_enum)]
        scope_kind: Option<LearningScopeKindArg>,
        #[arg(long)]
        scope_id: Option<String>,
        #[arg(long, value_enum)]
        kind: Option<LearningKindArg>,
        #[arg(long, value_enum)]
        status: Option<LearningStatusArg>,
        #[arg(long, value_enum)]
        policy_decision: Option<LearningPolicyDecisionArg>,
        #[arg(long)]
        policy_actor: Option<String>,
        #[arg(long)]
        matched_rule_name: Option<String>,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Supersede one published learning with a new active record.
    Supersede(LearningSupersedeArgs),
}

#[derive(Subcommand, Debug)]
enum LearningCandidatesCommand {
    /// List learning candidates.
    List {
        #[arg(long)]
        query: Option<String>,
        #[arg(long, value_enum)]
        scope_kind: Option<LearningScopeKindArg>,
        #[arg(long)]
        scope_id: Option<String>,
        #[arg(long, value_enum)]
        kind: Option<LearningKindArg>,
        #[arg(long, value_enum)]
        state: Option<LearningCandidateStateArg>,
    },
    /// Fetch one learning candidate by identifier.
    Get { candidate_id: String },
    /// Create one learning candidate for later publication.
    Create(LearningCandidateCreateArgs),
    /// Publish one candidate into the durable learning store.
    Publish(LearningCandidatePublishArgs),
    /// Reject one learning candidate.
    Reject { candidate_id: String },
}

#[derive(Subcommand, Debug)]
enum LearningSkillsCommand {
    /// List promoted procedural skills.
    List {
        #[arg(long)]
        source_learning_id: Option<String>,
        #[arg(long, value_enum)]
        status: Option<LearningSkillStatusArg>,
    },
    /// Fetch one promoted procedural skill by name.
    Get { skill_name: String },
    /// Promote one reviewed procedure learning into a daemon-owned skill.
    Promote(LearningSkillCreateArgs),
    /// Record daemon-validated rollout evidence from an existing run.
    RolloutResult(LearningSkillRolloutResultArgs),
    /// Revoke one promoted procedural skill.
    Revoke {
        skill_name: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Restore the latest historical active snapshot for one promoted procedural skill.
    Rollback {
        skill_name: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum ObservationsCommand {
    /// Manage durable observation sources.
    Sources {
        #[command(subcommand)]
        command: ObservationSourcesCommand,
    },
    /// List observations.
    List {
        #[arg(long)]
        source_id: Option<String>,
        #[arg(long)]
        stream_id: Option<String>,
        #[arg(long)]
        after_ms: Option<u64>,
        #[arg(long)]
        before_ms: Option<u64>,
        #[arg(long)]
        include_purged: bool,
    },
    /// Fetch one observation by identifier.
    Get { observation_id: String },
    /// List sanitized observation audit records.
    Audit(ObservationAuditArgs),
    /// Upload one observation through the source-scoped ingest endpoint.
    Ingest(ObservationIngestArgs),
    /// Materialize one observation selection into a normal daemon run.
    Materialize(ObservationMaterializeArgs),
    /// Create one durable schedule that materializes observations later.
    Schedule(ObservationScheduleArgs),
}

#[derive(Subcommand, Debug)]
enum ObservationSourcesCommand {
    /// List observation sources.
    List {
        #[arg(long)]
        query: Option<String>,
    },
    /// Fetch one observation source by identifier.
    Get { source_id: String },
    /// Create one observation source or rotate an existing source token in place.
    Create(ObservationSourceCreateArgs),
    /// Rotate one observation source upload token with an optional grace window.
    RotateToken(ObservationSourceRotateTokenArgs),
    /// Revoke one observation source upload token.
    RevokeToken(ObservationSourceRevokeTokenArgs),
}

#[derive(Subcommand, Debug)]
enum CaptureCommand {
    /// Create source tokens and render runtime configs for one or more capture agents.
    Provision(CaptureProvisionArgs),
    /// Inspect provisioned capture agents.
    Agents {
        #[command(subcommand)]
        command: CaptureAgentsCommand,
    },
    /// List current capture-agent alerts.
    Alerts,
    /// Revoke one capture agent, its leases, and owned observation sources.
    Revoke(CaptureAgentRevokeArgs),
    /// Send one source-token-authenticated heartbeat for a capture agent.
    Heartbeat(CaptureAgentHeartbeatArgs),
}

#[derive(Subcommand, Debug)]
enum CaptureAgentsCommand {
    /// List provisioned capture agents.
    List,
    /// Fetch one provisioned capture agent.
    Get { machine_id: String },
}

#[derive(Args, Debug)]
struct CaptureProvisionArgs {
    #[arg(long)]
    batch_id: String,
    #[arg(long)]
    daemon_url: Option<String>,
    #[arg(long, value_enum, default_value_t = CaptureOsProfileArg::Macos)]
    os_profile: CaptureOsProfileArg,
    #[arg(long = "agent-id")]
    agent_ids: Vec<String>,
    #[arg(long)]
    agent_prefix: Option<String>,
    #[arg(long)]
    count: Option<u64>,
    #[arg(long)]
    enable_screen: bool,
    #[arg(long)]
    enable_camera: bool,
    #[arg(long)]
    enable_system_audio: bool,
    #[arg(long)]
    enable_microphone: bool,
    #[arg(long)]
    camera_unique_id: Option<String>,
    #[arg(long)]
    camera_name: Option<String>,
    #[arg(long)]
    microphone_name: Option<String>,
    #[arg(long, default_value_t = 5_000)]
    interval_ms: u64,
    #[arg(long)]
    max_runs: Option<u64>,
    #[arg(long)]
    duration_ms: Option<u64>,
    #[arg(long, default_value_t = 7 * 24 * 60 * 60)]
    retention_seconds: u64,
    #[arg(long, default_value_t = 256)]
    max_active_observations: u64,
    #[arg(long, default_value_t = 128 * 1024 * 1024)]
    max_active_bytes: u64,
    #[arg(long, default_value_t = 7 * 24 * 60 * 60 * 1_000)]
    token_ttl_ms: u64,
    #[arg(long, default_value_t = 30_000)]
    heartbeat_interval_ms: u64,
    #[arg(long, default_value_t = 120_000)]
    heartbeat_grace_ms: u64,
    #[arg(long)]
    out_dir: Option<PathBuf>,
    /// Print upload tokens even when configs are written to --out-dir.
    #[arg(long)]
    show_secrets: bool,
}

#[derive(Args, Debug)]
struct CaptureAgentRevokeArgs {
    machine_id: String,
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Args, Debug)]
struct CaptureAgentHeartbeatArgs {
    machine_id: String,
    #[arg(long)]
    heartbeat_token: Option<String>,
    #[arg(long, hide = true)]
    upload_token: Option<String>,
}

#[derive(Args, Debug)]
struct DerivationCreateArgs {
    /// The deterministic derivation profile to materialize.
    #[arg(long, value_enum)]
    profile: DerivationProfileArg,
    /// The daemon-owned asset identifier used as the source subject.
    #[arg(long)]
    asset_id: Option<String>,
    /// The daemon-owned observation identifier used as the source subject.
    #[arg(long)]
    observation_id: Option<String>,
    /// The session identifier owning the source input event.
    #[arg(long)]
    session_id: Option<String>,
    /// The journal offset of the InputReceived event inside the source session.
    #[arg(long)]
    offset: Option<u64>,
    /// Optional provider prompt used to guide audio transcription.
    #[arg(long)]
    transcription_prompt: Option<String>,
    /// Optional language hint used for audio transcription.
    #[arg(long)]
    transcription_language: Option<String>,
    /// Reserved timestamp granularity request for audio transcription output.
    #[arg(long = "transcription-timestamp-granularity")]
    transcription_timestamp_granularities: Vec<String>,
    /// Recompute even when a terminal derivation already exists for the same cache key.
    #[arg(long)]
    force_refresh: bool,
    /// Recompute only when the current cache entry is a failed derivation.
    #[arg(long)]
    retry_failed: bool,
}

impl DerivationCreateArgs {
    fn transcription_options(&self) -> Option<kheish_daemon::DerivationTranscriptionOptions> {
        let has_prompt = self
            .transcription_prompt
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_language = self
            .transcription_language
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_timestamps = !self.transcription_timestamp_granularities.is_empty();
        (has_prompt || has_language || has_timestamps).then(|| {
            kheish_daemon::DerivationTranscriptionOptions {
                prompt: self.transcription_prompt.clone(),
                language: self.transcription_language.clone(),
                timestamp_granularities: self.transcription_timestamp_granularities.clone(),
                diarization: false,
            }
        })
    }
}

#[derive(Args, Debug)]
struct LearningCandidateCreateArgs {
    /// The durable scope kind that should own the candidate.
    #[arg(long, value_enum)]
    scope_kind: LearningScopeKindArg,
    /// The scope identifier for session, persona, or project candidates.
    #[arg(long)]
    scope_id: Option<String>,
    /// The candidate kind retained for review.
    #[arg(long, value_enum)]
    kind: LearningKindArg,
    /// The visibility class to retain with the candidate.
    #[arg(long, value_enum, default_value_t = LearningSensitivityArg::Scoped)]
    sensitivity: LearningSensitivityArg,
    /// The coarse confidence score in the inclusive range `[0, 100]`.
    #[arg(long, default_value_t = 80)]
    confidence: u8,
    /// Optional source run identifier retained for provenance.
    #[arg(long)]
    source_run_id: Option<String>,
    /// Optional source session identifier retained for provenance.
    #[arg(long)]
    source_session_id: Option<String>,
    /// Optional source agent identifier retained for provenance.
    #[arg(long)]
    source_agent_id: Option<String>,
    /// Optional source input event offset retained for provenance.
    #[arg(long)]
    source_input_event_offset: Option<u64>,
    /// Optional source observation identifier retained for provenance.
    #[arg(long)]
    source_observation_id: Option<String>,
    /// Optional source derivation identifier retained for provenance.
    #[arg(long)]
    source_derivation_id: Option<String>,
    /// Optional expiration timestamp in milliseconds since the Unix epoch.
    #[arg(long)]
    expires_at_ms: Option<u64>,
    /// Inline candidate content.
    content: Option<String>,
    /// Read candidate content from a file.
    #[arg(long)]
    content_file: Option<PathBuf>,
    /// Read candidate content from standard input.
    #[arg(long)]
    stdin: bool,
}

#[derive(Args, Debug)]
struct LearningCandidatePublishArgs {
    /// The learning candidate identifier that should be published.
    candidate_id: String,
    /// Optional replacement scope kind for the published learning.
    #[arg(long, value_enum)]
    scope_kind: Option<LearningScopeKindArg>,
    /// Optional replacement scope identifier for the published learning.
    #[arg(long)]
    scope_id: Option<String>,
    /// Optional replacement kind for the published learning.
    #[arg(long, value_enum)]
    kind: Option<LearningKindArg>,
    /// Optional replacement sensitivity for the published learning.
    #[arg(long, value_enum)]
    sensitivity: Option<LearningSensitivityArg>,
    /// Optional replacement confidence for the published learning.
    #[arg(long)]
    confidence: Option<u8>,
    /// Optional replacement expiration timestamp.
    #[arg(long)]
    expires_at_ms: Option<u64>,
    /// Optional publication tier for the durable learning.
    #[arg(long, value_enum)]
    publish_tier: Option<LearningPublishTierArg>,
    /// Optional older learning identifier superseded by the published record.
    #[arg(long)]
    supersedes: Option<String>,
    /// Optional replacement content.
    content: Option<String>,
    /// Read replacement content from a file.
    #[arg(long)]
    content_file: Option<PathBuf>,
    /// Read replacement content from standard input.
    #[arg(long)]
    stdin: bool,
}

#[derive(Args, Debug)]
struct LearningSupersedeArgs {
    /// The currently active learning identifier.
    learning_id: String,
    /// Optional replacement scope kind for the new record.
    #[arg(long, value_enum)]
    scope_kind: Option<LearningScopeKindArg>,
    /// Optional replacement scope identifier for the new record.
    #[arg(long)]
    scope_id: Option<String>,
    /// Optional replacement kind for the new record.
    #[arg(long, value_enum)]
    kind: Option<LearningKindArg>,
    /// Optional replacement sensitivity for the new record.
    #[arg(long, value_enum)]
    sensitivity: Option<LearningSensitivityArg>,
    /// Optional replacement confidence for the new record.
    #[arg(long)]
    confidence: Option<u8>,
    /// Optional replacement expiration timestamp.
    #[arg(long)]
    expires_at_ms: Option<u64>,
    /// Inline replacement content.
    content: Option<String>,
    /// Read replacement content from a file.
    #[arg(long)]
    content_file: Option<PathBuf>,
    /// Read replacement content from standard input.
    #[arg(long)]
    stdin: bool,
}

#[derive(Args, Debug)]
struct LearningSkillCreateArgs {
    /// The reviewed procedure learning identifier.
    learning_id: String,
    /// The stable skill name exposed in the catalog.
    #[arg(long)]
    skill_name: String,
    /// Optional description override.
    #[arg(long)]
    description: Option<String>,
    /// Optional when-to-use guidance stored in the skill frontmatter.
    #[arg(long)]
    when_to_use: Option<String>,
    /// Optional version string stored in the skill frontmatter.
    #[arg(long)]
    version: Option<String>,
    /// The execution context used when the promoted skill is activated.
    #[arg(long, value_enum, default_value_t = LearningSkillContextArg::Fork)]
    context: LearningSkillContextArg,
    /// Optional child profile used for forked skill execution.
    #[arg(long)]
    agent_profile: Option<String>,
    /// Optional provider override used for forked skill execution.
    #[arg(long)]
    provider: Option<String>,
    /// Optional primary model override used for forked skill execution.
    #[arg(long)]
    model: Option<String>,
    /// Optional fallback model override used for forked skill execution.
    #[arg(long)]
    fallback_model: Option<String>,
    /// Optional initial rollout status for the promoted skill.
    #[arg(long, value_enum)]
    status: Option<LearningSkillStatusArg>,
    /// Preferred tools declared by the promoted skill.
    #[arg(long = "allow-tool")]
    allowed_tools: Vec<String>,
    /// Tools the promoted skill should avoid.
    #[arg(long = "block-tool")]
    blocked_tools: Vec<String>,
    /// Inline skill instructions.
    instructions: Option<String>,
    /// Read skill instructions from a file.
    #[arg(long)]
    instructions_file: Option<PathBuf>,
    /// Read skill instructions from standard input.
    #[arg(long)]
    stdin: bool,
}

#[derive(Args, Debug)]
struct LearningSkillRolloutResultArgs {
    /// The promoted procedural skill name.
    skill_name: String,
    /// The rollout gate this run should satisfy.
    #[arg(long, value_enum)]
    kind: LearningSkillRolloutKindArg,
    /// The daemon run used as rollout evidence.
    #[arg(long)]
    run_id: String,
    /// Required marker that must appear in the latest daemon output.
    #[arg(long)]
    expected_output_contains: String,
    /// Optional current promoted-skill definition fingerprint guard.
    #[arg(long)]
    definition_fingerprint: Option<String>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum DerivationProfileArg {
    CanonicalText,
    VisualPreview,
}

impl From<DerivationProfileArg> for DerivationProfile {
    fn from(value: DerivationProfileArg) -> Self {
        match value {
            DerivationProfileArg::CanonicalText => Self::CanonicalText,
            DerivationProfileArg::VisualPreview => Self::VisualPreview,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ObservationSourceKindArg {
    ScreenSnapshot,
    WebcamSnapshot,
    MicrophoneSegment,
}

impl From<ObservationSourceKindArg> for ObservationSourceKind {
    fn from(value: ObservationSourceKindArg) -> Self {
        match value {
            ObservationSourceKindArg::ScreenSnapshot => Self::ScreenSnapshot,
            ObservationSourceKindArg::WebcamSnapshot => Self::WebcamSnapshot,
            ObservationSourceKindArg::MicrophoneSegment => Self::MicrophoneSegment,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum CaptureOsProfileArg {
    Macos,
    Linux,
    Windows,
}

impl From<CaptureOsProfileArg> for CaptureOsProfile {
    fn from(value: CaptureOsProfileArg) -> Self {
        match value {
            CaptureOsProfileArg::Macos => Self::Macos,
            CaptureOsProfileArg::Linux => Self::Linux,
            CaptureOsProfileArg::Windows => Self::Windows,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ObservationSensitivityArg {
    Standard,
    Sensitive,
}

impl From<ObservationSensitivityArg> for ObservationSensitivity {
    fn from(value: ObservationSensitivityArg) -> Self {
        match value {
            ObservationSensitivityArg::Standard => Self::Standard,
            ObservationSensitivityArg::Sensitive => Self::Sensitive,
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ObservationRawAssetPolicyArg {
    Auto,
    Never,
    Always,
}

impl From<ObservationRawAssetPolicyArg> for ObservationRawAssetPolicy {
    fn from(value: ObservationRawAssetPolicyArg) -> Self {
        match value {
            ObservationRawAssetPolicyArg::Auto => Self::Auto,
            ObservationRawAssetPolicyArg::Never => Self::Never,
            ObservationRawAssetPolicyArg::Always => Self::Always,
        }
    }
}

#[derive(Args, Debug)]
struct ObservationSourceCreateArgs {
    display_name: String,
    #[arg(long)]
    source_id: Option<String>,
    #[arg(long, value_enum)]
    kind: ObservationSourceKindArg,
    #[arg(long)]
    upload_token: Option<String>,
    #[arg(long)]
    upload_token_file: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = ObservationSensitivityArg::Sensitive)]
    sensitivity: ObservationSensitivityArg,
    #[arg(long, default_value_t = 7 * 24 * 60 * 60)]
    retention_seconds: u64,
    #[arg(long, default_value_t = 512)]
    max_active_observations: u64,
    #[arg(long, default_value_t = 512 * 1024 * 1024)]
    max_active_bytes: u64,
    #[arg(long, default_value_t = 60_000)]
    ingest_rate_limit_window_ms: u64,
    #[arg(long, default_value_t = 120)]
    ingest_rate_limit_burst: u64,
    #[arg(long)]
    purge_raw_on_retention: bool,
    #[arg(long)]
    disable_materialization: bool,
    #[arg(long)]
    allow_output_delivery: bool,
}

#[derive(Args, Debug)]
struct ObservationSourceRotateTokenArgs {
    source_id: String,
    #[arg(long)]
    upload_token: Option<String>,
    #[arg(long)]
    upload_token_file: Option<PathBuf>,
    #[arg(long, default_value_t = 0)]
    grace_period_ms: u64,
}

#[derive(Args, Debug)]
struct ObservationSourceRevokeTokenArgs {
    source_id: String,
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Args, Debug)]
struct ObservationAuditArgs {
    #[arg(long)]
    source_id: Option<String>,
    #[arg(long)]
    event: Option<String>,
    #[arg(long, default_value_t = 100)]
    limit: usize,
}

#[derive(Args, Debug)]
struct ObservationIngestArgs {
    source_id: String,
    path: PathBuf,
    #[arg(long)]
    media_type: Option<String>,
    #[arg(long)]
    idempotency_key: String,
    #[arg(long)]
    captured_at_ms: Option<u64>,
    #[arg(long)]
    stream_id: Option<String>,
    #[arg(long)]
    seq_no: Option<u64>,
    #[arg(long)]
    canonical_text: Option<String>,
    #[arg(long)]
    canonical_text_file: Option<PathBuf>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg(long)]
    upload_token: Option<String>,
    #[arg(long)]
    upload_token_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct ObservationMaterializeArgs {
    target_session_id: String,
    #[arg(long = "observation-id")]
    observation_ids: Vec<String>,
    #[arg(long)]
    source_id: Option<String>,
    #[arg(long)]
    stream_id: Option<String>,
    #[arg(long)]
    capture_group_id: Option<String>,
    #[arg(long, default_value_t = 3)]
    max_observations: u64,
    #[arg(long)]
    lookback_seconds: Option<u64>,
    content: Option<String>,
    #[arg(long)]
    content_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg(long)]
    reply_plugin: Option<String>,
    #[arg(long)]
    reply_address: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    without_raw_assets: bool,
    #[arg(long, value_enum)]
    raw_assets_policy: Option<ObservationRawAssetPolicyArg>,
    #[arg(long)]
    allow_empty: bool,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,
    #[command(flatten)]
    generation: GenerationArgs,
}

#[derive(Args, Debug)]
struct ObservationScheduleArgs {
    name: String,
    session_id: String,
    #[arg(long = "observation-id")]
    observation_ids: Vec<String>,
    #[arg(long)]
    source_id: Option<String>,
    #[arg(long)]
    stream_id: Option<String>,
    #[arg(long)]
    capture_group_id: Option<String>,
    #[arg(long, default_value_t = 3)]
    max_observations: u64,
    #[arg(long)]
    lookback_seconds: Option<u64>,
    content: Option<String>,
    #[arg(long)]
    content_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg(long)]
    reply_plugin: Option<String>,
    #[arg(long)]
    reply_address: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    without_raw_assets: bool,
    #[arg(long, value_enum)]
    raw_assets_policy: Option<ObservationRawAssetPolicyArg>,
    #[arg(long)]
    allow_empty: bool,
    #[arg(long)]
    at: Option<String>,
    #[arg(long)]
    every_seconds: Option<u64>,
    #[arg(long)]
    cron: Option<String>,
    #[arg(long)]
    timezone: Option<String>,
    #[arg(long, value_enum)]
    overlap_policy: Option<ScheduleOverlapPolicyArg>,
    #[arg(long, value_enum)]
    misfire_policy: Option<ScheduleMisfirePolicyArg>,
    #[arg(long)]
    max_executions: Option<u64>,
    #[command(flatten)]
    generation: GenerationArgs,
}

#[derive(Subcommand, Debug)]
enum SecretsCommand {
    /// Generate one new auth-store master key.
    Generate,
    /// List daemon-managed auth secret slots.
    List(SecretStoreArgs),
    /// Fetch one daemon-managed auth secret slot status.
    Get(SecretGetArgs),
    /// Store or rotate one static API-key secret slot.
    Set(SecretSetArgs),
    /// Import one OpenAI Codex account auth file into the global secret store.
    ImportCodex(SecretImportCodexArgs),
    /// Import one Anthropic Claude Code credentials file into the global secret store.
    ImportClaudeCode(SecretImportClaudeCodeArgs),
    /// Delete one daemon-managed auth secret slot.
    Delete(SecretDeleteArgs),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum HttpAuthModeArg {
    Auto,
    None,
    Bearer,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum McpDiscoveryArg {
    Auto,
    Disabled,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum LogFormatArg {
    Auto,
    Pretty,
    Json,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum LogLevelArg {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogFormatArg {
    fn resolve(self) -> LogFormat {
        match self {
            Self::Auto => {
                if std::io::stderr().is_terminal() {
                    LogFormat::Pretty
                } else {
                    LogFormat::Json
                }
            }
            Self::Pretty => LogFormat::Pretty,
            Self::Json => LogFormat::Json,
        }
    }
}

impl From<LogLevelArg> for LogLevel {
    fn from(value: LogLevelArg) -> Self {
        match value {
            LogLevelArg::Error => LogLevel::Error,
            LogLevelArg::Warn => LogLevel::Warn,
            LogLevelArg::Info => LogLevel::Info,
            LogLevelArg::Debug => LogLevel::Debug,
            LogLevelArg::Trace => LogLevel::Trace,
        }
    }
}

#[derive(Args, Clone, Debug, Default)]
struct ListPaginationArgs {
    /// Return a cursor-paginated envelope instead of the legacy array.
    #[arg(long)]
    page: bool,
    /// Maximum number of items to return; without --page or --cursor, preserves legacy array output.
    #[arg(long)]
    limit: Option<usize>,
    /// Cursor returned by a previous paginated response.
    #[arg(long)]
    cursor: Option<String>,
}

impl ListPaginationArgs {
    fn wants_page(&self) -> bool {
        self.page || self.cursor.is_some()
    }

    fn append_query_params(&self, params: &mut Vec<String>) {
        if self.page || self.cursor.is_some() {
            params.push("page=true".to_string());
        }
        if let Some(limit) = self.limit {
            params.push(format!("limit={limit}"));
        }
        if let Some(cursor) = self.cursor.as_deref() {
            params.push(format!(
                "cursor={}",
                crate::cli::url_encode_component(cursor)
            ));
        }
    }
}

#[derive(Subcommand, Debug)]
enum SessionsCommand {
    /// List known sessions.
    List {
        #[command(flatten)]
        pagination: ListPaginationArgs,
    },
    /// Create a new session.
    #[command(visible_alias = "open")]
    Create {
        session_id: Option<String>,
        #[arg(long)]
        thread_id: Option<String>,
        #[arg(long)]
        persona_id: Option<String>,
        #[arg(long)]
        capability_scope_json: Option<String>,
        #[arg(long)]
        capability_scope_file: Option<PathBuf>,
        #[arg(long)]
        credential_scope_json: Option<String>,
        #[arg(long)]
        credential_scope_file: Option<PathBuf>,
    },
    /// Fetch one session snapshot.
    #[command(visible_alias = "show")]
    Get { session_id: String },
    /// Fetch the effective memory projection for one session.
    MemoryContext {
        session_id: String,
        #[arg(long)]
        query: Option<String>,
    },
    /// Search the effective memory surface for one session.
    MemorySearch {
        session_id: String,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// List the skills currently visible to one session.
    Skills {
        session_id: String,
        #[arg(long)]
        query: Option<String>,
    },
    /// Fetch the persisted event log for a session.
    Events { session_id: String },
    /// Follow the session SSE event stream.
    #[command(visible_alias = "watch", visible_alias = "follow")]
    Stream {
        session_id: String,
        #[arg(long)]
        cursor: Option<u64>,
    },
    /// Submit one text input to a session.
    #[command(visible_alias = "send")]
    Input(SessionInputArgs),
    /// Inspect or mutate the durable session goal.
    Goal {
        #[command(subcommand)]
        command: SessionGoalCommand,
    },
    /// Replace the default route policy for one session.
    SetRoute(SessionSetRouteArgs),
    /// Replace the persisted capability scope for one idle session.
    SetCapabilityScope(SessionCapabilityScopeArgs),
    /// Replace the persisted credential scope for one idle session.
    SetCredentialScope(SessionCredentialScopeArgs),
    /// Replace the persisted reply-target defaults for one session.
    SetReplyTargets(SessionReplyTargetsArgs),
    /// Clear the persisted reply-target defaults for one session.
    ClearReplyTargets { session_id: String },
    /// Bind one persona snapshot to an idle session.
    SetPersona {
        session_id: String,
        persona_id: String,
    },
    /// Clear the bound persona snapshot from an idle session.
    ClearPersona { session_id: String },
    /// Approve one pending tool request.
    Approve(ApprovalArgs),
    /// Deny one pending tool request.
    Deny(DenyApprovalArgs),
    /// Interrupt the in-flight run for a session.
    Interrupt { session_id: String },
    /// Explicitly end one session and fire SessionEnd hooks.
    End {
        session_id: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SessionGoalCommand {
    /// Fetch the current goal.
    Get { session_id: String },
    /// Replace the current goal.
    Set {
        session_id: String,
        objective: String,
        #[arg(long)]
        token_budget: Option<u64>,
        #[arg(long)]
        status: Option<String>,
    },
    /// Clear the current goal.
    Clear { session_id: String },
}

#[derive(Subcommand, Debug)]
enum PersonasCommand {
    /// List known personas.
    List {
        #[arg(long)]
        query: Option<String>,
        #[command(flatten)]
        pagination: ListPaginationArgs,
    },
    /// Create one new persona.
    Create(CreatePersonaArgs),
    /// Import one new persona from a Markdown file.
    Import(ImportPersonaArgs),
    /// Fetch one persona.
    #[command(visible_alias = "show")]
    Get { persona_id: String },
    /// Update one existing persona.
    Update(UpdatePersonaArgs),
}

#[derive(Subcommand, Debug)]
enum ConnectorsCommand {
    /// List known runtime connectors.
    List,
    /// Fetch one runtime connector.
    Get { kind: String, name: String },
    /// Create or update one external sidecar connector from JSON input.
    PutExternal(ConnectorPutArgs),
    /// Create or update one Telegram connector from JSON input.
    PutTelegram(ConnectorPutArgs),
    /// Create or update one Slack connector from JSON input.
    PutSlack(ConnectorPutArgs),
    /// Create or update one HTTP connector from JSON input.
    PutHttp(ConnectorPutArgs),
    /// Delete one runtime connector.
    Delete { kind: String, name: String },
}

#[derive(Subcommand, Debug)]
enum DeliveriesCommand {
    /// List pending, delivered, and dead-lettered output deliveries.
    List(DeliveryListArgs),
    /// List dead-lettered output deliveries.
    DeadLetter(DeliveryListArgs),
    /// Fetch one redacted delivery view.
    Get { delivery_id: String },
    /// Replay one dead-lettered delivery by creating a new pending item.
    Replay {
        delivery_id: String,
        /// Create another replay even if this dead-letter already has a replay.
        #[arg(long)]
        force: bool,
    },
    /// Mark one dead-lettered delivery as operator-resolved without deleting it.
    Resolve {
        delivery_id: String,
        /// Operator reason stored in the resolved-DLQ ledger with secret-looking spans redacted.
        #[arg(long)]
        reason: String,
    },
    /// Replay a bounded batch of unresolved dead-lettered deliveries.
    ReplayBulk(DeliveryBulkReplayArgs),
    /// Reset persisted target backpressure/circuit state by redacted target or plugin.
    ResetBackpressure(DeliveryBackpressureResetArgs),
}

#[derive(Args, Debug)]
struct DeliveryListArgs {
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long)]
    plugin: Option<String>,
    #[arg(long, value_enum)]
    status: Option<DeliveryStatusArg>,
    #[command(flatten)]
    pagination: ListPaginationArgs,
}

#[derive(Args, Debug)]
struct DeliveryBulkReplayArgs {
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long)]
    plugin: Option<String>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    force: bool,
    /// Include manually resolved or already replay-delivered dead letters.
    #[arg(long)]
    include_resolved: bool,
}

#[derive(Args, Debug)]
struct DeliveryBackpressureResetArgs {
    /// Redacted target digest from `deliveries list/get`, for example `http:address_sha256:...`.
    #[arg(long)]
    target: Option<String>,
    /// Reset all currently persisted target backpressure entries for one plugin.
    #[arg(long)]
    plugin: Option<String>,
    /// Show matching targets without mutating backpressure state.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand, Debug)]
enum RunsCommand {
    /// List detached runs.
    List {
        #[arg(long)]
        session_id: Option<String>,
        #[command(flatten)]
        pagination: ListPaginationArgs,
    },
    /// Fetch one run snapshot.
    #[command(visible_alias = "show")]
    Get { run_id: String },
    /// Fetch the signed external-action audit records for one run.
    ExternalActions { run_id: String },
    /// Fetch the persisted event log for one run.
    Events { run_id: String },
    /// Follow the live SSE stream for one run.
    #[command(visible_alias = "watch", visible_alias = "follow")]
    Stream {
        run_id: String,
        #[arg(long)]
        cursor: Option<u64>,
    },
    /// Wait until one run reaches a terminal state.
    Wait(WaitRunArgs),
    /// Show the stored debug bundle for one run.
    Debug { run_id: String },
    /// Print one stored debug artifact for one run.
    DebugArtifact { run_id: String, artifact_id: String },
    /// Prune debug evidence for old terminal runs.
    Prune(RunsPruneArgs),
    /// Cancel one detached run.
    Cancel { run_id: String },
}

#[derive(Args, Debug)]
struct RunsPruneArgs {
    /// Only debug evidence for terminal runs older than this age is eligible.
    #[arg(long)]
    older_than_ms: u64,
    /// Scope pruning to one session.
    #[arg(long)]
    session_id: Option<String>,
    /// Maximum number of candidate debug bundles to prune or report.
    #[arg(long)]
    limit: Option<usize>,
    /// Report candidates without deleting evidence.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand, Debug)]
enum ApprovalsCommand {
    /// List pending approvals, optionally scoped to one session.
    List {
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Show one pending approval by identifier.
    Show {
        request_id: String,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Approve one pending tool request.
    Allow(ApprovalArgs),
    /// Approve every pending tool request, optionally within one session.
    #[command(visible_alias = "approve-all")]
    AllowAll(BulkApprovalArgs),
    /// Deny one pending tool request.
    Deny(DenyApprovalArgs),
    /// Deny every pending tool request, optionally within one session.
    DenyAll(BulkDenyApprovalArgs),
}

#[derive(Subcommand, Debug)]
enum QuestionsCommand {
    /// List pending structured user-question requests.
    List {
        #[arg(long)]
        session_id: Option<String>,
        #[command(flatten)]
        pagination: ListPaginationArgs,
    },
    /// Show one pending user-question request by identifier.
    Show {
        request_id: String,
        #[arg(long)]
        session_id: Option<String>,
    },
    /// Answer one pending structured user-question request.
    Answer(QuestionAnswerArgs),
    /// Cancel the run waiting on one structured user-question request.
    Cancel(QuestionCancelArgs),
}

#[derive(Subcommand, Debug)]
enum AgentsCommand {
    /// List tracked agents.
    List,
    /// List lightweight tracked-agent summaries.
    Summaries {
        /// Restrict summaries to one root agent tree.
        #[arg(long, alias = "root")]
        root_agent_id: Option<String>,
        /// Restrict summaries to one session identifier.
        #[arg(long)]
        session_id: Option<String>,
        /// Restrict summaries to one agent lifecycle status.
        #[arg(long)]
        status: Option<AgentStatusArg>,
        /// Restrict summaries by whether a live runtime actor is present.
        #[arg(long)]
        has_runtime: Option<bool>,
        #[command(flatten)]
        pagination: ListPaginationArgs,
    },
    /// List durable supervisor lifecycle audit entries.
    Audit {
        #[arg(long)]
        agent_id: Option<String>,
    },
    /// Fetch one agent snapshot.
    #[command(visible_alias = "show")]
    Get { agent_id: String },
    /// Set the visible nickname for one tracked agent.
    Rename { agent_id: String, nickname: String },
    /// Clear the visible nickname for one tracked agent.
    ClearNickname { agent_id: String },
    /// Spawn a sidechain agent under a parent agent.
    SpawnSidechain(SpawnSidechainArgs),
    /// Dry-run a sidechain spawn and explain the policy decision.
    ExplainSidechain(SpawnSidechainArgs),
    /// Inspect one agent mailbox.
    DrainMailbox { agent_id: String },
    /// Inspect one agent mailbox dead-letter queue.
    MailboxDeadLetters { agent_id: String },
    /// Acknowledge and remove one pending mailbox message.
    AckMailbox {
        agent_id: String,
        message_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum TasksCommand {
    /// List tasks for one session.
    List {
        session_id: String,
        #[arg(long, value_enum)]
        status: Option<TaskStatusArg>,
        #[command(flatten)]
        pagination: ListPaginationArgs,
    },
    /// Fetch one task by identifier.
    Get { session_id: String, task_id: String },
    /// Read the latest output view for one task.
    Output(TaskOutputArgs),
    /// Stop one task.
    Stop(TaskStopArgs),
}

#[derive(Subcommand, Debug)]
enum SchedulesCommand {
    /// List schedules, optionally scoped to one session.
    List {
        #[arg(long)]
        session_id: Option<String>,
        #[command(flatten)]
        pagination: ListPaginationArgs,
    },
    /// Fetch one schedule by identifier.
    Get { schedule_id: String },
    /// Create one durable schedule.
    Create(CreateScheduleArgs),
    /// Cancel one schedule.
    Cancel { schedule_id: String },
    /// Pause one schedule.
    Pause { schedule_id: String },
    /// Resume one schedule.
    Resume { schedule_id: String },
    /// Trigger one schedule immediately.
    TriggerNow { schedule_id: String },
}

#[derive(Subcommand, Debug)]
enum MailboxesCommand {
    /// Post one mailbox message.
    Post(PostMailboxArgs),
}

#[derive(Args, Debug)]
struct SessionInputArgs {
    session_id: String,
    content: Option<String>,
    #[arg(long)]
    content_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long = "file")]
    files: Vec<PathBuf>,
    #[arg(long = "asset")]
    asset_ids: Vec<String>,
    #[arg(long)]
    source_plugin: Option<String>,
    #[arg(long)]
    source_kind: Option<String>,
    #[arg(long)]
    actor_id: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg(long)]
    reply_plugin: Option<String>,
    #[arg(long)]
    reply_address: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    require_workspace_file_write: bool,
    #[arg(long)]
    require_workspace_file_path: Option<String>,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,
    #[command(flatten)]
    generation: GenerationArgs,
}

#[derive(Args, Debug)]
struct SessionSetRouteArgs {
    session_id: String,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    clear: bool,
    #[command(flatten)]
    generation: GenerationArgs,
}

#[derive(Args, Debug)]
struct SessionCapabilityScopeArgs {
    session_id: String,
    #[arg(long)]
    clear: bool,
    #[arg(long)]
    capability_scope_json: Option<String>,
    #[arg(long)]
    capability_scope_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct SessionCredentialScopeArgs {
    session_id: String,
    #[arg(long)]
    clear: bool,
    #[arg(long)]
    credential_scope_json: Option<String>,
    #[arg(long)]
    credential_scope_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct SessionReplyTargetsArgs {
    session_id: String,
    #[arg(long)]
    reply_targets_json: Option<String>,
    #[arg(long)]
    reply_targets_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct ConnectorPutArgs {
    name: String,
    #[arg(long)]
    json: Option<String>,
    #[arg(long)]
    file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct CreatePersonaArgs {
    display_name: String,
    #[arg(long)]
    persona_id: Option<String>,
    #[arg(long)]
    soul: Option<String>,
    #[arg(long)]
    soul_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg(long)]
    capability_scope_json: Option<String>,
    #[arg(long)]
    capability_scope_file: Option<PathBuf>,
    #[arg(long)]
    default_skills_json: Option<String>,
    #[arg(long)]
    default_skills_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct ImportPersonaArgs {
    /// The Markdown file whose contents become the persona soul.
    path: PathBuf,
    /// Optional override for the imported display name.
    #[arg(long)]
    display_name: Option<String>,
    /// Optional caller-selected persona identifier.
    #[arg(long)]
    persona_id: Option<String>,
    /// Optional inline JSON metadata stored with the persona.
    #[arg(long)]
    metadata_json: Option<String>,
    /// Optional JSON file containing persona metadata.
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    /// Optional inline JSON capability scope baseline.
    #[arg(long)]
    capability_scope_json: Option<String>,
    /// Optional JSON file containing the capability scope baseline.
    #[arg(long)]
    capability_scope_file: Option<PathBuf>,
    /// Optional inline JSON default inline-skill assignments.
    #[arg(long)]
    default_skills_json: Option<String>,
    /// Optional JSON file containing default inline-skill assignments.
    #[arg(long)]
    default_skills_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct UpdatePersonaArgs {
    persona_id: String,
    #[arg(long)]
    display_name: Option<String>,
    #[arg(long)]
    soul: Option<String>,
    #[arg(long)]
    soul_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    metadata_json: Option<String>,
    #[arg(long)]
    metadata_file: Option<PathBuf>,
    #[arg(long)]
    capability_scope_json: Option<String>,
    #[arg(long)]
    capability_scope_file: Option<PathBuf>,
    #[arg(long)]
    default_skills_json: Option<String>,
    #[arg(long)]
    default_skills_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct AssetImportArgs {
    path: PathBuf,
    #[arg(long)]
    media_type: Option<String>,
}

#[derive(Args, Debug, Clone)]
struct SecretStoreArgs {
    #[arg(long, env = "KHEISH_STATE_ROOT")]
    state_root: Option<PathBuf>,
    #[arg(long)]
    offline: bool,
}

#[derive(Args, Debug)]
struct SecretGetArgs {
    secret_ref: String,
    #[command(flatten)]
    store: SecretStoreArgs,
}

#[derive(Args, Debug)]
struct SecretDeleteArgs {
    secret_ref: String,
    #[command(flatten)]
    store: SecretStoreArgs,
}

#[derive(Args, Debug)]
struct SecretSetArgs {
    secret_ref: String,
    #[arg(long, value_enum)]
    provider: SecretProviderKind,
    #[arg(long)]
    value: Option<String>,
    #[arg(long)]
    from_env: Option<String>,
    #[arg(long)]
    from_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    organization: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[command(flatten)]
    store: SecretStoreArgs,
}

#[derive(Args, Debug)]
struct SecretImportCodexArgs {
    secret_ref: String,
    #[arg(long)]
    file: Option<PathBuf>,
    #[arg(long)]
    organization: Option<String>,
    #[arg(long)]
    project: Option<String>,
    #[command(flatten)]
    store: SecretStoreArgs,
}

#[derive(Args, Debug)]
struct SecretImportClaudeCodeArgs {
    secret_ref: String,
    #[arg(long)]
    file: Option<PathBuf>,
    #[command(flatten)]
    store: SecretStoreArgs,
}

#[derive(Args, Debug)]
struct QuestionAnswerArgs {
    request_id: String,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long)]
    answers_json: Option<String>,
    #[arg(long)]
    answers_file: Option<PathBuf>,
    #[arg(long)]
    interactive: bool,
    #[arg(long)]
    declined: bool,
    #[arg(long)]
    justification: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 200)]
    poll_interval_ms: u64,
}

#[derive(Args, Debug)]
struct QuestionCancelArgs {
    request_id: String,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    run_id: Option<String>,
    #[arg(long)]
    justification: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct ApprovalArgs {
    session_id: String,
    request_id: String,
    #[arg(long)]
    updated_input_json: Option<String>,
    #[arg(long)]
    updated_input_file: Option<PathBuf>,
    #[arg(long)]
    justification: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,
}

#[derive(Args, Debug)]
struct DenyApprovalArgs {
    session_id: String,
    request_id: String,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long)]
    justification: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,
}

#[derive(Args, Debug)]
struct BulkApprovalArgs {
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    justification: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,
}

#[derive(Args, Debug)]
struct BulkDenyApprovalArgs {
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long)]
    justification: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,
}

#[derive(Args, Debug)]
struct WaitRunArgs {
    run_id: String,
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,
}

#[derive(Args, Debug)]
struct SpawnSidechainArgs {
    parent_agent_id: String,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    thread_id: Option<String>,
    #[arg(long, default_value = "")]
    parent_assistant_message: String,
    #[arg(long, default_value = "")]
    system_prompt: String,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long, default_value_t = false)]
    append_system_prompt: bool,
    #[arg(long = "inherited-tool-call-id")]
    inherited_tool_call_ids: Vec<String>,
    #[arg(long)]
    worktree_path: Option<String>,
    #[arg(long, value_enum)]
    retention: Option<ChildRetentionPolicyArg>,
    #[arg(long)]
    nickname: Option<String>,
    #[arg(long)]
    spawn_request_id: Option<String>,
    #[arg(long = "allow-tool")]
    allowed_tools: Vec<String>,
    #[arg(long = "deny-tool")]
    blocked_tools: Vec<String>,
    #[arg(long)]
    capability_scope_json: Option<String>,
    #[arg(long)]
    capability_scope_file: Option<PathBuf>,
    #[arg(long)]
    credential_scope_json: Option<String>,
    #[arg(long)]
    credential_scope_file: Option<PathBuf>,
    #[command(flatten)]
    generation: GenerationArgs,
    #[arg(long)]
    subtask_name: Option<String>,
    #[arg(long)]
    subtask_description: Option<String>,
    #[arg(long)]
    subtask_content: Option<String>,
    #[arg(long)]
    subtask_content_file: Option<PathBuf>,
    #[arg(long)]
    subtask_stdin: bool,
}

#[derive(Args, Debug)]
struct PostMailboxArgs {
    #[arg(long)]
    message_id: Option<String>,
    #[arg(long)]
    from_agent_id: String,
    #[arg(long)]
    to_agent_id: String,
    #[arg(long)]
    subject: String,
    #[arg(long)]
    ttl_ms: Option<u64>,
    #[arg(long)]
    payload_json: Option<String>,
    #[arg(long)]
    payload_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct TaskOutputArgs {
    session_id: String,
    task_id: String,
    #[arg(long)]
    wait: bool,
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
    #[arg(long)]
    tail_bytes: Option<usize>,
    #[arg(long)]
    full: bool,
}

#[derive(Args, Debug)]
struct TaskStopArgs {
    session_id: String,
    task_id: String,
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Args, Debug, Clone)]
struct CreateScheduleArgs {
    name: String,
    session_id: String,
    content: Option<String>,
    #[arg(long)]
    content_file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long)]
    at: Option<String>,
    #[arg(long)]
    every_seconds: Option<u64>,
    #[arg(long)]
    cron: Option<String>,
    #[arg(long)]
    timezone: Option<String>,
    #[arg(long, value_enum)]
    overlap_policy: Option<ScheduleOverlapPolicyArg>,
    #[arg(long, value_enum)]
    misfire_policy: Option<ScheduleMisfirePolicyArg>,
    #[arg(long)]
    max_executions: Option<u64>,
    #[arg(long)]
    provider: Option<String>,
    #[command(flatten)]
    generation: GenerationArgs,
}

#[derive(Args, Debug, Default, Clone)]
struct GenerationArgs {
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    fallback_model: Option<String>,
    #[arg(long)]
    temperature: Option<f32>,
    #[arg(long)]
    max_output_tokens: Option<u32>,
    #[arg(long, value_enum)]
    reasoning_effort: Option<ReasoningEffortArg>,
    #[arg(long, value_enum)]
    reasoning_summary: Option<ReasoningSummaryArg>,
    #[arg(long)]
    reasoning_budget_tokens: Option<u32>,
    #[arg(long)]
    reasoning_interleaved: bool,
    #[arg(long, value_enum)]
    tool_choice: Option<ToolChoiceArg>,
    #[arg(long)]
    tool_name: Option<String>,
    #[arg(long)]
    serial_tools: bool,
    #[arg(long, value_enum)]
    response_format: Option<ResponseFormatArg>,
    #[arg(long)]
    response_schema_json: Option<String>,
    #[arg(long)]
    response_schema_file: Option<PathBuf>,
}

#[derive(Clone, Debug, ValueEnum)]
enum ChildRetentionPolicyArg {
    Retain,
    CloseOnSettle,
}

impl ChildRetentionPolicyArg {
    fn into_retention(self) -> ChildRetentionPolicy {
        match self {
            Self::Retain => ChildRetentionPolicy::Retain,
            Self::CloseOnSettle => ChildRetentionPolicy::CloseOnSettle,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Pretty,
    Json,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ProviderKind {
    Anthropic,
    Google,
    Openai,
    Openrouter,
    Xai,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum SecretProviderKind {
    Generic,
    Anthropic,
    Google,
    Openai,
    Openrouter,
    Xai,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum OpenAiAuthSourceArg {
    ApiKey,
    Codex,
    Auto,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ResolvedOpenAiAuthSource {
    ApiKey,
    Codex,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum AnthropicAuthSourceArg {
    ApiKey,
    ClaudeCode,
    Auto,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ResolvedAnthropicAuthSource {
    ApiKey,
    ClaudeCode,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum PermissionModeArg {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
    DontAsk,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ScheduleOverlapPolicyArg {
    Skip,
    QueueOne,
    Parallel,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ScheduleMisfirePolicyArg {
    CoalesceOnce,
    SkipMissed,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ToolChoiceArg {
    Auto,
    None,
    Required,
    Specific,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ResponseFormatArg {
    Text,
    StructuredJson,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ReasoningEffortArg {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum ReasoningSummaryArg {
    Auto,
    Concise,
    Detailed,
    None,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum AgentStatusArg {
    Idle,
    Running,
    WaitingForApproval,
    WaitingForUserInput,
    Failed,
    Completed,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum TaskStatusArg {
    Pending,
    InProgress,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
enum DeliveryStatusArg {
    Pending,
    Retrying,
    Delivered,
    DeadLettered,
}

impl DeliveryStatusArg {
    fn as_query_value(self) -> &'static str {
        match self {
            DeliveryStatusArg::Pending => "pending",
            DeliveryStatusArg::Retrying => "retrying",
            DeliveryStatusArg::Delivered => "delivered",
            DeliveryStatusArg::DeadLettered => "dead_lettered",
        }
    }
}

#[derive(Serialize)]
struct DoctorView {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<kheish_daemon::DaemonStatusView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_error: Option<String>,
    session_count: usize,
    pending_approvals: usize,
    pending_questions: usize,
    readyz_reachable: bool,
    events_stream_reachable: bool,
    checks: Vec<DoctorCheckView>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Serialize)]
struct DoctorCheckView {
    name: String,
    code: String,
    ok: bool,
    severity: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    related_id: Option<String>,
}

#[derive(Serialize)]
struct DoctorRoutesView {
    ok: bool,
    source: String,
    default_route: Option<String>,
    route_count: usize,
    auth_checked: bool,
    reference_checked: bool,
    canary_checked: bool,
    routes: Vec<DoctorRouteView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    canaries: Vec<DoctorRouteCanaryView>,
    diagnostics: Vec<kheish_daemon::RouteDiagnosticView>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Clone, Serialize)]
struct DoctorRouteView {
    route_id: String,
    provider: String,
    model: String,
    auth_ref: Option<String>,
    auth_kind: String,
    model_support: Option<kheish_daemon::ModelSupportPolicy>,
    capabilities: Option<kheish_daemon::RouteCapabilities>,
    #[serde(skip)]
    account_auth_slot: Option<String>,
    #[serde(skip)]
    account_auth_provider: Option<kheish_auth::AuthProvider>,
    #[serde(skip)]
    account_auth_file: Option<std::path::PathBuf>,
}

#[derive(Clone, Serialize)]
struct DoctorRouteCanaryView {
    route_id: String,
    provider: String,
    model: String,
    session_id: String,
    run_id: Option<String>,
    status: String,
    duration_ms: u64,
    message: String,
}

#[derive(Deserialize, Serialize)]
struct AcceptedResponse {
    accepted: bool,
}

#[derive(Debug, Serialize)]
struct StreamEventView {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    event: String,
    data: DaemonEvent,
}

struct ApprovalResolutionRequest {
    session_id: String,
    run_id: Option<String>,
    idempotency_key: Option<String>,
    resolution: ApprovalResolution,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::process::ExitCode {
    match cli::app::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:?}");
            cli::exit_code_for_error(&error)
        }
    }
}

impl TaskStatusArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    #[allow(dead_code)]
    fn into_task_status(self) -> TaskStatus {
        match self {
            Self::Pending => TaskStatus::Pending,
            Self::InProgress => TaskStatus::InProgress,
            Self::Blocked => TaskStatus::Blocked,
            Self::Completed => TaskStatus::Completed,
            Self::Failed => TaskStatus::Failed,
            Self::Cancelled => TaskStatus::Cancelled,
        }
    }
}

impl AgentStatusArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::WaitingForApproval => "waiting_for_approval",
            Self::WaitingForUserInput => "waiting_for_user_input",
            Self::Failed => "failed",
            Self::Completed => "completed",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Mutex, OnceLock};

    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    use super::*;
    use crate::cli::app::normalize_cli_args;
    use crate::cli::builders::{build_observation_selection, resolve_observation_raw_asset_policy};
    use crate::cli::commands::personas::{
        derive_persona_display_name_from_markdown, ensure_markdown_persona_path,
        read_persona_markdown_import,
    };
    use crate::cli::commands::secrets::{run_local_secrets_command, run_secrets_command};
    use crate::cli::secrets::read_secret_value;
    use crate::cli::serve::*;
    use crate::cli::{
        DaemonHttpClient, Printer, build_schedule_create_request, build_session_route_policy,
        global_auth_store_path, parse_model_selector,
    };
    use clap::Parser;
    use kheish_daemon::ModelSupportPolicy;

    fn auth_store_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn cli_parses_runtime_permission_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "set-permission-mode",
            "dont-ask",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command: RuntimeCommand::SetPermissionMode { mode, .. },
            } => assert_eq!(mode, PermissionModeArg::DontAsk),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_channel_autonomy_policy_flags() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "channels",
            "create",
            "ops",
            "--channel-id",
            "ops-room",
            "--max-parallel-public-speakers",
            "2",
            "--max-agent-replies-per-human-message",
            "4",
            "--member-cooldown-ms",
            "0",
            "--lease-timeout-ms",
            "5000",
            "--max-pending-stimuli",
            "8",
            "--max-autonomous-root-posts-per-hour",
            "2",
            "--max-active-autonomous-roots",
            "1",
            "--quiet-period-ms-after-root-post",
            "1000",
        ]);
        match cli.command.expect("command") {
            Command::Channels {
                command: ChannelsCommand::Create(args),
            } => {
                assert_eq!(args.channel_id.as_deref(), Some("ops-room"));
                assert_eq!(args.autonomy_policy.max_parallel_public_speakers, Some(2));
                assert_eq!(
                    args.autonomy_policy.max_agent_replies_per_human_message,
                    Some(4)
                );
                assert_eq!(args.autonomy_policy.member_cooldown_ms, Some(0));
                assert_eq!(args.autonomy_policy.lease_timeout_ms, Some(5000));
                assert_eq!(args.autonomy_policy.max_pending_stimuli, Some(8));
                assert_eq!(
                    args.autonomy_policy.max_autonomous_root_posts_per_hour,
                    Some(2)
                );
                assert_eq!(args.autonomy_policy.max_active_autonomous_roots, Some(1));
                assert_eq!(
                    args.autonomy_policy.quiet_period_ms_after_root_post,
                    Some(1000)
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_permission_check_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "permissions",
            "check",
            "bash",
            "--input-json",
            r#"{"command":"echo hi"}"#,
            "--session-id",
            "demo",
            "--mode",
            "plan",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Permissions {
                        command: RuntimePermissionsCommand::Check(args),
                    },
            } => {
                assert_eq!(args.tool_name, "bash");
                assert_eq!(args.session_id.as_deref(), Some("demo"));
                assert_eq!(args.input_json, r#"{"command":"echo hi"}"#);
                assert_eq!(args.mode, Some(PermissionModeArg::Plan));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_permission_matrix_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "permissions",
            "matrix",
            "--session-id",
            "demo",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Permissions {
                        command: RuntimePermissionsCommand::Matrix(args),
                    },
            } => {
                assert_eq!(args.session_id.as_deref(), Some("demo"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_subagent_policy_quotas_command() {
        let cli = Cli::parse_from(["kheish-daemon", "runtime", "subagent-policy", "quotas"]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::SubagentPolicy {
                        command: RuntimeSubagentPolicyCommand::Quotas,
                    },
            } => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_doctor_without_subcommand() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "doctor",
            "--cors-origin",
            "http://localhost:5173",
        ]);
        match cli.command.expect("command") {
            Command::Doctor {
                cors_origin,
                command: None,
            } => {
                assert_eq!(cors_origin.as_deref(), Some("http://localhost:5173"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_doctor_routes_diagnostics() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "doctor",
            "routes",
            "--route",
            "openai",
            "--default-route",
            "fallback",
            "--check-auth",
            "--check-references",
            "--canary",
            "--canary-timeout-ms",
            "1234",
            "--routes-file",
            "routes.toml",
        ]);
        match cli.command.expect("command") {
            Command::Doctor {
                cors_origin,
                command:
                    Some(DoctorCommand::Routes(DoctorRoutesArgs {
                        route,
                        default_route,
                        check_auth,
                        check_references,
                        canary,
                        canary_timeout_ms,
                        routes_file,
                    })),
            } => {
                assert_eq!(cors_origin, None);
                assert_eq!(route.as_deref(), Some("openai"));
                assert_eq!(default_route.as_deref(), Some("fallback"));
                assert!(check_auth);
                assert!(check_references);
                assert!(canary);
                assert_eq!(canary_timeout_ms, 1234);
                assert_eq!(routes_file.as_deref(), Some(Path::new("routes.toml")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_system_prompt_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "set-system-prompt",
            "--mode",
            "custom",
            "Reply with OK",
            "--language",
            "fr",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command: RuntimeCommand::SetSystemPrompt(args),
            } => {
                assert_eq!(args.mode, Some(SystemPromptModeArg::Custom));
                assert_eq!(args.content.as_deref(), Some("Reply with OK"));
                assert_eq!(args.language.as_deref(), Some("fr"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_mcp_profile_startup_flag() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "serve",
            "--mcp-profile",
            "docs",
            "--mcp-profile",
            "repo",
            "--event-history-capacity",
            "64",
        ]);
        match cli.command.expect("command") {
            Command::Serve(args) => {
                assert_eq!(args.mcp_profiles, vec!["docs", "repo"]);
                assert_eq!(args.event_history_capacity, 64);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_mcp_catalog_list_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "mcp",
            "catalog",
            "list",
            "--profile",
            "docs",
            "--supported-only",
        ]);
        match cli.command.expect("command") {
            Command::Mcp {
                command:
                    McpCommand::Catalog {
                        command:
                            McpCatalogCommand::List {
                                profile,
                                supported_only,
                            },
                    },
            } => {
                assert_eq!(profile.as_deref(), Some("docs"));
                assert!(supported_only);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_mcp_auth_set_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "mcp",
            "auth",
            "set",
            "linear",
            "--from-env",
            "LINEAR_API_KEY",
        ]);
        match cli.command.expect("command") {
            Command::Mcp {
                command:
                    McpCommand::Auth {
                        command: McpAuthCommand::Set(args),
                    },
            } => {
                assert_eq!(args.id, "linear");
                assert_eq!(args.from_env.as_deref(), Some("LINEAR_API_KEY"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_mcp_tool_call_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "mcp",
            "tools",
            "call",
            "mcp__github__get_me",
            "--input-json",
            r#"{"dummy":true}"#,
        ]);
        match cli.command.expect("command") {
            Command::Mcp {
                command:
                    McpCommand::Tools {
                        command: McpToolsCommand::Call(args),
                    },
            } => {
                assert_eq!(args.tool_name, "mcp__github__get_me");
                assert_eq!(args.input_json.as_deref(), Some(r#"{"dummy":true}"#));
                assert_eq!(args.input_file, None);
                assert!(!args.stdin);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_debug_level_command() {
        let cli = Cli::parse_from(["kheish-daemon", "runtime", "set-debug-level", "redacted"]);
        match cli.command.expect("command") {
            Command::Runtime {
                command: RuntimeCommand::SetDebugLevel { level, .. },
            } => assert_eq!(level, DebugLevelArg::Redacted),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_direct_setter_expected_revisions() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "set-model",
            "openai/gpt-5.4",
            "--expected-revision",
            "7",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::SetModel {
                        model,
                        expected_revision,
                    },
            } => {
                assert_eq!(model, "openai/gpt-5.4");
                assert_eq!(expected_revision, Some(7));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "set-permission-mode",
            "accept-edits",
            "--expected-revision",
            "8",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::SetPermissionMode {
                        mode,
                        expected_revision,
                    },
            } => {
                assert_eq!(mode, PermissionModeArg::AcceptEdits);
                assert_eq!(expected_revision, Some(8));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "set-debug-level",
            "full",
            "--expected-revision",
            "9",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::SetDebugLevel {
                        level,
                        expected_revision,
                    },
            } => {
                assert_eq!(level, DebugLevelArg::Full);
                assert_eq!(expected_revision, Some(9));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_system_prompt_expected_revision() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "set-system-prompt",
            "--mode",
            "append",
            "Prefer direct execution.",
            "--expected-revision",
            "7",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command: RuntimeCommand::SetSystemPrompt(args),
            } => {
                assert_eq!(args.mode, Some(SystemPromptModeArg::Append));
                assert_eq!(args.content.as_deref(), Some("Prefer direct execution."));
                assert_eq!(args.expected_revision, Some(7));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_hooks_get_command() {
        let cli = Cli::parse_from(["kheish-daemon", "runtime", "hooks", "get"]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Hooks {
                        command: RuntimeHooksCommand::Get,
                    },
            } => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_hooks_dead_letter_commands() {
        let cli = Cli::parse_from(["kheish-daemon", "runtime", "hooks", "dead-letter"]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Hooks {
                        command: RuntimeHooksCommand::DeadLetter,
                    },
            } => {}
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "hooks",
            "resolve-dead-letter",
            "hook-dlq-1",
            "--reason",
            "investigated",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Hooks {
                        command:
                            RuntimeHooksCommand::ResolveDeadLetter {
                                dead_letter_id,
                                reason,
                            },
                    },
            } => {
                assert_eq!(dead_letter_id, "hook-dlq-1");
                assert_eq!(reason, "investigated");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_tool_limits_set_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "tool-limits",
            "set",
            "--reset",
            "--expected-revision",
            "7",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::ToolLimits {
                        command: RuntimeToolLimitsCommand::Set(args),
                    },
            } => {
                assert!(args.reset);
                assert_eq!(args.expected_revision, Some(7));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_run_memory_policy_set_expected_revision() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "run-memory-policy",
            "set",
            "--reset",
            "--expected-revision",
            "7",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::RunMemoryPolicy {
                        command: RuntimeRunMemoryPolicyCommand::Set(args),
                    },
            } => {
                assert!(args.reset);
                assert_eq!(args.expected_revision, Some(7));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_hooks_set_expected_revision_and_skip_hooks() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "hooks",
            "set",
            "--reset",
            "--expected-revision",
            "7",
            "--skip-hooks",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Hooks {
                        command: RuntimeHooksCommand::Set(args),
                    },
            } => {
                assert!(args.reset);
                assert_eq!(args.expected_revision, Some(7));
                assert!(args.skip_hooks);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_rollback_skip_hooks() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "rollback",
            "--target-revision",
            "3",
            "--expected-revision",
            "7",
            "--skip-hooks",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Rollback {
                        target_revision,
                        expected_revision,
                        skip_hooks,
                    },
            } => {
                assert_eq!(target_revision, Some(3));
                assert_eq!(expected_revision, Some(7));
                assert!(skip_hooks);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_learning_policy_set_expected_revision() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "learning-policy",
            "set",
            "--reset",
            "--expected-revision",
            "7",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::LearningPolicy {
                        command: RuntimeLearningPolicyCommand::Set(args),
                    },
            } => {
                assert!(args.reset);
                assert_eq!(args.expected_revision, Some(7));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_hooks_set_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "hooks",
            "set",
            "--file",
            "hooks.json",
        ]);
        match cli.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Hooks {
                        command: RuntimeHooksCommand::Set(args),
                    },
            } => {
                assert_eq!(args.file.as_deref(), Some(Path::new("hooks.json")));
                assert!(!args.stdin);
                assert!(!args.reset);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_playbook_commands() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "playbooks",
            "validate",
            "--manifest-json",
            r#"{"playbook_id":"ops","version":"1","title":"Ops","objective":"Run ops"}"#,
        ]);
        match cli.command.expect("command") {
            Command::Playbooks {
                command: PlaybooksCommand::Validate(args),
            } => assert!(args.manifest_json.is_some()),
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "playbooks",
            "list",
            "--query",
            "ops",
            "--status",
            "active",
        ]);
        match cli.command.expect("command") {
            Command::Playbooks {
                command: PlaybooksCommand::List { query, status },
            } => {
                assert_eq!(query.as_deref(), Some("ops"));
                assert!(matches!(status, Some(PlaybookStatusArg::Active)));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_flow_commands() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "flows",
            "start",
            "--flow-id",
            "flow-1",
            "--idempotency-key",
            "idem-1",
            "--playbook-id",
            "ops",
            "--version",
            "1",
            "--digest",
            "abc123",
            "--session-id",
            "demo",
            "--request-json",
            r#"{"content":"run","source_plugin":null,"source_kind":null,"actor_id":null,"provider":null,"generation":null,"metadata":null,"reply_address":null}"#,
            "--metadata-json",
            r#"{"operator":"test"}"#,
        ]);
        match cli.command.expect("command") {
            Command::Flows {
                command: FlowsCommand::Start(args),
            } => {
                assert_eq!(args.flow_id.as_deref(), Some("flow-1"));
                assert_eq!(args.idempotency_key.as_deref(), Some("idem-1"));
                assert_eq!(args.playbook_id, "ops");
                assert_eq!(args.version, "1");
                assert_eq!(args.digest, "abc123");
                assert_eq!(args.session_id, "demo");
                assert!(args.request_json.is_some());
                assert!(args.metadata_json.is_some());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "flows",
            "list",
            "--playbook-id",
            "ops",
            "--status",
            "waiting",
        ]);
        match cli.command.expect("command") {
            Command::Flows {
                command:
                    FlowsCommand::List {
                        playbook_id,
                        session_id,
                        status,
                    },
            } => {
                assert_eq!(playbook_id.as_deref(), Some("ops"));
                assert!(session_id.is_none());
                assert!(matches!(status, Some(FlowStatusArg::Waiting)));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "flows",
            "evidence",
            "flow-1",
            "--evidence-refs-json",
            r#"[{"kind":"flow","id":"flow-1"}]"#,
        ]);
        match cli.command.expect("command") {
            Command::Flows {
                command: FlowsCommand::Evidence(args),
            } => {
                assert_eq!(args.flow_id, "flow-1");
                assert!(args.evidence_refs_json.is_some());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "flows",
            "verify-product-view",
            "flow-1",
            "--report-path",
            "reports/product-view.md",
            "--required-section",
            "Evidence",
            "--forbidden-tool",
            "web_search",
            "--output-file",
            "verdict.json",
        ]);
        match cli.command.expect("command") {
            Command::Flows {
                command: FlowsCommand::VerifyProductView(args),
            } => {
                assert_eq!(args.flow_id, "flow-1");
                assert_eq!(args.report_path, "reports/product-view.md");
                assert_eq!(args.required_section, vec!["Evidence"]);
                assert_eq!(args.forbidden_tool, vec!["web_search"]);
                assert_eq!(args.output_file.as_deref(), Some(Path::new("verdict.json")));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runtime_auth_commands() {
        let subject = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "auth",
            "revoke-subject",
            "agent:agent-1",
        ]);
        match subject.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Auth {
                        command: RuntimeAuthCommand::RevokeSubject { subject_id },
                    },
            } => assert_eq!(subject_id, "agent:agent-1"),
            other => panic!("unexpected command: {other:?}"),
        }

        let lease = Cli::parse_from(["kheish-daemon", "runtime", "auth", "lease", "lease-1"]);
        match lease.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Auth {
                        command: RuntimeAuthCommand::Lease { lease_id },
                    },
            } => assert_eq!(lease_id, "lease-1"),
            other => panic!("unexpected command: {other:?}"),
        }

        let hyphen_lease =
            Cli::parse_from(["kheish-daemon", "runtime", "auth", "lease", "-tokenish"]);
        match hyphen_lease.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Auth {
                        command: RuntimeAuthCommand::Lease { lease_id },
                    },
            } => assert_eq!(lease_id, "-tokenish"),
            other => panic!("unexpected command: {other:?}"),
        }

        let slot = Cli::parse_from([
            "kheish-daemon",
            "runtime",
            "auth",
            "revoke-slot",
            "openai.prod",
        ]);
        match slot.command.expect("command") {
            Command::Runtime {
                command:
                    RuntimeCommand::Auth {
                        command: RuntimeAuthCommand::RevokeSlot { slot_id },
                    },
            } => assert_eq!(slot_id, "openai.prod"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn build_observation_selection_rejects_empty_stream_id() {
        let error = build_observation_selection(&[], Some("mic-1"), Some("   "), None, 1, None)
            .expect_err("whitespace-only stream_id should fail");
        assert!(error.to_string().contains("--stream-id cannot be empty"));
    }

    #[test]
    fn resolve_observation_raw_asset_policy_rejects_conflicting_flags() {
        let error =
            resolve_observation_raw_asset_policy(true, Some(ObservationRawAssetPolicyArg::Always))
                .expect_err("conflicting raw asset flags should fail");
        assert!(
            error
                .to_string()
                .contains("--without-raw-assets cannot be combined with --raw-assets-policy")
        );
    }

    #[test]
    fn cli_parses_observation_materialize_raw_assets_policy() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "observations",
            "materialize",
            "demo",
            "Analyze the call.",
            "--source-id",
            "mic-1",
            "--stream-id",
            "call-1",
            "--raw-assets-policy",
            "always",
        ]);
        match cli.command.expect("command") {
            Command::Observations {
                command: ObservationsCommand::Materialize(args),
            } => {
                assert_eq!(args.source_id.as_deref(), Some("mic-1"));
                assert_eq!(args.stream_id.as_deref(), Some("call-1"));
                assert!(matches!(
                    args.raw_assets_policy,
                    Some(ObservationRawAssetPolicyArg::Always)
                ));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_observation_source_token_commands_and_audit() {
        let rotate = Cli::parse_from([
            "kheish-daemon",
            "observations",
            "sources",
            "rotate-token",
            "screen-1",
            "--upload-token",
            "next-token",
            "--grace-period-ms",
            "5000",
        ]);
        match rotate.command.expect("command") {
            Command::Observations {
                command:
                    ObservationsCommand::Sources {
                        command: ObservationSourcesCommand::RotateToken(args),
                    },
            } => {
                assert_eq!(args.source_id, "screen-1");
                assert_eq!(args.upload_token.as_deref(), Some("next-token"));
                assert_eq!(args.grace_period_ms, 5000);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let revoke = Cli::parse_from([
            "kheish-daemon",
            "observations",
            "sources",
            "revoke-token",
            "screen-1",
            "--reason",
            "lost device",
        ]);
        match revoke.command.expect("command") {
            Command::Observations {
                command:
                    ObservationsCommand::Sources {
                        command: ObservationSourcesCommand::RevokeToken(args),
                    },
            } => {
                assert_eq!(args.source_id, "screen-1");
                assert_eq!(args.reason.as_deref(), Some("lost device"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let audit = Cli::parse_from([
            "kheish-daemon",
            "observations",
            "audit",
            "--source-id",
            "screen-1",
            "--event",
            "upload_rejected",
            "--limit",
            "7",
        ]);
        match audit.command.expect("command") {
            Command::Observations {
                command: ObservationsCommand::Audit(args),
            } => {
                assert_eq!(args.source_id.as_deref(), Some("screen-1"));
                assert_eq!(args.event.as_deref(), Some("upload_rejected"));
                assert_eq!(args.limit, 7);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_derivation_observation_subject() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "derivations",
            "create",
            "--profile",
            "canonical-text",
            "--observation-id",
            "observation-7",
        ]);
        match cli.command.expect("command") {
            Command::Derivations {
                command: DerivationsCommand::Create(args),
            } => {
                assert_eq!(args.observation_id.as_deref(), Some("observation-7"));
                assert!(args.asset_id.is_none());
                assert!(args.session_id.is_none());
                assert!(args.offset.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_derivation_transcription_options() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "derivations",
            "create",
            "--profile",
            "canonical-text",
            "--asset-id",
            "asset-7",
            "--transcription-prompt",
            "project glossary",
            "--transcription-language",
            "en",
            "--transcription-timestamp-granularity",
            "word",
            "--force-refresh",
            "--retry-failed",
        ]);
        match cli.command.expect("command") {
            Command::Derivations {
                command: DerivationsCommand::Create(args),
            } => {
                assert_eq!(args.asset_id.as_deref(), Some("asset-7"));
                assert!(args.force_refresh);
                assert!(args.retry_failed);
                let options = args
                    .transcription_options()
                    .expect("transcription options should be present");
                assert_eq!(options.prompt.as_deref(), Some("project glossary"));
                assert_eq!(options.language.as_deref(), Some("en"));
                assert_eq!(options.timestamp_granularities, ["word".to_string()]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_bulk_approval_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "approvals",
            "allow-all",
            "--session-id",
            "demo",
            "--justification",
            "approved",
            "--idempotency-key",
            "approval-batch-1",
        ]);
        match cli.command.expect("command") {
            Command::Approvals {
                command: ApprovalsCommand::AllowAll(args),
            } => {
                assert_eq!(args.session_id.as_deref(), Some("demo"));
                assert_eq!(args.justification.as_deref(), Some("approved"));
                assert_eq!(args.idempotency_key.as_deref(), Some("approval-batch-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_question_interactive_answer_and_cancel_commands() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "questions",
            "answer",
            "question-1",
            "--session-id",
            "session-1",
            "--interactive",
            "--wait",
        ]);
        match cli.command.expect("command") {
            Command::Questions {
                command: QuestionsCommand::Answer(args),
            } => {
                assert_eq!(args.request_id, "question-1");
                assert_eq!(args.session_id.as_deref(), Some("session-1"));
                assert!(args.interactive);
                assert!(args.wait);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "questions",
            "cancel",
            "question-1",
            "--run-id",
            "run-1",
        ]);
        match cli.command.expect("command") {
            Command::Questions {
                command: QuestionsCommand::Cancel(args),
            } => {
                assert_eq!(args.request_id, "question-1");
                assert_eq!(args.run_id.as_deref(), Some("run-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runs_debug_artifact_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runs",
            "debug-artifact",
            "run-7",
            "turn-0001-attempt-0001-provider-request",
        ]);
        match cli.command.expect("command") {
            Command::Runs {
                command:
                    RunsCommand::DebugArtifact {
                        run_id,
                        artifact_id,
                    },
            } => {
                assert_eq!(run_id, "run-7");
                assert_eq!(artifact_id, "turn-0001-attempt-0001-provider-request");
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runs_external_actions_command() {
        let cli = Cli::parse_from(["kheish-daemon", "runs", "external-actions", "run-7"]);
        match cli.command.expect("command") {
            Command::Runs {
                command: RunsCommand::ExternalActions { run_id },
            } => assert_eq!(run_id, "run-7"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runs_prune_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runs",
            "prune",
            "--older-than-ms",
            "1000",
            "--session-id",
            "session-1",
            "--limit",
            "25",
            "--dry-run",
        ]);
        match cli.command.expect("command") {
            Command::Runs {
                command: RunsCommand::Prune(args),
            } => {
                assert_eq!(args.older_than_ms, 1000);
                assert_eq!(args.session_id.as_deref(), Some("session-1"));
                assert_eq!(args.limit, Some(25));
                assert!(args.dry_run);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_deliveries_commands() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "deliveries",
            "list",
            "--run-id",
            "run-1",
            "--plugin",
            "http",
            "--status",
            "dead-lettered",
            "--limit",
            "10",
        ]);
        match cli.command.expect("command") {
            Command::Deliveries {
                command: DeliveriesCommand::List(args),
            } => {
                assert_eq!(args.run_id.as_deref(), Some("run-1"));
                assert_eq!(args.plugin.as_deref(), Some("http"));
                assert!(matches!(args.status, Some(DeliveryStatusArg::DeadLettered)));
                assert_eq!(args.pagination.limit, Some(10));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "deliveries",
            "replay",
            "delivery-7",
            "--force",
        ]);
        match cli.command.expect("command") {
            Command::Deliveries {
                command: DeliveriesCommand::Replay { delivery_id, force },
            } => {
                assert_eq!(delivery_id, "delivery-7");
                assert!(force);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "deliveries",
            "resolve",
            "delivery-8",
            "--reason",
            "destination retired",
        ]);
        match cli.command.expect("command") {
            Command::Deliveries {
                command:
                    DeliveriesCommand::Resolve {
                        delivery_id,
                        reason,
                    },
            } => {
                assert_eq!(delivery_id, "delivery-8");
                assert_eq!(reason, "destination retired");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "deliveries",
            "replay-bulk",
            "--session-id",
            "session-1",
            "--plugin",
            "external",
            "--limit",
            "25",
            "--dry-run",
            "--include-resolved",
        ]);
        match cli.command.expect("command") {
            Command::Deliveries {
                command: DeliveriesCommand::ReplayBulk(args),
            } => {
                assert_eq!(args.session_id.as_deref(), Some("session-1"));
                assert_eq!(args.plugin.as_deref(), Some("external"));
                assert_eq!(args.limit, Some(25));
                assert!(args.dry_run);
                assert!(args.include_resolved);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "deliveries",
            "reset-backpressure",
            "--target",
            "external:address_sha256:abc123",
            "--plugin",
            "external",
            "--dry-run",
        ]);
        match cli.command.expect("command") {
            Command::Deliveries {
                command: DeliveriesCommand::ResetBackpressure(args),
            } => {
                assert_eq!(
                    args.target.as_deref(),
                    Some("external:address_sha256:abc123")
                );
                assert_eq!(args.plugin.as_deref(), Some("external"));
                assert!(args.dry_run);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_runs_wait_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "runs",
            "wait",
            "run-7",
            "--poll-interval-ms",
            "750",
        ]);
        match cli.command.expect("command") {
            Command::Runs {
                command: RunsCommand::Wait(args),
            } => {
                assert_eq!(args.run_id, "run-7");
                assert_eq!(args.poll_interval_ms, 750);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_schedules_list_command() {
        let cli = Cli::parse_from(["kheish-daemon", "schedules", "list", "--session-id", "demo"]);
        match cli.command.expect("command") {
            Command::Schedules {
                command:
                    SchedulesCommand::List {
                        session_id,
                        pagination,
                    },
            } => {
                assert_eq!(session_id.as_deref(), Some("demo"));
                assert!(!pagination.wants_page());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_list_limit_preserves_legacy_array_shape_without_page_or_cursor() {
        let cli = Cli::parse_from(["kheish-daemon", "sessions", "list", "--limit", "2"]);
        match cli.command.expect("command") {
            Command::Sessions {
                command: SessionsCommand::List { pagination },
            } => {
                assert_eq!(pagination.limit, Some(2));
                assert!(!pagination.wants_page());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from(["kheish-daemon", "sessions", "list", "--cursor", "abc"]);
        match cli.command.expect("command") {
            Command::Sessions {
                command: SessionsCommand::List { pagination },
            } => {
                assert!(pagination.wants_page());
                let mut params = Vec::new();
                pagination.append_query_params(&mut params);
                assert!(params.iter().any(|param| param == "page=true"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_events_stream_filters_and_cursor() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "events",
            "stream",
            "--run-id",
            "run-7",
            "--cursor",
            "42",
        ]);
        match cli.command.expect("command") {
            Command::Events {
                command:
                    EventsCommand::Stream {
                        session_id,
                        run_id,
                        cursor,
                    },
            } => {
                assert_eq!(session_id, None);
                assert_eq!(run_id.as_deref(), Some("run-7"));
                assert_eq!(cursor, Some(42));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[tokio::test]
    async fn build_schedule_create_request_supports_interval_fields() -> Result<()> {
        let request = build_schedule_create_request(
            &CreateScheduleArgs {
                name: "heartbeat".to_string(),
                session_id: "demo".to_string(),
                content: Some("Reply exactly TICK".to_string()),
                content_file: None,
                stdin: false,
                at: None,
                every_seconds: Some(30),
                cron: None,
                timezone: None,
                overlap_policy: Some(ScheduleOverlapPolicyArg::Skip),
                misfire_policy: Some(ScheduleMisfirePolicyArg::CoalesceOnce),
                max_executions: Some(2),
                provider: Some("openai".to_string()),
                generation: GenerationArgs {
                    model: Some("gpt-5.4".to_string()),
                    ..GenerationArgs::default()
                },
            },
            &BTreeSet::from(["openai".to_string()]),
        )
        .await?;
        assert_eq!(request.name, "heartbeat");
        assert_eq!(request.target_session_id, "demo");
        assert!(matches!(
            request.cadence,
            ScheduleCadence::Interval { every_seconds: 30 }
        ));
        assert_eq!(request.max_executions, Some(2));
        let submit_request = request.request.as_ref().expect("schedule request");
        assert_eq!(submit_request.provider.as_deref(), Some("openai"));
        assert_eq!(
            submit_request
                .generation
                .as_ref()
                .and_then(|g| g.model.as_deref()),
            Some("gpt-5.4")
        );
        Ok(())
    }

    #[test]
    fn cli_parses_session_input_wait_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "sessions",
            "input",
            "demo",
            "do it",
            "--wait",
        ]);
        match cli.command.expect("command") {
            Command::Sessions {
                command: SessionsCommand::Input(args),
            } => {
                assert_eq!(args.session_id, "demo");
                assert_eq!(args.content.as_deref(), Some("do it"));
                assert!(args.wait);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_asset_delete_and_gc_commands() {
        let delete = Cli::parse_from(["kheish-daemon", "assets", "delete", "asset-1", "--dry-run"]);
        match delete.command.expect("command") {
            Command::Assets {
                command: AssetsCommand::Delete { asset_id, dry_run },
            } => {
                assert_eq!(asset_id, "asset-1");
                assert!(dry_run);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let gc = Cli::parse_from(["kheish-daemon", "assets", "gc", "--execute"]);
        match gc.command.expect("command") {
            Command::Assets {
                command: AssetsCommand::Gc { execute },
            } => assert!(execute),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_session_input_provider_override() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "sessions",
            "input",
            "demo",
            "do it",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4",
        ]);
        match cli.command.expect("command") {
            Command::Sessions {
                command: SessionsCommand::Input(args),
            } => {
                assert_eq!(args.session_id, "demo");
                assert_eq!(args.provider.as_deref(), Some("openai"));
                assert_eq!(args.generation.model.as_deref(), Some("gpt-5.4"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_secret_set_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "secrets",
            "set",
            "openrouter.primary",
            "--provider",
            "openai",
            "--from-env",
            "OPENROUTER_API_KEY",
        ]);
        match cli.command.expect("command") {
            Command::Secrets {
                command: SecretsCommand::Set(args),
            } => {
                assert_eq!(args.secret_ref, "openrouter.primary");
                assert_eq!(args.provider, SecretProviderKind::Openai);
                assert_eq!(args.from_env.as_deref(), Some("OPENROUTER_API_KEY"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_secret_generate_command() {
        let cli = Cli::parse_from(["kheish-daemon", "secrets", "generate"]);
        match cli.command.expect("command") {
            Command::Secrets {
                command: SecretsCommand::Generate,
            } => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_personas_import_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "personas",
            "import",
            "reviewer.md",
            "--persona-id",
            "reviewer.persona",
            "--display-name",
            "Reviewer",
        ]);
        match cli.command.expect("command") {
            Command::Personas {
                command: PersonasCommand::Import(args),
            } => {
                assert_eq!(args.path, PathBuf::from("reviewer.md"));
                assert_eq!(args.persona_id.as_deref(), Some("reviewer.persona"));
                assert_eq!(args.display_name.as_deref(), Some("Reviewer"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn derive_persona_display_name_prefers_first_markdown_heading() -> Result<()> {
        let display_name = derive_persona_display_name_from_markdown(
            "# Reviewer Persona\n\nReply as a reviewer.",
            Path::new("reviewer.md"),
        )?;
        assert_eq!(display_name, "Reviewer Persona");
        Ok(())
    }

    #[test]
    fn derive_persona_display_name_falls_back_to_file_stem() -> Result<()> {
        let display_name = derive_persona_display_name_from_markdown(
            "Reply as a reviewer.",
            Path::new("reviewer-persona.md"),
        )?;
        assert_eq!(display_name, "reviewer-persona");
        Ok(())
    }

    #[test]
    fn ensure_markdown_persona_path_rejects_non_markdown_files() {
        let error = ensure_markdown_persona_path(Path::new("reviewer.txt"))
            .expect_err("non-markdown persona import path should fail");
        assert!(error.to_string().contains(".md or .markdown"));
    }

    #[tokio::test]
    async fn read_persona_markdown_import_reads_soul_and_heading() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("reviewer.md");
        tokio::fs::write(
            &path,
            "# Reviewer Persona\n\nReply as Reviewer Persona imported from Markdown.\n",
        )
        .await?;

        let imported = read_persona_markdown_import(&path).await?;
        assert_eq!(imported.display_name, "Reviewer Persona");
        assert!(imported.soul.contains("imported from Markdown"));
        Ok(())
    }

    #[test]
    fn read_secret_value_requires_exactly_one_source() {
        let error = read_secret_value(
            Some("inline".to_string()),
            Some("OPENAI_API_KEY"),
            None,
            false,
            "--value",
        )
        .expect_err("multiple sources should fail");
        assert!(error.to_string().contains("provide exactly one"));
    }

    #[test]
    fn read_secret_value_rejects_empty_file_contents() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("secret.txt");
        std::fs::write(&path, "\n").expect("write secret file");
        let error = read_secret_value(None, None, Some(&path), false, "--value")
            .expect_err("empty file should fail");
        assert!(
            error
                .to_string()
                .contains("--from-file resolved to an empty secret")
        );
    }

    #[tokio::test]
    async fn local_secrets_command_round_trips_global_store() -> Result<()> {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir()?;
        let printer = Printer {
            format: OutputFormat::Json,
        };
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }

        run_local_secrets_command(
            &printer,
            SecretsCommand::Set(SecretSetArgs {
                secret_ref: "openai.prod".to_string(),
                provider: SecretProviderKind::Openai,
                value: Some("sk-test".to_string()),
                from_env: None,
                from_file: None,
                stdin: false,
                organization: None,
                project: None,
                store: SecretStoreArgs {
                    state_root: Some(temp.path().join("state")),
                    offline: false,
                },
            }),
        )
        .await?;

        let manager = AuthManager::new(global_auth_store_path(&temp.path().join("state")))?;
        let status = manager
            .status(&AuthSlotId::new("openai.prod"))
            .await?
            .ok_or_else(|| anyhow!("missing persisted auth slot"))?;
        assert_eq!(status.provider, AuthProvider::OpenAi);

        run_local_secrets_command(
            &printer,
            SecretsCommand::Delete(SecretDeleteArgs {
                secret_ref: "openai.prod".to_string(),
                store: SecretStoreArgs {
                    state_root: Some(temp.path().join("state")),
                    offline: false,
                },
            }),
        )
        .await?;

        let manager = AuthManager::new(global_auth_store_path(&temp.path().join("state")))?;
        assert!(!manager.has_slot(&AuthSlotId::new("openai.prod")).await);
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        Ok(())
    }

    #[tokio::test]
    async fn local_secrets_command_requires_master_key_for_mutation() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let printer = Printer {
            format: OutputFormat::Json,
        };
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        let error = run_local_secrets_command(
            &printer,
            SecretsCommand::Set(SecretSetArgs {
                secret_ref: "openai.prod".to_string(),
                provider: SecretProviderKind::Openai,
                value: Some("sk-test".to_string()),
                from_env: None,
                from_file: None,
                stdin: false,
                organization: None,
                project: None,
                store: SecretStoreArgs {
                    state_root: Some(temp.path().join("state")),
                    offline: false,
                },
            }),
        )
        .await
        .expect_err("mutating secrets without a master key should fail");
        assert!(error.to_string().contains(AUTH_STORE_MASTER_KEY_ENV));
    }

    #[tokio::test]
    async fn local_secrets_command_rejects_invalid_master_key_for_mutation() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let printer = Printer {
            format: OutputFormat::Json,
        };
        unsafe {
            std::env::set_var(AUTH_STORE_MASTER_KEY_ENV, "1234567890123456789012345678901");
        }
        let error = run_local_secrets_command(
            &printer,
            SecretsCommand::Set(SecretSetArgs {
                secret_ref: "openai.prod".to_string(),
                provider: SecretProviderKind::Openai,
                value: Some("sk-test".to_string()),
                from_env: None,
                from_file: None,
                stdin: false,
                organization: None,
                project: None,
                store: SecretStoreArgs {
                    state_root: Some(temp.path().join("state")),
                    offline: false,
                },
            }),
        )
        .await
        .expect_err("mutating secrets with an invalid master key should fail");
        assert!(
            error
                .to_string()
                .contains("must be exactly 32 raw bytes or base64-encoded 32 bytes")
        );
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn local_secrets_command_rejects_non_generic_connector_slots() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let printer = Printer {
            format: OutputFormat::Json,
        };
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let error = run_local_secrets_command(
            &printer,
            SecretsCommand::Set(SecretSetArgs {
                secret_ref: "connectors.telegram.ops-bot.bot_token".to_string(),
                provider: SecretProviderKind::Openai,
                value: Some("sk-test".to_string()),
                from_env: None,
                from_file: None,
                stdin: false,
                organization: None,
                project: None,
                store: SecretStoreArgs {
                    state_root: Some(temp.path().join("state")),
                    offline: true,
                },
            }),
        )
        .await
        .expect_err("connector-backed secret slots should reject non-generic records");
        assert!(error.to_string().contains(
            "connector and MCP secret slots must use generic opaque or MCP OAuth records"
        ));
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn run_secrets_command_requires_explicit_offline_mode_when_daemon_is_unreachable() {
        let client = DaemonHttpClient::new("http://127.0.0.1:1".to_string(), None);
        let printer = Printer {
            format: OutputFormat::Json,
        };
        let error = run_secrets_command(
            &client,
            &printer,
            SecretsCommand::List(SecretStoreArgs {
                state_root: None,
                offline: false,
            }),
        )
        .await
        .expect_err("unreachable daemon should require explicit offline mode");
        assert!(error.to_string().contains("--offline"));
    }

    #[tokio::test]
    async fn run_secrets_command_generate_bypasses_daemon_and_store_routing() -> Result<()> {
        let client = DaemonHttpClient::new("http://127.0.0.1:1".to_string(), None);
        let printer = Printer {
            format: OutputFormat::Json,
        };
        run_secrets_command(&client, &printer, SecretsCommand::Generate).await?;
        let generated = generate_auth_store_master_key_base64();
        assert_eq!(STANDARD.decode(generated)?.len(), 32);
        Ok(())
    }

    #[tokio::test]
    async fn local_secrets_command_requires_explicit_state_root() {
        let printer = Printer {
            format: OutputFormat::Json,
        };
        let error = run_local_secrets_command(
            &printer,
            SecretsCommand::List(SecretStoreArgs {
                state_root: None,
                offline: true,
            }),
        )
        .await
        .expect_err("offline secret operations should require an explicit state root");
        assert!(error.to_string().contains("--state-root"));
    }

    #[test]
    fn cli_parses_session_set_route_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "sessions",
            "set-route",
            "demo",
            "--provider",
            "openrouter",
            "--model",
            "anthropic/claude-sonnet-4",
        ]);
        match cli.command.expect("command") {
            Command::Sessions {
                command: SessionsCommand::SetRoute(args),
            } => {
                assert_eq!(args.session_id, "demo");
                assert_eq!(args.provider.as_deref(), Some("openrouter"));
                assert_eq!(
                    args.generation.model.as_deref(),
                    Some("anthropic/claude-sonnet-4")
                );
                assert!(!args.clear);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[tokio::test]
    async fn build_session_route_policy_supports_clear_and_model_overrides() -> Result<()> {
        let cleared = build_session_route_policy(
            &SessionSetRouteArgs {
                session_id: "demo".to_string(),
                provider: None,
                clear: true,
                generation: GenerationArgs::default(),
            },
            &BTreeSet::from(["openai".to_string()]),
        )
        .await?;
        assert!(cleared.is_none());

        let built = build_session_route_policy(
            &SessionSetRouteArgs {
                session_id: "demo".to_string(),
                provider: Some("openai".to_string()),
                clear: false,
                generation: GenerationArgs {
                    model: Some("gpt-5.4".to_string()),
                    ..GenerationArgs::default()
                },
            },
            &BTreeSet::from(["openai".to_string()]),
        )
        .await?;
        let built = built.expect("route policy");
        assert_eq!(built.provider.as_deref(), Some("openai"));
        assert_eq!(
            built
                .generation
                .as_ref()
                .and_then(|generation| generation.model.as_deref()),
            Some("gpt-5.4")
        );
        Ok(())
    }

    #[test]
    fn cli_parses_sidechain_provider_override() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "agents",
            "spawn-sidechain",
            "agent-1",
            "--session-id",
            "child",
            "--thread-id",
            "thread-1",
            "--provider",
            "anthropic",
            "--spawn-request-id",
            "spawn-cli-1",
            "--model",
            "claude-opus-4-6",
        ]);
        match cli.command.expect("command") {
            Command::Agents {
                command: AgentsCommand::SpawnSidechain(args),
            } => {
                assert_eq!(args.parent_agent_id, "agent-1");
                assert_eq!(args.provider.as_deref(), Some("anthropic"));
                assert_eq!(args.spawn_request_id.as_deref(), Some("spawn-cli-1"));
                assert_eq!(args.generation.model.as_deref(), Some("claude-opus-4-6"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_sidechain_explain_command() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "agents",
            "explain-sidechain",
            "agent-1",
            "--session-id",
            "child",
            "--spawn-request-id",
            "spawn-cli-explain-1",
        ]);
        match cli.command.expect("command") {
            Command::Agents {
                command: AgentsCommand::ExplainSidechain(args),
            } => {
                assert_eq!(args.parent_agent_id, "agent-1");
                assert_eq!(args.session_id.as_deref(), Some("child"));
                assert_eq!(
                    args.spawn_request_id.as_deref(),
                    Some("spawn-cli-explain-1")
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_agent_summaries_command() {
        let cli = Cli::parse_from(["kheish-daemon", "agents", "summaries"]);
        match cli.command.expect("command") {
            Command::Agents {
                command:
                    AgentsCommand::Summaries {
                        root_agent_id,
                        session_id,
                        status,
                        has_runtime,
                        pagination,
                    },
            } => {
                assert!(root_agent_id.is_none());
                assert!(session_id.is_none());
                assert!(status.is_none());
                assert!(has_runtime.is_none());
                assert!(!pagination.wants_page());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_agent_summary_filters() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "agents",
            "summaries",
            "--root",
            "agent-1",
            "--session-id",
            "demo",
            "--status",
            "running",
            "--has-runtime",
            "true",
            "--page",
            "--limit",
            "10",
        ]);
        match cli.command.expect("command") {
            Command::Agents {
                command:
                    AgentsCommand::Summaries {
                        root_agent_id,
                        session_id,
                        status,
                        has_runtime,
                        pagination,
                    },
            } => {
                assert_eq!(root_agent_id.as_deref(), Some("agent-1"));
                assert_eq!(session_id.as_deref(), Some("demo"));
                assert_eq!(status, Some(AgentStatusArg::Running));
                assert_eq!(has_runtime, Some(true));
                assert!(pagination.wants_page());
                assert_eq!(pagination.limit, Some(10));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_agent_audit_command() {
        let cli = Cli::parse_from(["kheish-daemon", "agents", "audit", "--agent-id", "agent-1"]);
        match cli.command.expect("command") {
            Command::Agents {
                command: AgentsCommand::Audit { agent_id },
            } => assert_eq!(agent_id.as_deref(), Some("agent-1")),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_mailbox_id_ttl_ack_and_dlq_commands() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "mailboxes",
            "post",
            "--message-id",
            "mailbox-1",
            "--from-agent-id",
            "agent-1",
            "--to-agent-id",
            "agent-2",
            "--subject",
            "handoff",
            "--ttl-ms",
            "500",
        ]);
        match cli.command.expect("command") {
            Command::Mailboxes {
                command: MailboxesCommand::Post(args),
            } => {
                assert_eq!(args.message_id.as_deref(), Some("mailbox-1"));
                assert_eq!(args.ttl_ms, Some(500));
                assert_eq!(args.from_agent_id, "agent-1");
                assert_eq!(args.to_agent_id, "agent-2");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from([
            "kheish-daemon",
            "agents",
            "ack-mailbox",
            "agent-2",
            "mailbox-1",
        ]);
        match cli.command.expect("command") {
            Command::Agents {
                command:
                    AgentsCommand::AckMailbox {
                        agent_id,
                        message_id,
                    },
            } => {
                assert_eq!(agent_id, "agent-2");
                assert_eq!(message_id, "mailbox-1");
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::parse_from(["kheish-daemon", "agents", "mailbox-dead-letters", "agent-2"]);
        match cli.command.expect("command") {
            Command::Agents {
                command: AgentsCommand::MailboxDeadLetters { agent_id },
            } => assert_eq!(agent_id, "agent-2"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[tokio::test]
    async fn generation_args_build_specific_tool_choice() {
        let args = GenerationArgs {
            tool_choice: Some(ToolChoiceArg::Specific),
            tool_name: Some("write_file".to_string()),
            max_output_tokens: Some(128),
            ..GenerationArgs::default()
        };
        let config = args.build().await.expect("generation config").expect("set");
        assert_eq!(
            config.tool_choice,
            ToolChoice::Specific {
                name: "write_file".to_string()
            }
        );
        assert_eq!(config.max_output_tokens, Some(128));
    }

    #[tokio::test]
    async fn generation_args_build_reasoning_overrides() {
        let args = GenerationArgs {
            reasoning_effort: Some(ReasoningEffortArg::Xhigh),
            reasoning_summary: Some(ReasoningSummaryArg::Auto),
            reasoning_budget_tokens: Some(32_768),
            ..GenerationArgs::default()
        };

        let config = args.build().await.expect("generation config").expect("set");
        let reasoning = config.reasoning.expect("reasoning config");
        assert_eq!(
            reasoning.effort,
            Some(kheish_runtime::ReasoningEffort::Xhigh)
        );
        assert_eq!(
            reasoning.summary,
            Some(kheish_runtime::ReasoningSummary::Auto)
        );
        assert_eq!(reasoning.budget_tokens, Some(32_768));
    }

    #[test]
    fn sessions_end_command_parses_reason() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "sessions",
            "end",
            "session-a",
            "--reason",
            "operator requested shutdown",
        ]);
        match cli.command.expect("command") {
            Command::Sessions {
                command: SessionsCommand::End { session_id, reason },
            } => {
                assert_eq!(session_id, "session-a");
                assert_eq!(reason.as_deref(), Some("operator requested shutdown"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parse_model_selector_extracts_explicit_route_prefix() {
        let parsed = parse_model_selector(
            "openrouter/anthropic/claude-sonnet-4",
            &BTreeSet::from(["openai".to_string(), "openrouter".to_string()]),
        )
        .expect("selector");
        assert_eq!(parsed.provider.as_deref(), Some("openrouter"));
        assert_eq!(parsed.model, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn parse_model_selector_leaves_unknown_prefix_as_model_text() {
        let parsed = parse_model_selector(
            "anthropic/claude-sonnet-4",
            &BTreeSet::from(["openrouter".to_string()]),
        )
        .expect("selector");
        assert_eq!(parsed.provider, None);
        assert_eq!(parsed.model, "anthropic/claude-sonnet-4");
    }

    #[test]
    fn parse_model_selector_rejects_empty_value() {
        let error =
            parse_model_selector("   ", &BTreeSet::new()).expect_err("empty selector should fail");
        assert!(error.to_string().contains("model selector cannot be empty"));
    }

    #[test]
    fn parse_model_selector_rejects_route_without_model_suffix() {
        let error = parse_model_selector(
            "openrouter/   ",
            &BTreeSet::from(["openrouter".to_string()]),
        )
        .expect_err("selector without model suffix should fail");
        assert!(
            error
                .to_string()
                .contains("must include a model after the route id")
        );
    }

    #[tokio::test]
    async fn build_session_route_policy_rejects_conflicting_provider_and_selector() {
        let error = build_session_route_policy(
            &SessionSetRouteArgs {
                session_id: "demo".to_string(),
                provider: Some("openai".to_string()),
                clear: false,
                generation: GenerationArgs {
                    model: Some("openrouter/openai/gpt-5.4-mini".to_string()),
                    ..GenerationArgs::default()
                },
            },
            &BTreeSet::from(["openai".to_string(), "openrouter".to_string()]),
        )
        .await
        .expect_err("conflicting route selectors should fail");
        assert!(
            error
                .to_string()
                .contains("--model route `openrouter` conflicts with selected route `openai`")
        );
    }

    #[tokio::test]
    async fn build_session_route_policy_normalizes_selector_prefixes() -> Result<()> {
        let built = build_session_route_policy(
            &SessionSetRouteArgs {
                session_id: "demo".to_string(),
                provider: None,
                clear: false,
                generation: GenerationArgs {
                    model: Some("openrouter/anthropic/claude-sonnet-4".to_string()),
                    fallback_model: Some("openrouter/anthropic/claude-haiku-4".to_string()),
                    ..GenerationArgs::default()
                },
            },
            &BTreeSet::from(["openrouter".to_string()]),
        )
        .await?
        .expect("route policy");
        assert_eq!(built.provider.as_deref(), Some("openrouter"));
        let generation = built.generation.expect("generation");
        assert_eq!(
            generation.model.as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(
            generation.fallback_model.as_deref(),
            Some("anthropic/claude-haiku-4")
        );
        Ok(())
    }

    #[tokio::test]
    async fn build_schedule_create_request_normalizes_selector_prefixes() -> Result<()> {
        let request = build_schedule_create_request(
            &CreateScheduleArgs {
                name: "nightly".to_string(),
                session_id: "demo".to_string(),
                content: Some("run".to_string()),
                content_file: None,
                stdin: false,
                at: None,
                every_seconds: Some(60),
                cron: None,
                timezone: None,
                overlap_policy: None,
                misfire_policy: None,
                max_executions: None,
                provider: None,
                generation: GenerationArgs {
                    model: Some("openrouter/openai/gpt-5.4-mini".to_string()),
                    ..GenerationArgs::default()
                },
            },
            &BTreeSet::from(["openrouter".to_string()]),
        )
        .await?;
        let submit_request = request.request.as_ref().expect("schedule request");
        assert_eq!(submit_request.provider.as_deref(), Some("openrouter"));
        assert_eq!(
            submit_request
                .generation
                .as_ref()
                .and_then(|generation| generation.model.as_deref()),
            Some("openai/gpt-5.4-mini")
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_from_file_builds_named_routes() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let routes_path = temp.path().join("routes.toml");
        std::fs::write(
            &routes_path,
            r#"version = 1
default_route = "openrouter"

[routes.openrouter]
driver = "openai"
default_model = "openai/gpt-5.4-mini"
model_support = "any"
api_key = "test-key"
base_url = "http://127.0.0.1:1/v1/responses"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
api_key = "test-key"
base_url = "http://127.0.0.1:1/v1/responses"
"#,
        )?;
        let mut args = test_serve_args(&temp);
        args.routes_file = Some(routes_path);
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;
        let routes = resolve_route_inventory(&args, manager).await?;
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].route_id(), "openrouter");
        assert_eq!(routes[0].provider_name(), "openai");
        assert_eq!(routes[0].model_name(), "openai/gpt-5.4-mini");
        assert_eq!(routes[1].route_id(), "openai");
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_from_file_honors_valid_default_route_override() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let routes_path = temp.path().join("routes.toml");
        std::fs::write(
            &routes_path,
            r#"version = 1
default_route = "openrouter"

[routes.openrouter]
driver = "openai"
default_model = "openai/gpt-5.4-mini"
model_support = "any"
api_key = "test-key"
base_url = "http://127.0.0.1:1/v1/responses"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
api_key = "test-key"
base_url = "http://127.0.0.1:1/v1/responses"
"#,
        )?;
        let mut args = test_serve_args(&temp);
        args.routes_file = Some(routes_path);
        args.default_route = Some("openai".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;

        let routes = resolve_route_inventory(&args, manager).await?;

        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].route_id(), "openai");
        assert_eq!(routes[1].route_id(), "openrouter");
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_from_file_rejects_missing_file_default_even_with_override()
    -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let routes_path = temp.path().join("routes.toml");
        std::fs::write(
            &routes_path,
            r#"version = 1

[routes.openrouter]
driver = "openai"
default_model = "openai/gpt-5.4-mini"
model_support = "any"
api_key = "test-key"
base_url = "http://127.0.0.1:1/v1/responses"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
api_key = "test-key"
base_url = "http://127.0.0.1:1/v1/responses"
"#,
        )?;
        let mut args = test_serve_args(&temp);
        args.routes_file = Some(routes_path);
        args.default_route = Some("openai".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;

        let error = resolve_route_inventory(&args, manager)
            .await
            .expect_err("multi-route files should still declare default_route explicitly");
        assert!(
            error
                .to_string()
                .contains("must set default_route when multiple routes are configured")
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_from_file_supports_multiple_route_drivers() -> Result<()> {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::TempDir::new()?;
        let routes_path = temp.path().join("routes.toml");
        std::fs::write(
            &routes_path,
            r#"version = 1
default_route = "anthropic"

[routes.anthropic]
driver = "anthropic"
default_model = "claude-opus-4-6"
auth_ref = "anthropic.prod"
anthropic_version = "2023-06-01"
anthropic_beta_headers = ["tools-2024-04-04"]

[routes.google]
driver = "google"
default_model = "gemini-2.5-flash"
auth_ref = "google.prod"

[routes.xai]
driver = "xai"
default_model = "grok-4.20-0309-reasoning"
auth_ref = "xai.prod"
"#,
        )?;
        let mut args = test_serve_args(&temp);
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;
        manager
            .store_anthropic_api_key(AuthSlotId::new("anthropic.prod"), "test-anthropic-key")
            .await?;
        manager
            .store_google_api_key(AuthSlotId::new("google.prod"), "test-google-key")
            .await?;
        manager
            .store_xai_api_key(AuthSlotId::new("xai.prod"), "test-xai-key")
            .await?;
        args.routes_file = Some(routes_path);
        let routes = resolve_route_inventory(&args, manager).await?;
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[0].route_id(), "anthropic");
        assert_eq!(routes[0].provider_name(), "anthropic");
        assert_eq!(routes[0].model_name(), "claude-opus-4-6");
        assert_eq!(routes[0].auth_ref(), Some("anthropic.prod"));
        assert_eq!(routes[1].route_id(), "google");
        assert_eq!(routes[1].provider_name(), "google");
        assert_eq!(routes[1].model_name(), "gemini-2.5-flash");
        assert_eq!(routes[1].auth_ref(), Some("google.prod"));
        assert_eq!(routes[2].route_id(), "xai");
        assert_eq!(routes[2].provider_name(), "xai");
        assert_eq!(routes[2].model_name(), "grok-4.20-0309-reasoning");
        assert_eq!(routes[2].auth_ref(), Some("xai.prod"));
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_from_file_supports_auth_refs() -> Result<()> {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::TempDir::new()?;
        let routes_path = temp.path().join("routes.toml");
        std::fs::write(
            &routes_path,
            r#"version = 1
default_route = "openrouter"

[routes.openrouter]
driver = "openai"
default_model = "openai/gpt-5.4-mini"
model_support = "any"
auth_ref = "openrouter.primary"
base_url = "http://127.0.0.1:1/v1/responses"
"#,
        )?;
        let mut args = test_serve_args(&temp);
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;
        manager
            .store_openai_api_key(
                AuthSlotId::new("openrouter.primary"),
                "test-key",
                None,
                None,
            )
            .await?;
        args.routes_file = Some(routes_path);
        let routes = resolve_route_inventory(&args, manager).await?;
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].auth_ref(), Some("openrouter.primary"));
        let ModelRouteConfig::OpenAi(config) = routes[0].route_config() else {
            panic!("expected openai route");
        };
        assert!(config.api_key.is_none());
        assert!(config.request_auth_provider.is_some());
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_from_file_rejects_incompatible_auth_refs() -> Result<()> {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::TempDir::new()?;
        let routes_path = temp.path().join("routes.toml");
        std::fs::write(
            &routes_path,
            r#"version = 1
default_route = "google"

[routes.google]
driver = "google"
default_model = "gemini-2.5-flash"
auth_ref = "openrouter.primary"
"#,
        )?;
        let mut args = test_serve_args(&temp);
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;
        manager
            .store_openai_api_key(
                AuthSlotId::new("openrouter.primary"),
                "test-key",
                None,
                None,
            )
            .await?;
        args.routes_file = Some(routes_path);
        let error = resolve_route_inventory(&args, manager)
            .await
            .expect_err("mismatched auth_ref provider should fail");
        assert!(
            error
                .to_string()
                .contains("auth_ref `openrouter.primary` targets provider `openai`")
        );
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_without_routes_file_builds_google_primary_route() -> Result<()>
    {
        let temp = tempfile::TempDir::new()?;
        let args = ServeArgs {
            provider: ProviderKind::Google,
            google_api_key: Some("google-key".to_string()),
            google_base_url: Some("https://google.example/v1beta".to_string()),
            openai_auth_source: Some(OpenAiAuthSourceArg::ApiKey),
            anthropic_auth_source: Some(AnthropicAuthSourceArg::ApiKey),
            ..test_serve_args(&temp)
        };
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;

        let routes = resolve_route_inventory(&args, manager).await?;

        let ModelRouteConfig::Google(config) = routes[0].route_config() else {
            panic!(
                "expected google route, found {:?}",
                routes[0].route_config()
            );
        };
        assert_eq!(routes[0].route_id(), "google");
        assert_eq!(config.base_url, "https://google.example/v1beta");
        assert!(config.api_key.is_none());
        let auth_material = config
            .request_auth_provider
            .as_ref()
            .expect("google route should use dynamic request auth")
            .resolve()
            .await?;
        assert_eq!(
            auth_material
                .headers
                .get("x-goog-api-key")
                .map(String::as_str),
            Some("google-key")
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_without_routes_file_builds_anthropic_primary_route()
    -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let args = ServeArgs {
            provider: ProviderKind::Anthropic,
            api_key: Some("anthropic-key".to_string()),
            openai_auth_source: Some(OpenAiAuthSourceArg::ApiKey),
            anthropic_auth_source: Some(AnthropicAuthSourceArg::ApiKey),
            anthropic_base_url: Some("https://anthropic.example".to_string()),
            anthropic_version: Some("2023-06-01".to_string()),
            anthropic_beta_headers: vec!["beta-one".to_string()],
            ..test_serve_args(&temp)
        };
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;

        let routes = resolve_route_inventory(&args, manager).await?;

        let ModelRouteConfig::Anthropic(config) = routes[0].route_config() else {
            panic!(
                "expected anthropic route, found {:?}",
                routes[0].route_config()
            );
        };
        assert_eq!(routes[0].route_id(), "anthropic");
        assert_eq!(config.base_url, "https://anthropic.example");
        assert_eq!(config.anthropic_version, "2023-06-01");
        assert_eq!(config.beta_headers, vec!["beta-one".to_string()]);
        assert!(config.api_key.is_none());
        let auth_material = config
            .request_auth_provider
            .as_ref()
            .expect("anthropic route should use dynamic request auth")
            .resolve()
            .await?;
        assert_eq!(
            auth_material.headers.get("x-api-key").map(String::as_str),
            Some("anthropic-key")
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_without_routes_file_builds_xai_primary_route() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let args = ServeArgs {
            provider: ProviderKind::Xai,
            api_key: Some("xai-key".to_string()),
            xai_base_url: Some("https://xai.example/v1".to_string()),
            openai_auth_source: Some(OpenAiAuthSourceArg::ApiKey),
            anthropic_auth_source: Some(AnthropicAuthSourceArg::ApiKey),
            ..test_serve_args(&temp)
        };
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;

        let routes = resolve_route_inventory(&args, manager).await?;

        let ModelRouteConfig::XAi(config) = routes[0].route_config() else {
            panic!("expected xai route, found {:?}", routes[0].route_config());
        };
        assert_eq!(routes[0].route_id(), "xai");
        assert_eq!(config.base_url, "https://xai.example/v1");
        assert!(config.api_key.is_none());
        let auth_material = config
            .request_auth_provider
            .as_ref()
            .expect("xai route should use dynamic request auth")
            .resolve()
            .await?;
        assert_eq!(
            auth_material
                .headers
                .get("Authorization")
                .map(String::as_str),
            Some("Bearer xai-key")
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_without_routes_file_honors_default_route_override()
    -> Result<()> {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::TempDir::new()?;
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-fallback");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("XAI_API_KEY");
        }
        let args = ServeArgs {
            provider: ProviderKind::Google,
            google_api_key: Some("google-key".to_string()),
            default_route: Some("openai".to_string()),
            openai_auth_source: Some(OpenAiAuthSourceArg::ApiKey),
            anthropic_auth_source: Some(AnthropicAuthSourceArg::ApiKey),
            ..test_serve_args(&temp)
        };
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;

        let routes = resolve_route_inventory(&args, manager).await?;

        assert_eq!(routes[0].route_id(), "openai");
        assert!(routes.iter().any(|route| route.route_id() == "google"));
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
        Ok(())
    }

    #[tokio::test]
    async fn resolve_route_inventory_without_routes_file_rejects_unknown_default_route()
    -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let args = ServeArgs {
            provider: ProviderKind::Google,
            google_api_key: Some("google-key".to_string()),
            default_route: Some("missing".to_string()),
            openai_auth_source: Some(OpenAiAuthSourceArg::ApiKey),
            anthropic_auth_source: Some(AnthropicAuthSourceArg::ApiKey),
            ..test_serve_args(&temp)
        };
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;

        let error = resolve_route_inventory(&args, manager)
            .await
            .expect_err("unknown default route should fail");
        assert!(
            error
                .to_string()
                .contains("default route `missing` is not configured")
        );
        Ok(())
    }

    #[tokio::test]
    async fn global_auth_store_reports_missing_master_key_for_encrypted_store() -> Result<()> {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::TempDir::new()?;
        let args = test_serve_args(&temp);
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        manager
            .store_openai_api_key(AuthSlotId::new("openai.prod"), "test-key", None, None)
            .await?;
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        let error = AuthManager::new(global_auth_store_path(&args.state_root))
            .err()
            .ok_or_else(|| anyhow!("encrypted global auth store should require a master key"))?;
        assert!(error.to_string().contains("is encrypted"));
        Ok(())
    }

    fn test_serve_args(temp: &tempfile::TempDir) -> ServeArgs {
        ServeArgs {
            bind: DEFAULT_BIND.parse().expect("bind"),
            state_root: temp.path().join("state"),
            workspace_root: None,
            mcp_config: None,
            mcp_credentials: None,
            mcp_profiles: Vec::new(),
            mcp_discovery: McpDiscoveryArg::Auto,
            connectors_config: None,
            skill_roots: Vec::new(),
            log_format: LogFormatArg::Auto,
            log_level: LogLevelArg::Info,
            event_history_capacity: DEFAULT_EVENT_HISTORY_CAPACITY,
            http_auth_mode: HttpAuthModeArg::Auto,
            http_admin_token: None,
            http_admin_token_file: None,
            http_readonly_token: None,
            http_readonly_token_file: None,
            http_cors_allow_origins: Vec::new(),
            provider: ProviderKind::Anthropic,
            routes_file: None,
            default_route: None,
            model: None,
            api_key: None,
            google_api_key: None,
            anthropic_base_url: None,
            anthropic_version: None,
            anthropic_beta_headers: Vec::new(),
            google_base_url: None,
            image_provider: None,
            image_model: None,
            image_api_key: None,
            google_image_model: None,
            transcription_provider: None,
            transcription_model: None,
            transcription_api_key: None,
            transcription_base_url: None,
            openai_base_url: None,
            openrouter_base_url: None,
            xai_base_url: None,
            openai_organization: None,
            openai_project: None,
            openai_auth_source: None,
            openai_auth_file: None,
            anthropic_auth_source: None,
            anthropic_credentials_file: None,
            max_child_depth: 4,
            max_live_children_per_parent: 6,
            max_live_descendants_per_root: 24,
            max_spawns_per_run: 6,
            subagent_policy_file: None,
            max_live_sidechains_global: 128,
            max_live_sidechains_per_session: 24,
            spawn_rate_window_ms: 60_000,
            max_spawns_per_session_window: 60,
            max_spawns_per_profile_window: 60,
            max_spawns_per_project_window: 120,
            max_spawns_global_window: 240,
            max_spawn_input_tokens_per_request: 128_000,
            max_spawn_output_tokens_per_request: 64_000,
            model_budget_max_total_output_tokens: 1_000_000,
            model_budget_max_total_cost_usd: 500.0,
            max_spawn_cost_microusd_per_window: 1_000_000,
            max_spawn_cpu_ms_per_window: 240_000,
            estimated_spawn_cost_microusd: 1_000,
            estimated_spawn_cpu_ms: 1_000,
            scheduler_retry_base_delay_ms: 500,
            scheduler_retry_max_delay_ms: 30_000,
            scheduler_retry_jitter_ms: 250,
            scheduler_retry_max_attempts: 0,
        }
    }

    #[test]
    fn cli_parses_global_daemon_token() {
        let cli = Cli::parse_from(["kheish-daemon", "--token", "secret-token", "status"]);
        assert_eq!(cli.token.as_deref(), Some("secret-token"));
    }

    #[test]
    fn control_plane_auth_auto_allows_loopback_without_tokens() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = test_serve_args(&temp);
        let config = resolve_control_plane_auth_config(&args).expect("auth config");
        assert!(!config.is_enabled());
    }

    #[test]
    fn control_plane_auth_auto_rejects_non_loopback_without_admin_token() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.bind = "0.0.0.0:4000".parse().expect("bind");
        let error = resolve_control_plane_auth_config(&args).expect_err("should reject bind");
        assert!(
            error
                .to_string()
                .contains("refusing to expose daemon control-plane on non-loopback bind")
        );
    }

    #[test]
    fn control_plane_auth_none_rejects_non_loopback_bind() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.bind = "0.0.0.0:4000".parse().expect("bind");
        args.http_auth_mode = HttpAuthModeArg::None;
        let error = resolve_control_plane_auth_config(&args).expect_err("should reject bind");
        assert!(
            error.to_string().contains("with --http-auth-mode none"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn control_plane_auth_bearer_requires_admin_token() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.http_auth_mode = HttpAuthModeArg::Bearer;
        let error = resolve_control_plane_auth_config(&args).expect_err("missing admin token");
        assert!(
            error
                .to_string()
                .contains("--http-auth-mode bearer requires --http-admin-token")
        );
    }

    #[test]
    fn control_plane_auth_reads_admin_and_read_only_tokens() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.http_admin_token = Some("admin-secret".to_string());
        args.http_readonly_token = Some("readonly-secret".to_string());
        let config = resolve_control_plane_auth_config(&args).expect("auth config");
        assert_eq!(config.admin_token.as_deref(), Some("admin-secret"));
        assert_eq!(config.read_only_token.as_deref(), Some("readonly-secret"));
    }

    #[test]
    fn control_plane_auth_rejects_duplicate_admin_and_read_only_tokens() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.http_admin_token = Some("same-secret".to_string());
        args.http_readonly_token = Some("same-secret".to_string());
        let error = resolve_control_plane_auth_config(&args).expect_err("duplicates should fail");
        assert!(
            error.to_string().contains("must be distinct"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn control_plane_auth_reads_token_from_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let token_file = temp.path().join("admin.token");
        std::fs::write(&token_file, "admin-secret\n").expect("write token file");
        let mut args = test_serve_args(&temp);
        args.http_auth_mode = HttpAuthModeArg::Bearer;
        args.http_admin_token_file = Some(token_file);
        let config = resolve_control_plane_auth_config(&args).expect("auth config");
        assert_eq!(config.admin_token.as_deref(), Some("admin-secret"));
    }

    #[test]
    fn control_plane_cors_defaults_to_loopback_policy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = test_serve_args(&temp);
        let config = resolve_control_plane_cors_config(&args).expect("cors config");
        assert_eq!(config, ControlPlaneCorsConfig::loopback());
    }

    #[test]
    fn control_plane_cors_accepts_exact_loopback_allowlist() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.http_cors_allow_origins = vec![
            "http://localhost:5173".to_string(),
            " http://127.0.0.1:3000 ".to_string(),
            "http://localhost:5173".to_string(),
        ];
        let config = resolve_control_plane_cors_config(&args).expect("cors config");
        assert_eq!(
            config.allowed_origins,
            vec![
                "http://localhost:5173".to_string(),
                "http://127.0.0.1:3000".to_string()
            ]
        );
    }

    #[test]
    fn control_plane_cors_rejects_unsafe_origins() {
        let temp = tempfile::tempdir().expect("tempdir");
        for origin in [
            "",
            "*",
            "https://example.com",
            "http://localhost.evil.test",
            "http://localhost:5173/path",
            "file://localhost",
        ] {
            let mut args = test_serve_args(&temp);
            args.http_cors_allow_origins = vec![origin.to_string()];
            let error = resolve_control_plane_cors_config(&args).expect_err("origin should reject");
            assert!(
                error.to_string().contains("--http-cors-allow-origin"),
                "{origin}: {error}"
            );
        }
    }

    #[test]
    fn cli_parses_repeated_and_comma_delimited_cors_origins() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "serve",
            "--http-cors-allow-origin",
            "http://localhost:5173,http://127.0.0.1:3000",
            "--http-cors-allow-origin",
            "http://[::1]:4173",
        ]);
        let Command::Serve(args) = cli.command.expect("command") else {
            panic!("expected serve command");
        };
        assert_eq!(
            args.http_cors_allow_origins,
            vec![
                "http://localhost:5173".to_string(),
                "http://127.0.0.1:3000".to_string(),
                "http://[::1]:4173".to_string()
            ]
        );
    }

    #[test]
    fn bare_cli_injects_serve_for_serve_flags() {
        let cli = Cli::parse_from(normalize_cli_args([
            "kheish-daemon",
            "--provider",
            "openai",
            "--model",
            "gpt-5.4-mini",
        ]));
        match cli.command.expect("command") {
            Command::Serve(args) => {
                assert_eq!(args.provider, ProviderKind::Openai);
                assert_eq!(args.model.as_deref(), Some("gpt-5.4-mini"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn bare_cli_preserves_explicit_commands_after_globals() {
        let cli = Cli::parse_from(normalize_cli_args([
            "kheish-daemon",
            "--base-url",
            "http://127.0.0.1:9999",
            "status",
        ]));
        match cli.command.expect("command") {
            Command::Status => {}
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_boards_create_revision_client_revision_id() {
        let cli = Cli::parse_from([
            "kheish-daemon",
            "boards",
            "create-revision",
            "board-1",
            "--previous-revision-id",
            "board-revision-1",
            "--client-revision-id",
            "client-rev-1",
            "--render-asset-id",
            "asset-render-1",
            "--state-asset-id",
            "asset-state-1",
        ]);
        let Some(Command::Boards {
            command: BoardsCommand::CreateRevision(args),
        }) = cli.command
        else {
            panic!("expected boards create-revision command");
        };
        assert_eq!(args.board_id, "board-1");
        assert_eq!(
            args.previous_revision_id.as_deref(),
            Some("board-revision-1")
        );
        assert_eq!(args.client_revision_id.as_deref(), Some("client-rev-1"));
        assert_eq!(args.render_asset_id, "asset-render-1");
        assert_eq!(args.state_asset_id.as_deref(), Some("asset-state-1"));
    }

    #[test]
    fn bare_cli_does_not_rewrite_unknown_subcommands_as_serve() {
        let error = Cli::try_parse_from(normalize_cli_args(["kheish-daemon", "statuz"]))
            .expect_err("unknown subcommand should fail");
        let rendered = error.to_string();
        assert!(rendered.contains("unrecognized subcommand"));
        assert!(rendered.contains("statuz"));
    }

    #[test]
    fn shared_env_overrides_provider_specific_values() {
        let model = resolve_model_with(ProviderKind::Openai, None, |key| match key {
            "KHEISH_MODEL" => Some("shared-model".to_string()),
            "OPENAI_MODEL" => Some("gpt-5.4".to_string()),
            _ => None,
        });
        let api_key = resolve_api_key_with(ProviderKind::Anthropic, None, |key| match key {
            "KHEISH_API_KEY" => Some("shared-key".to_string()),
            "ANTHROPIC_API_KEY" => Some("anthropic-key".to_string()),
            _ => None,
        });
        assert_eq!(model, "shared-model");
        assert_eq!(api_key.as_deref(), Some("shared-key"));
    }

    #[test]
    fn google_env_aliases_are_resolved_for_model_and_api_key() {
        let model = resolve_model_with(ProviderKind::Google, None, |key| match key {
            "GEMINI_MODEL" => Some("gemini-2.5-pro".to_string()),
            _ => None,
        });
        let api_key = resolve_api_key_with(ProviderKind::Google, None, |key| match key {
            "GEMINI_API_KEY" => Some("gemini-key".to_string()),
            _ => None,
        });
        assert_eq!(model, "gemini-2.5-pro");
        assert_eq!(api_key.as_deref(), Some("gemini-key"));
    }

    #[test]
    fn resolve_additional_image_backends_uses_legacy_google_image_configuration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Openai;
        args.google_api_key = Some("google-image-key".to_string());
        args.google_image_model = Some("gemini-3-pro-image-preview".to_string());
        args.google_base_url = Some("https://example.test/v1beta".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let backends =
            resolve_additional_image_backends(&args, manager).expect("additional backends");
        assert_eq!(backends.len(), 1);
        let ModelRouteConfig::Google(config) = backends[0].route_config() else {
            panic!("expected google image backend");
        };
        assert_eq!(config.model, "gemini-3-pro-image-preview");
        assert_eq!(config.api_key.as_deref(), Some("google-image-key"));
        assert_eq!(config.base_url, "https://example.test/v1beta");
        assert!(config.request_auth_provider.is_some());
    }

    #[test]
    fn resolve_additional_image_backends_supports_primary_google_image_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Google;
        args.api_key = Some("google-primary-key".to_string());
        args.google_image_model = Some("gemini-3-pro-image-preview".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let backends =
            resolve_additional_image_backends(&args, manager).expect("additional backends");
        assert_eq!(backends.len(), 1);
        let ModelRouteConfig::Google(config) = backends[0].route_config() else {
            panic!("expected google image backend");
        };
        assert_eq!(config.model, "gemini-3-pro-image-preview");
        assert_eq!(config.api_key.as_deref(), Some("google-primary-key"));
        assert!(config.request_auth_provider.is_some());
    }

    #[test]
    fn resolve_additional_image_backends_supports_generic_openai_image_configuration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Google;
        args.image_provider = Some(ProviderKind::Openai);
        args.image_model = Some("gpt-image-1.5".to_string());
        args.image_api_key = Some("openai-image-key".to_string());
        args.openai_base_url = Some("https://example.test/v1".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let backends =
            resolve_additional_image_backends(&args, manager).expect("additional backends");
        assert_eq!(backends.len(), 1);
        let ModelRouteConfig::OpenAi(config) = backends[0].route_config() else {
            panic!("expected openai image backend");
        };
        assert_eq!(config.model, "gpt-image-1.5");
        assert_eq!(config.api_key.as_deref(), Some("openai-image-key"));
        assert_eq!(config.base_url, "https://example.test/v1");
        assert!(config.request_auth_provider.is_some());
    }

    #[test]
    fn resolve_additional_image_backends_supports_generic_xai_image_configuration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Xai;
        args.api_key = Some("xai-image-key".to_string());
        args.image_model = Some("grok-imagine-image".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let backends =
            resolve_additional_image_backends(&args, manager).expect("additional backends");
        assert_eq!(backends.len(), 1);
        let ModelRouteConfig::XAi(config) = backends[0].route_config() else {
            panic!("expected xai image backend");
        };
        assert_eq!(config.model, "grok-imagine-image");
        assert_eq!(config.api_key.as_deref(), Some("xai-image-key"));
        assert!(config.request_auth_provider.is_some());
    }

    #[test]
    fn resolve_additional_image_backends_defaults_to_the_primary_provider_for_generic_image_model()
    {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Openai;
        args.api_key = Some("shared-openai-key".to_string());
        args.image_model = Some("gpt-image-1.5".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let backends =
            resolve_additional_image_backends(&args, manager).expect("additional backends");
        assert_eq!(backends.len(), 1);
        let ModelRouteConfig::OpenAi(config) = backends[0].route_config() else {
            panic!("expected openai image backend");
        };
        assert_eq!(config.model, "gpt-image-1.5");
        assert_eq!(config.api_key.as_deref(), Some("shared-openai-key"));
        assert!(config.request_auth_provider.is_some());
    }

    #[test]
    fn resolve_additional_transcription_backends_supports_explicit_openai_configuration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Anthropic;
        args.transcription_provider = Some(ProviderKind::Openai);
        args.transcription_model = Some("gpt-4o-transcribe".to_string());
        args.transcription_api_key = Some("openai-stt-key".to_string());
        args.transcription_base_url = Some("https://example.test/v1".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let backends = resolve_additional_transcription_backends(&args, manager)
            .expect("transcription backends");
        assert_eq!(backends.len(), 1);
        let ModelRouteConfig::OpenAi(config) = backends[0].route_config() else {
            panic!("expected openai transcription backend");
        };
        assert_eq!(config.model, "gpt-4o-transcribe");
        assert_eq!(config.api_key.as_deref(), Some("openai-stt-key"));
        assert_eq!(config.base_url, "https://example.test/v1");
        assert!(config.request_auth_provider.is_some());
    }

    #[test]
    fn resolve_additional_transcription_backends_defaults_to_primary_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Openai;
        args.api_key = Some("shared-openai-key".to_string());
        args.transcription_model = Some("gpt-4o-mini-transcribe".to_string());
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let backends = resolve_additional_transcription_backends(&args, manager)
            .expect("transcription backends");
        assert_eq!(backends.len(), 1);
        let ModelRouteConfig::OpenAi(config) = backends[0].route_config() else {
            panic!("expected openai transcription backend");
        };
        assert_eq!(config.model, "gpt-4o-mini-transcribe");
        assert_eq!(config.api_key.as_deref(), Some("shared-openai-key"));
        assert!(config.request_auth_provider.is_some());
    }

    #[test]
    fn resolve_additional_image_backends_rejects_unsupported_anthropic_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = ServeArgs {
            provider: ProviderKind::Openai,
            image_provider: Some(ProviderKind::Anthropic),
            image_model: Some("claude-image".to_string()),
            image_api_key: Some("anthropic-key".to_string()),
            ..test_serve_args(&temp)
        };
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let error = match resolve_additional_image_backends(&args, manager) {
            Ok(_) => panic!("anthropic image backend should fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("anthropic does not support dedicated image backends")
        );
    }

    #[test]
    fn resolve_additional_transcription_backends_rejects_non_openai_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = ServeArgs {
            provider: ProviderKind::Openai,
            transcription_provider: Some(ProviderKind::Google),
            transcription_model: Some("gemini-transcribe".to_string()),
            transcription_api_key: Some("google-key".to_string()),
            ..test_serve_args(&temp)
        };
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");

        let error = match resolve_additional_transcription_backends(&args, manager) {
            Ok(_) => panic!("non-openai transcription backend should fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("does not support dedicated transcription backends")
        );
    }

    #[test]
    fn resolve_additional_image_backends_rejects_invalid_provider_env_value() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var("KHEISH_IMAGE_PROVIDER", "bogus");
        }
        let args = test_serve_args(&temp);
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");
        let error = match resolve_additional_image_backends(&args, manager) {
            Ok(_) => panic!("invalid image provider env should fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid KHEISH_IMAGE_PROVIDER value")
        );
        unsafe {
            std::env::remove_var("KHEISH_IMAGE_PROVIDER");
        }
    }

    #[test]
    fn resolve_additional_transcription_backends_rejects_invalid_provider_env_value() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var("KHEISH_TRANSCRIPTION_PROVIDER", "bogus");
        }
        let args = test_serve_args(&temp);
        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");
        let error = match resolve_additional_transcription_backends(&args, manager) {
            Ok(_) => panic!("invalid transcription provider env should fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("invalid KHEISH_TRANSCRIPTION_PROVIDER value")
        );
        unsafe {
            std::env::remove_var("KHEISH_TRANSCRIPTION_PROVIDER");
        }
    }

    #[test]
    fn explicit_openai_codex_auth_source_wins_over_api_key_presence() {
        let selected =
            select_openai_auth_source(OpenAiAuthSourceArg::Codex, true, false, true).unwrap();
        assert_eq!(selected, ResolvedOpenAiAuthSource::Codex);
    }

    #[test]
    fn resolve_openai_auth_file_path_prefers_explicit_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let explicit = temp.path().join("custom-auth.json");
        let args = ServeArgs {
            provider: ProviderKind::Openai,
            openai_auth_file: Some(explicit.clone()),
            ..test_serve_args(&temp)
        };

        assert_eq!(resolve_openai_auth_file_path(&args), Some(explicit));
    }

    #[test]
    fn auto_openai_auth_source_prefers_codex_slot_when_no_api_key_exists() {
        let selected =
            select_openai_auth_source(OpenAiAuthSourceArg::Auto, false, true, false).unwrap();
        assert_eq!(selected, ResolvedOpenAiAuthSource::Codex);
    }

    #[test]
    fn auto_openai_auth_source_still_prefers_api_key_when_explicit_auth_file_is_set() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = ServeArgs {
            provider: ProviderKind::Openai,
            api_key: Some("sk-explicit".to_string()),
            openai_auth_source: Some(OpenAiAuthSourceArg::Auto),
            openai_auth_file: Some(temp.path().join("missing-auth.json")),
            ..test_serve_args(&temp)
        };

        let selected = resolve_openai_auth_source(&args, "openai-default").expect("auth source");
        assert_eq!(selected, ResolvedOpenAiAuthSource::ApiKey);
    }

    #[test]
    fn explicit_openai_auth_file_reports_missing_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing-auth.json");
        let args = ServeArgs {
            provider: ProviderKind::Openai,
            openai_auth_source: Some(OpenAiAuthSourceArg::Codex),
            openai_auth_file: Some(missing.clone()),
            ..test_serve_args(&temp)
        };

        let error = resolve_openai_auth_source(&args, "openai-default").expect_err("missing path");
        assert!(error.to_string().contains(&missing.display().to_string()));
    }

    #[test]
    fn resolve_route_value_reads_named_environment_variables() {
        let home = std::env::var("HOME").expect("HOME should be set for tests");
        assert_eq!(
            resolve_route_value(None, Some("HOME")).as_deref(),
            Some(home.as_str())
        );
    }

    #[test]
    fn resolve_route_api_key_prefers_route_specific_env_values() {
        let entry = RouteFileEntry {
            driver: RouteFileDriver::Openai,
            default_model: "gpt-5.4".to_string(),
            model_support: ModelSupportPolicy::Family,
            auth_ref: None,
            api_key: None,
            api_key_env: Some("HOME".to_string()),
            base_url: None,
            organization: None,
            organization_env: None,
            project: None,
            project_env: None,
            anthropic_version: None,
            anthropic_beta_headers: Vec::new(),
            openai_auth_source: None,
            openai_auth_file: None,
            anthropic_auth_source: None,
            anthropic_credentials_file: None,
            multimodal_input: None,
            native_web_search: None,
            image_generation: None,
            image_edit: None,
            audio_generation: None,
            transcription: None,
        };

        let home = std::env::var("HOME").expect("HOME should be set for tests");
        assert_eq!(
            resolve_route_api_key("openrouter", &entry).as_deref(),
            Some(home.as_str())
        );
    }

    #[test]
    fn custom_openai_routes_require_explicit_credentials_by_default() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = test_serve_args(&temp);
        let entry = RouteFileEntry {
            driver: RouteFileDriver::Openai,
            default_model: "openai/gpt-5.4-mini".to_string(),
            model_support: ModelSupportPolicy::Any,
            auth_ref: None,
            api_key: None,
            api_key_env: None,
            base_url: Some("https://openrouter.example/v1/responses".to_string()),
            organization: None,
            organization_env: None,
            project: None,
            project_env: None,
            anthropic_version: None,
            anthropic_beta_headers: Vec::new(),
            openai_auth_source: None,
            openai_auth_file: None,
            anthropic_auth_source: None,
            anthropic_credentials_file: None,
            multimodal_input: None,
            native_web_search: None,
            image_generation: None,
            image_edit: None,
            audio_generation: None,
            transcription: None,
        };

        let error = resolve_route_openai_auth_source(&args, "openrouter", &entry)
            .expect_err("custom route should require explicit credentials");
        assert!(
            error
                .to_string()
                .contains("route `openrouter` is missing an OpenAI-compatible API key")
        );
    }

    #[test]
    fn resolve_route_openai_auth_source_reports_corrupt_auth_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Openai;
        let auth_store_path = global_auth_store_path(&args.state_root);
        std::fs::create_dir_all(auth_store_path.parent().expect("auth store parent"))
            .expect("create auth store parent");
        std::fs::write(&auth_store_path, b"{not-json").expect("write corrupt auth store");
        let entry = RouteFileEntry {
            driver: RouteFileDriver::Openai,
            default_model: "gpt-5.4".to_string(),
            model_support: ModelSupportPolicy::Any,
            auth_ref: None,
            api_key: None,
            api_key_env: None,
            base_url: None,
            organization: None,
            organization_env: None,
            project: None,
            project_env: None,
            anthropic_version: None,
            anthropic_beta_headers: Vec::new(),
            openai_auth_source: Some(RouteFileOpenAiAuthSource::Codex),
            openai_auth_file: Some(temp.path().join("missing-auth.json")),
            anthropic_auth_source: None,
            anthropic_credentials_file: None,
            multimodal_input: None,
            native_web_search: None,
            image_generation: None,
            image_edit: None,
            audio_generation: None,
            transcription: None,
        };

        let error = resolve_route_openai_auth_source(&args, "openrouter", &entry)
            .expect_err("corrupt auth store should fail");
        assert!(error.to_string().contains("failed to parse auth store"));
    }

    #[tokio::test]
    async fn custom_openai_routes_support_explicit_codex_auth() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_auth_path = temp.path().join("auth.json");
        std::fs::write(
            &codex_auth_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "fresh-id-token",
                    "refresh_token": "fresh-refresh-token",
                    "account_id": "acc-route"
                }
            }))
            .expect("auth json"),
        )
        .expect("write auth json");
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }

        let mut args = test_serve_args(&temp);
        args.routes_file = Some(temp.path().join("unused-routes.toml"));
        let entry = RouteFileEntry {
            driver: RouteFileDriver::Openai,
            default_model: "openai/gpt-5.4-mini".to_string(),
            model_support: ModelSupportPolicy::Any,
            auth_ref: None,
            api_key: None,
            api_key_env: None,
            base_url: Some("https://openrouter.example/v1/responses".to_string()),
            organization: None,
            organization_env: None,
            project: None,
            project_env: None,
            anthropic_version: None,
            anthropic_beta_headers: Vec::new(),
            openai_auth_source: Some(RouteFileOpenAiAuthSource::Codex),
            openai_auth_file: Some(codex_auth_path),
            anthropic_auth_source: None,
            anthropic_credentials_file: None,
            multimodal_input: None,
            native_web_search: None,
            image_generation: None,
            image_edit: None,
            audio_generation: None,
            transcription: None,
        };

        let route = resolve_configured_openai_route(
            &args,
            "openrouter",
            &entry,
            AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager"),
        )
        .await
        .expect("custom codex route should resolve");
        assert_eq!(route.route_id(), "openrouter");
        assert_eq!(route.provider_name(), "openai");
        assert!(!route.capabilities().image_generation);
        assert!(!route.capabilities().image_edit);
        assert!(!route.capabilities().audio_generation);
        assert!(!route.capabilities().transcription);

        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");
        assert!(manager.has_slot(&route_auth_slot_id("openrouter")).await);
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn openai_codex_routes_reject_media_capability_overrides() -> Result<()> {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir()?;
        let codex_auth_path = temp.path().join("auth.json");
        std::fs::write(
            &codex_auth_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "fresh-id-token",
                    "refresh_token": "fresh-refresh-token",
                    "account_id": "acc-route"
                }
            }))?,
        )?;
        let routes_path = temp.path().join("routes.toml");
        std::fs::write(
            &routes_path,
            format!(
                r#"version = 1
default_route = "openai"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
openai_auth_source = "codex"
openai_auth_file = "{}"
audio_generation = true
"#,
                codex_auth_path.display()
            ),
        )?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let mut args = test_serve_args(&temp);
        args.routes_file = Some(routes_path);
        let manager = AuthManager::new(global_auth_store_path(&args.state_root))?;
        let error = resolve_route_inventory(&args, manager)
            .await
            .expect_err("Codex media capability override should fail");
        assert!(error.to_string().contains("Codex account auth"));
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        Ok(())
    }

    #[test]
    fn explicit_anthropic_claude_code_auth_source_wins_over_api_key_presence() {
        let selected =
            select_anthropic_auth_source(AnthropicAuthSourceArg::ClaudeCode, true, false, true)
                .unwrap();
        assert_eq!(selected, ResolvedAnthropicAuthSource::ClaudeCode);
    }

    #[test]
    fn resolve_anthropic_credentials_path_prefers_explicit_override() {
        let temp = tempfile::tempdir().expect("tempdir");
        let explicit = temp.path().join("custom-credentials.json");
        let args = ServeArgs {
            provider: ProviderKind::Anthropic,
            anthropic_credentials_file: Some(explicit.clone()),
            ..test_serve_args(&temp)
        };

        assert_eq!(resolve_anthropic_credentials_path(&args), Some(explicit));
    }

    #[test]
    fn auto_anthropic_auth_source_prefers_claude_code_slot_when_no_api_key_exists() {
        let selected =
            select_anthropic_auth_source(AnthropicAuthSourceArg::Auto, false, true, false).unwrap();
        assert_eq!(selected, ResolvedAnthropicAuthSource::ClaudeCode);
    }

    #[test]
    fn auto_anthropic_auth_source_still_prefers_api_key_when_explicit_credentials_file_is_set() {
        let temp = tempfile::tempdir().expect("tempdir");
        let args = ServeArgs {
            provider: ProviderKind::Anthropic,
            api_key: Some("anthropic-key".to_string()),
            anthropic_auth_source: Some(AnthropicAuthSourceArg::Auto),
            anthropic_credentials_file: Some(temp.path().join("missing-credentials.json")),
            ..test_serve_args(&temp)
        };

        let selected =
            resolve_anthropic_auth_source(&args, "anthropic-default").expect("auth source");
        assert_eq!(selected, ResolvedAnthropicAuthSource::ApiKey);
    }

    #[test]
    fn explicit_anthropic_credentials_file_reports_missing_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing-credentials.json");
        let args = ServeArgs {
            provider: ProviderKind::Anthropic,
            anthropic_auth_source: Some(AnthropicAuthSourceArg::ClaudeCode),
            anthropic_credentials_file: Some(missing.clone()),
            ..test_serve_args(&temp)
        };

        let error =
            resolve_anthropic_auth_source(&args, "anthropic-default").expect_err("missing path");
        assert!(error.to_string().contains(&missing.display().to_string()));
    }

    #[test]
    fn resolve_route_anthropic_auth_source_reports_corrupt_auth_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut args = test_serve_args(&temp);
        args.provider = ProviderKind::Anthropic;
        let auth_store_path = global_auth_store_path(&args.state_root);
        std::fs::create_dir_all(auth_store_path.parent().expect("auth store parent"))
            .expect("create auth store parent");
        std::fs::write(&auth_store_path, b"{not-json").expect("write corrupt auth store");
        let entry = RouteFileEntry {
            driver: RouteFileDriver::Anthropic,
            default_model: "claude-opus-4-6".to_string(),
            model_support: ModelSupportPolicy::Family,
            auth_ref: None,
            api_key: None,
            api_key_env: None,
            base_url: None,
            organization: None,
            organization_env: None,
            project: None,
            project_env: None,
            anthropic_version: None,
            anthropic_beta_headers: Vec::new(),
            openai_auth_source: None,
            openai_auth_file: None,
            anthropic_auth_source: Some(RouteFileAnthropicAuthSource::ClaudeCode),
            anthropic_credentials_file: Some(temp.path().join("missing-credentials.json")),
            multimodal_input: None,
            native_web_search: None,
            image_generation: None,
            image_edit: None,
            audio_generation: None,
            transcription: None,
        };

        let error = resolve_route_anthropic_auth_source(&args, "anthropic-route", &entry)
            .expect_err("corrupt auth store should fail");
        assert!(error.to_string().contains("failed to parse auth store"));
    }

    #[tokio::test]
    async fn custom_anthropic_routes_support_explicit_claude_code_auth() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let credentials_path = temp.path().join("claude-credentials.json");
        std::fs::write(
            &credentials_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "fresh-access-token",
                    "refreshToken": "fresh-refresh-token",
                    "expiresAt": u64::MAX,
                    "scopes": ["user:profile", "user:inference"]
                }
            }))
            .expect("credentials json"),
        )
        .expect("write credentials json");
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }

        let args = test_serve_args(&temp);
        let entry = RouteFileEntry {
            driver: RouteFileDriver::Anthropic,
            default_model: "claude-opus-4-6".to_string(),
            model_support: ModelSupportPolicy::Family,
            auth_ref: None,
            api_key: None,
            api_key_env: None,
            base_url: None,
            organization: None,
            organization_env: None,
            project: None,
            project_env: None,
            anthropic_version: None,
            anthropic_beta_headers: Vec::new(),
            openai_auth_source: None,
            openai_auth_file: None,
            anthropic_auth_source: Some(RouteFileAnthropicAuthSource::ClaudeCode),
            anthropic_credentials_file: Some(credentials_path),
            multimodal_input: None,
            native_web_search: None,
            image_generation: None,
            image_edit: None,
            audio_generation: None,
            transcription: None,
        };

        let route = resolve_configured_anthropic_route(
            &args,
            "research",
            &entry,
            AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager"),
        )
        .await
        .expect("custom claude-code route should resolve");
        assert_eq!(route.route_id(), "research");
        assert_eq!(route.provider_name(), "anthropic");

        let manager = AuthManager::new(global_auth_store_path(&args.state_root)).expect("manager");
        assert!(manager.has_slot(&route_auth_slot_id("research")).await);
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_openai_codex_slot_preserves_existing_state() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_auth_path = temp.path().join("auth.json");
        std::fs::write(
            &codex_auth_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "stale-id-token",
                    "refresh_token": "stale-refresh-token",
                    "account_id": "acc-test"
                }
            }))
            .expect("auth json"),
        )
        .expect("write auth json");

        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(temp.path().join("slots.json")).expect("manager");
        manager
            .put_record(
                OpenAiAuthBackend::static_api_key_record(
                    AuthSlotId::new("openai-default"),
                    "sk-existing",
                    None,
                    None,
                )
                .expect("record"),
            )
            .await
            .expect("put record");

        ensure_openai_codex_slot_from_path(
            &manager,
            AuthSlotId::new("openai-default"),
            codex_auth_path,
            None,
            None,
        )
        .await
        .expect("ensure slot");

        let resolved = manager
            .resolve(&AuthSlotId::new("openai-default"), false)
            .await
            .expect("resolve");
        let expected_authorization = format!("Bearer {}{}", "sk-", "existing");
        assert_eq!(
            resolved.headers.get("Authorization"),
            Some(&expected_authorization)
        );
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_openai_codex_slot_does_not_require_source_file_when_slot_exists() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(temp.path().join("slots.json")).expect("manager");
        manager
            .put_record(
                OpenAiAuthBackend::static_api_key_record(
                    AuthSlotId::new("openai-default"),
                    "sk-existing",
                    None,
                    None,
                )
                .expect("record"),
            )
            .await
            .expect("put record");

        ensure_openai_codex_slot_from_path(
            &manager,
            AuthSlotId::new("openai-default"),
            temp.path().join("missing-auth.json"),
            None,
            None,
        )
        .await
        .expect("ensure slot");

        let resolved = manager
            .resolve(&AuthSlotId::new("openai-default"), false)
            .await
            .expect("resolve");
        let expected_authorization = format!("Bearer {}{}", "sk-", "existing");
        assert_eq!(
            resolved.headers.get("Authorization"),
            Some(&expected_authorization)
        );
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_openai_codex_slot_imports_from_explicit_auth_file() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_auth_path = temp.path().join("custom-openai-auth.json");
        std::fs::write(
            &codex_auth_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "fresh-id-token",
                    "refresh_token": "fresh-refresh-token",
                    "account_id": "acc-explicit"
                }
            }))
            .expect("auth json"),
        )
        .expect("write auth json");

        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(temp.path().join("slots.json")).expect("manager");
        ensure_openai_codex_slot(
            &manager,
            AuthSlotId::new("openai-default"),
            Some(codex_auth_path),
            None,
            None,
        )
        .await
        .expect("ensure slot");

        assert!(manager.has_slot(&AuthSlotId::new("openai-default")).await);
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_openai_codex_slot_requires_master_key_for_bootstrap_import() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let codex_auth_path = temp.path().join("custom-openai-auth.json");
        std::fs::write(
            &codex_auth_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "id_token": "fresh-id-token",
                    "refresh_token": "fresh-refresh-token",
                    "account_id": "acc-explicit"
                }
            }))
            .expect("auth json"),
        )
        .expect("write auth json");
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        let manager = AuthManager::new(temp.path().join("slots.json")).expect("manager");
        let error = ensure_openai_codex_slot(
            &manager,
            AuthSlotId::new("openai-default"),
            Some(codex_auth_path),
            None,
            None,
        )
        .await
        .expect_err("boot import without a master key should fail");
        assert!(error.to_string().contains(AUTH_STORE_MASTER_KEY_ENV));
    }

    #[tokio::test]
    async fn resolve_openai_provider_uses_persisted_slot_when_explicit_auth_file_is_missing() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let state_root = temp.path().join("state");
        std::fs::create_dir_all(state_root.join("auth")).expect("auth dir");
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(global_auth_store_path(&state_root)).expect("manager");
        manager
            .put_record(
                OpenAiAuthBackend::static_api_key_record(
                    AuthSlotId::new("openai-default"),
                    "sk-existing",
                    None,
                    None,
                )
                .expect("record"),
            )
            .await
            .expect("put record");
        let args = ServeArgs {
            state_root: state_root.clone(),
            provider: ProviderKind::Openai,
            openai_auth_source: Some(OpenAiAuthSourceArg::Codex),
            openai_auth_file: Some(temp.path().join("missing-auth.json")),
            openai_organization: Some("org-123".to_string()),
            openai_project: Some("proj-456".to_string()),
            ..test_serve_args(&temp)
        };

        let provider = resolve_openai_provider(&args, manager.clone())
            .await
            .expect("provider");
        let material = provider
            .request_auth_provider
            .as_ref()
            .expect("request auth provider")
            .resolve()
            .await
            .expect("resolve auth");
        let expected_authorization = format!("Bearer {}{}", "sk-", "existing");
        assert_eq!(
            material.headers.get("Authorization"),
            Some(&expected_authorization)
        );
        assert_eq!(provider.organization.as_deref(), Some("org-123"));
        assert_eq!(provider.project.as_deref(), Some("proj-456"));
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_anthropic_claude_code_slot_preserves_existing_state() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let credentials_path = temp.path().join(".credentials.json");
        std::fs::write(
            &credentials_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "stale-access-token",
                    "refreshToken": "stale-refresh-token",
                    "expiresAt": 1,
                    "scopes": ["user:profile", "user:inference"]
                }
            }))
            .expect("credentials json"),
        )
        .expect("write credentials json");

        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(temp.path().join("slots.json")).expect("manager");
        manager
            .put_record(
                AnthropicAuthBackend::static_api_key_record(
                    AuthSlotId::new("anthropic-default"),
                    "anthropic-key-existing",
                )
                .expect("record"),
            )
            .await
            .expect("put record");

        ensure_anthropic_claude_code_slot_from_path(
            &manager,
            AuthSlotId::new("anthropic-default"),
            credentials_path,
        )
        .await
        .expect("ensure slot");

        let resolved = manager
            .resolve(&AuthSlotId::new("anthropic-default"), false)
            .await
            .expect("resolve");
        assert_eq!(
            resolved.headers.get("x-api-key"),
            Some(&"anthropic-key-existing".to_string())
        );
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_anthropic_claude_code_slot_does_not_require_source_file_when_slot_exists() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(temp.path().join("slots.json")).expect("manager");
        manager
            .put_record(
                AnthropicAuthBackend::static_api_key_record(
                    AuthSlotId::new("anthropic-default"),
                    "anthropic-key-existing",
                )
                .expect("record"),
            )
            .await
            .expect("put record");

        ensure_anthropic_claude_code_slot_from_path(
            &manager,
            AuthSlotId::new("anthropic-default"),
            temp.path().join("missing-credentials.json"),
        )
        .await
        .expect("ensure slot");

        let resolved = manager
            .resolve(&AuthSlotId::new("anthropic-default"), false)
            .await
            .expect("resolve");
        assert_eq!(
            resolved.headers.get("x-api-key"),
            Some(&"anthropic-key-existing".to_string())
        );
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_anthropic_claude_code_slot_imports_from_explicit_credentials_file() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let credentials_path = temp.path().join("custom-claude-credentials.json");
        std::fs::write(
            &credentials_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "fresh-access-token",
                    "refreshToken": "fresh-refresh-token",
                    "expiresAt": u64::MAX,
                    "scopes": ["user:profile", "user:inference"]
                }
            }))
            .expect("credentials json"),
        )
        .expect("write credentials json");

        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(temp.path().join("slots.json")).expect("manager");
        ensure_anthropic_claude_code_slot(
            &manager,
            AuthSlotId::new("anthropic-default"),
            Some(credentials_path),
        )
        .await
        .expect("ensure slot");

        assert!(
            manager
                .has_slot(&AuthSlotId::new("anthropic-default"))
                .await
        );
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }

    #[tokio::test]
    async fn ensure_anthropic_claude_code_slot_requires_master_key_for_bootstrap_import() {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let credentials_path = temp.path().join("custom-claude-credentials.json");
        std::fs::write(
            &credentials_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "claudeAiOauth": {
                    "accessToken": "fresh-access-token",
                    "refreshToken": "fresh-refresh-token",
                    "expiresAt": u64::MAX,
                    "scopes": ["user:profile", "user:inference"]
                }
            }))
            .expect("credentials json"),
        )
        .expect("write credentials json");
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
        let manager = AuthManager::new(temp.path().join("slots.json")).expect("manager");
        let error = ensure_anthropic_claude_code_slot(
            &manager,
            AuthSlotId::new("anthropic-default"),
            Some(credentials_path),
        )
        .await
        .expect_err("boot import without a master key should fail");
        assert!(error.to_string().contains(AUTH_STORE_MASTER_KEY_ENV));
    }

    #[tokio::test]
    async fn resolve_anthropic_provider_uses_persisted_slot_when_explicit_credentials_file_is_missing()
     {
        let _guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempfile::tempdir().expect("tempdir");
        let state_root = temp.path().join("state");
        std::fs::create_dir_all(state_root.join("auth")).expect("auth dir");
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let manager = AuthManager::new(global_auth_store_path(&state_root)).expect("manager");
        manager
            .put_record(
                AnthropicAuthBackend::static_api_key_record(
                    AuthSlotId::new("anthropic-default"),
                    "anthropic-key-existing",
                )
                .expect("record"),
            )
            .await
            .expect("put record");
        let args = ServeArgs {
            state_root: state_root.clone(),
            provider: ProviderKind::Anthropic,
            anthropic_auth_source: Some(AnthropicAuthSourceArg::ClaudeCode),
            anthropic_credentials_file: Some(temp.path().join("missing-credentials.json")),
            ..test_serve_args(&temp)
        };

        let provider = resolve_anthropic_provider(&args, manager.clone())
            .await
            .expect("provider");
        let material = provider
            .request_auth_provider
            .as_ref()
            .expect("request auth provider")
            .resolve()
            .await
            .expect("resolve auth");
        assert_eq!(
            material.headers.get("x-api-key"),
            Some(&"anthropic-key-existing".to_string())
        );
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }
    }
}
