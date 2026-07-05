//! Top-level CLI dispatch for the daemon binary.

use std::ffi::OsString;

use anyhow::Result;
use clap::Parser;

/// Parses the CLI, builds shared helpers, and dispatches the selected command.
pub(crate) async fn run() -> Result<()> {
    let cli = parse_cli();
    let printer = crate::cli::Printer { format: cli.output };
    let token = crate::cli::resolve_cli_control_plane_token(&cli)?;
    let client = crate::cli::DaemonHttpClient::new(cli.base_url.clone(), token);

    match cli
        .command
        .expect("normalized CLI should always produce a command")
    {
        crate::Command::Serve(args) => crate::cli::serve::run_serve(args).await,
        crate::Command::Up(args) => crate::cli::serve::run_up(args).await,
        crate::Command::Status => {
            let status = crate::cli::commands::runtime::fetch_daemon_status(&client).await?;
            printer.print(&status)
        }
        crate::Command::Doctor {
            cors_origin,
            command,
        } => match command {
            Some(crate::DoctorCommand::Routes(args)) => {
                crate::cli::commands::runtime::run_doctor_routes(&client, &printer, args).await
            }
            None => crate::cli::commands::runtime::run_doctor(&client, &printer, cors_origin).await,
        },
        crate::Command::Capabilities => {
            let capabilities = client
                .get_json::<kheish_daemon::DaemonCapabilities>("/v1/capabilities")
                .await?;
            printer.print(&capabilities)
        }
        crate::Command::Runtime { command } => {
            crate::cli::commands::runtime::run_runtime_command(&client, &printer, command).await
        }
        crate::Command::Mcp { command } => {
            crate::cli::commands::mcp::run_mcp_command(&client, &printer, command).await
        }
        crate::Command::Events { command } => {
            crate::cli::commands::runtime::run_events_command(&client, &printer, command).await
        }
        crate::Command::Assets { command } => {
            crate::cli::commands::assets::run_assets_command(&client, &printer, command).await
        }
        crate::Command::Boards { command } => {
            crate::cli::commands::boards::run_boards_command(&client, &printer, command).await
        }
        crate::Command::Channels { command } => {
            crate::cli::commands::channels::run_channels_command(&client, &printer, command).await
        }
        crate::Command::Projects { command } => {
            crate::cli::commands::projects::run_projects_command(&client, &printer, command).await
        }
        crate::Command::Playbooks { command } => {
            crate::cli::commands::playbooks::run_playbooks_command(&client, &printer, command).await
        }
        crate::Command::Stack { command } => {
            crate::cli::commands::stack::run_stack_command(&client, &printer, command).await
        }
        crate::Command::Flows { command } => {
            crate::cli::commands::playbooks::run_flows_command(&client, &printer, command).await
        }
        crate::Command::Derivations { command } => {
            crate::cli::commands::derivations::run_derivations_command(&client, &printer, command)
                .await
        }
        crate::Command::Learnings { command } => {
            crate::cli::commands::learnings::run_learnings_command(&client, &printer, command).await
        }
        crate::Command::Observations { command } => {
            crate::cli::commands::observations::run_observations_command(&client, &printer, command)
                .await
        }
        crate::Command::Capture { command } => {
            crate::cli::commands::capture::run_capture_command(&client, &printer, command).await
        }
        crate::Command::Sessions { command } => {
            crate::cli::commands::sessions::run_sessions_command(&client, &printer, command).await
        }
        crate::Command::Personas { command } => {
            crate::cli::commands::personas::run_personas_command(&client, &printer, command).await
        }
        crate::Command::Secrets { command } => {
            crate::cli::commands::secrets::run_secrets_command(&client, &printer, command).await
        }
        crate::Command::Connectors { command } => {
            crate::cli::commands::connectors::run_connectors_command(&client, &printer, command)
                .await
        }
        crate::Command::Deliveries { command } => {
            crate::cli::commands::deliveries::run_deliveries_command(&client, &printer, command)
                .await
        }
        crate::Command::Approvals { command } => {
            crate::cli::commands::approvals::run_approvals_command(&client, &printer, command).await
        }
        crate::Command::Questions { command } => {
            crate::cli::commands::questions::run_questions_command(&client, &printer, command).await
        }
        crate::Command::Run { command } | crate::Command::Runs { command } => {
            crate::cli::commands::runs::run_runs_command(&client, &printer, command).await
        }
        crate::Command::Agents { command } => {
            crate::cli::commands::agents::run_agents_command(&client, &printer, command).await
        }
        crate::Command::Schedules { command } => {
            crate::cli::commands::schedules::run_schedules_command(&client, &printer, command).await
        }
        crate::Command::Tasks { command } => {
            crate::cli::commands::tasks::run_tasks_command(&client, &printer, command).await
        }
        crate::Command::Mailboxes { command } => {
            crate::cli::commands::mailboxes::run_mailboxes_command(&client, &printer, command).await
        }
    }
}

fn parse_cli() -> crate::Cli {
    crate::Cli::parse_from(normalize_cli_args(std::env::args_os()))
}

/// Normalizes bare daemon invocations so serve flags still dispatch to `serve`.
pub(crate) fn normalize_cli_args<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into).collect::<Vec<_>>();
    if let Some(index) = serve_insertion_index(&args) {
        args.insert(index, OsString::from("serve"));
    }
    args
}

fn serve_insertion_index(args: &[OsString]) -> Option<usize> {
    if args.len() <= 1 {
        return Some(args.len());
    }

    let mut index = 1;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if is_passthrough_top_level_argument(&argument) || is_top_level_command(&argument) {
            return None;
        }
        if consumes_global_option_value(&argument) {
            index += 2;
            continue;
        }
        if is_inline_global_option(&argument) {
            index += 1;
            continue;
        }
        return argument.starts_with('-').then_some(index);
    }

    Some(index)
}

fn is_top_level_command(argument: &str) -> bool {
    matches!(
        argument,
        "serve"
            | "start"
            | "status"
            | "doctor"
            | "capabilities"
            | "runtime"
            | "mcp"
            | "events"
            | "assets"
            | "projects"
            | "project"
            | "playbooks"
            | "playbook"
            | "stack"
            | "kheishfile"
            | "flows"
            | "flow"
            | "derivations"
            | "observations"
            | "observation"
            | "sessions"
            | "session"
            | "personas"
            | "persona"
            | "secrets"
            | "secret"
            | "deliveries"
            | "delivery"
            | "approvals"
            | "approval"
            | "questions"
            | "question"
            | "run"
            | "runs"
            | "agents"
            | "agent"
            | "schedules"
            | "schedule"
            | "tasks"
            | "task"
            | "mailboxes"
            | "mailbox"
            | "help"
    )
}

fn is_passthrough_top_level_argument(argument: &str) -> bool {
    matches!(argument, "-h" | "--help" | "-V" | "--version")
}

fn consumes_global_option_value(argument: &str) -> bool {
    matches!(
        argument,
        "--base-url" | "--output" | "--token" | "--token-file"
    )
}

fn is_inline_global_option(argument: &str) -> bool {
    argument.starts_with("--base-url=")
        || argument.starts_with("--output=")
        || argument.starts_with("--token=")
        || argument.starts_with("--token-file=")
}
