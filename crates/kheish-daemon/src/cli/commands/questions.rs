//! Structured question command handlers.

use std::collections::BTreeSet;
use std::io::Write as _;

use anyhow::{Context, Result, anyhow, bail};

/// Handles `questions ...`.
pub(crate) async fn run_questions_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::QuestionsCommand,
) -> Result<()> {
    match command {
        crate::QuestionsCommand::List {
            session_id,
            pagination,
        } => {
            let mut path = "/v1/questions".to_string();
            let mut params = Vec::new();
            if let Some(session_id) = session_id.filter(|value| !value.trim().is_empty()) {
                params.push(format!(
                    "session_id={}",
                    crate::cli::url_encode_component(&session_id)
                ));
            }
            pagination.append_query_params(&mut params);
            if !params.is_empty() {
                path.push('?');
                path.push_str(&params.join("&"));
            }
            if pagination.wants_page() {
                let questions = client
                    .get_list_page_compat::<kheish_daemon::PendingQuestionView>(&path)
                    .await?;
                printer.print(&questions)
            } else {
                let questions = client
                    .get_json::<Vec<kheish_daemon::PendingQuestionView>>(&path)
                    .await?;
                printer.print(&questions)
            }
        }
        crate::QuestionsCommand::Show {
            request_id,
            session_id,
        } => {
            let question =
                find_pending_question_view(client, session_id.as_deref(), &request_id).await?;
            printer.print(&question)
        }
        crate::QuestionsCommand::Answer(args) => {
            let run_for_args = if let Some(run_id) = args.run_id.as_deref() {
                let encoded_run_id = crate::cli::url_encode_path_segment(run_id);
                let run = client
                    .get_json::<kheish_daemon::RunView>(&format!("/v1/runs/{encoded_run_id}"))
                    .await?;
                if let Some(session_id) = args.session_id.as_deref()
                    && run.session_id != session_id
                {
                    bail!(
                        "run {run_id} belongs to session {}, not {}",
                        run.session_id,
                        session_id
                    );
                }
                Some(run)
            } else {
                None
            };
            let pending_question = if args.interactive {
                Some(if let Some(run) = run_for_args.as_ref() {
                    pending_question_view_from_run(run, &args.request_id)?
                } else {
                    find_pending_question_view(client, args.session_id.as_deref(), &args.request_id)
                        .await?
                })
            } else if args.run_id.is_none() {
                Some(
                    find_pending_question_view(
                        client,
                        args.session_id.as_deref(),
                        &args.request_id,
                    )
                    .await?,
                )
            } else {
                None
            };
            let answers = if args.interactive {
                if args.answers_json.is_some() || args.answers_file.is_some() {
                    bail!("--interactive cannot be combined with --answers-json or --answers-file");
                }
                if args.declined {
                    Vec::new()
                } else {
                    prompt_question_answers(
                        pending_question
                            .as_ref()
                            .expect("interactive pending question should be loaded")
                            .request
                            .clone(),
                    )
                    .await?
                }
            } else {
                crate::cli::load_question_answers(&args).await?
            };
            let resolution = kheish_daemon::ResolveUserQuestionRequest {
                idempotency_key: None,
                resolution: kheish_types::UserQuestionResolution {
                    request_id: args.request_id.clone(),
                    answers,
                    declined: args.declined,
                    justification: args.justification.clone(),
                },
            };
            let run_id = if let Some(run) = run_for_args {
                run.run_id
            } else if let Some(run_id) = pending_question.and_then(|question| question.run_id) {
                run_id
            } else {
                crate::cli::find_pending_question_run_id(
                    client,
                    args.session_id.as_deref(),
                    &args.request_id,
                )
                .await?
                .ok_or_else(|| {
                    anyhow!(
                        "pending question {} is not attached to a run",
                        args.request_id
                    )
                })?
            };
            let run_id = crate::cli::url_encode_path_segment(&run_id);
            let path = format!("/v1/runs/{run_id}/questions");
            let run = match args.idempotency_key.as_deref() {
                Some(key) => {
                    client
                        .post_json_with_idempotency_key::<_, kheish_daemon::RunView>(
                            &path,
                            key,
                            &resolution,
                        )
                        .await?
                }
                None => {
                    client
                        .post_json::<_, kheish_daemon::RunView>(&path, &resolution)
                        .await?
                }
            };
            if args.wait {
                let run =
                    crate::cli::wait_for_run(client, &run.run_id, args.poll_interval_ms).await?;
                printer.print(&run)
            } else {
                printer.print(&run)
            }
        }
        crate::QuestionsCommand::Cancel(args) => {
            if args.idempotency_key.is_some() && args.run_id.is_none() {
                bail!("questions cancel --idempotency-key requires --run-id for reliable replay");
            }
            let run = if let Some(run_id) = args.run_id.as_deref() {
                let encoded_run_id = crate::cli::url_encode_path_segment(run_id);
                let run = client
                    .get_json::<kheish_daemon::RunView>(&format!("/v1/runs/{encoded_run_id}"))
                    .await?;
                if args
                    .session_id
                    .as_deref()
                    .is_some_and(|session_id| session_id != run.session_id)
                {
                    bail!(
                        "run {run_id} belongs to session {}, not {}",
                        run.session_id,
                        args.session_id.as_deref().unwrap_or_default()
                    );
                }
                if args.idempotency_key.is_none() {
                    ensure_run_has_pending_question(&run, &args.request_id)?;
                }
                run
            } else {
                let run_id = find_pending_question_view(
                    client,
                    args.session_id.as_deref(),
                    &args.request_id,
                )
                .await?
                .run_id
                .ok_or_else(|| {
                    anyhow!(
                        "pending question {} is not attached to a run",
                        args.request_id
                    )
                })?;
                let encoded_run_id = crate::cli::url_encode_path_segment(&run_id);
                client
                    .get_json::<kheish_daemon::RunView>(&format!("/v1/runs/{encoded_run_id}"))
                    .await?
            };
            let run_id = crate::cli::url_encode_path_segment(&run.run_id);
            let request_id = crate::cli::url_encode_path_segment(&args.request_id);
            let path = format!("/v1/runs/{run_id}/questions/{request_id}/cancel");
            let request = kheish_daemon::CancelUserQuestionRequest {
                idempotency_key: None,
                justification: args.justification.clone(),
            };
            let run = match args.idempotency_key.as_deref() {
                Some(key) => {
                    client
                        .post_json_with_idempotency_key::<_, kheish_daemon::RunView>(
                            &path, key, &request,
                        )
                        .await?
                }
                None => {
                    client
                        .post_json::<_, kheish_daemon::RunView>(&path, &request)
                        .await?
                }
            };
            printer.print(&run)
        }
    }
}

async fn find_pending_question_view(
    client: &crate::cli::DaemonHttpClient,
    session_id: Option<&str>,
    request_id: &str,
) -> Result<kheish_daemon::PendingQuestionView> {
    let matches = crate::cli::collect_pending_questions(client, session_id)
        .await?
        .into_iter()
        .filter(|question| question.request.id == request_id)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(anyhow!("unknown pending question request {request_id}")),
        [question] => Ok(question.clone()),
        _ => bail!(
            "pending question request {request_id} is ambiguous; pass --session-id or --run-id"
        ),
    }
}

fn ensure_run_has_pending_question(run: &kheish_daemon::RunView, request_id: &str) -> Result<()> {
    if run
        .pending_questions
        .iter()
        .any(|question| question.id == request_id)
    {
        return Ok(());
    }
    bail!(
        "run {} is not waiting on pending question request {request_id}",
        run.run_id
    )
}

fn pending_question_view_from_run(
    run: &kheish_daemon::RunView,
    request_id: &str,
) -> Result<kheish_daemon::PendingQuestionView> {
    let request = run
        .pending_questions
        .iter()
        .find(|question| question.id == request_id)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "run {} is not waiting on pending question request {request_id}",
                run.run_id
            )
        })?;
    Ok(kheish_daemon::PendingQuestionView {
        session_id: run.session_id.clone(),
        agent_id: run.agent_id.clone(),
        run_id: Some(run.run_id.clone()),
        run_kind: Some(run.kind.clone()),
        requester_agent_id: None,
        requester_session_id: None,
        requester_run_id: None,
        requester_tool_call_id: None,
        requester_project_ids: Vec::new(),
        requester_channel_ids: Vec::new(),
        parent_project_ids: Vec::new(),
        parent_channel_ids: Vec::new(),
        request,
    })
}

async fn prompt_question_answers(
    request: kheish_types::UserQuestionRequest,
) -> Result<Vec<kheish_types::UserQuestionAnswer>> {
    tokio::task::spawn_blocking(move || prompt_question_answers_blocking(&request))
        .await
        .context("interactive question prompt task panicked")?
}

fn prompt_question_answers_blocking(
    request: &kheish_types::UserQuestionRequest,
) -> Result<Vec<kheish_types::UserQuestionAnswer>> {
    request
        .questions
        .iter()
        .map(prompt_one_question_answer)
        .collect()
}

fn prompt_one_question_answer(
    question: &kheish_types::UserQuestion,
) -> Result<kheish_types::UserQuestionAnswer> {
    eprintln!("{}", question.header);
    eprintln!("{}", question.question);
    for (index, option) in question.options.iter().enumerate() {
        if let Some(description) = &option.description {
            eprintln!("  {}. {} - {}", index + 1, option.label, description);
        } else {
            eprintln!("  {}. {}", index + 1, option.label);
        }
    }
    loop {
        if question.multi_select {
            eprint!("Selection(s), comma-separated: ");
        } else {
            eprint!("Selection: ");
        }
        std::io::stderr()
            .flush()
            .context("failed to flush prompt")?;
        let selection = read_prompt_line()?;
        let selected_option_ids = match parse_selected_options(question, selection.trim()) {
            Ok(selected_option_ids) => selected_option_ids,
            Err(error) => {
                eprintln!("Invalid selection: {error}");
                continue;
            }
        };

        eprint!("Freeform answer (optional): ");
        std::io::stderr()
            .flush()
            .context("failed to flush prompt")?;
        let freeform = read_prompt_line()?;
        let freeform_answer = freeform.trim().to_string();
        if selected_option_ids.is_empty() && freeform_answer.is_empty() {
            eprintln!("Question {} requires at least one answer", question.id);
            continue;
        }

        return Ok(kheish_types::UserQuestionAnswer {
            question_id: question.id.clone(),
            selected_option_ids,
            freeform_answer: (!freeform_answer.is_empty()).then_some(freeform_answer),
        });
    }
}

fn read_prompt_line() -> Result<String> {
    let mut value = String::new();
    let bytes_read = std::io::stdin()
        .read_line(&mut value)
        .context("failed to read interactive answer")?;
    if bytes_read == 0 {
        bail!("interactive answer input ended before a response was provided");
    }
    Ok(value)
}

fn parse_selected_options(
    question: &kheish_types::UserQuestion,
    raw_selection: &str,
) -> Result<Vec<String>> {
    if raw_selection.is_empty() {
        return Ok(Vec::new());
    }
    let tokens = if raw_selection.contains(',') {
        raw_selection
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>()
    } else {
        raw_selection
            .split_whitespace()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>()
    };
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    if !question.multi_select && tokens.len() > 1 {
        bail!("question {} allows only one option", question.id);
    }

    let mut seen = BTreeSet::new();
    let mut selected = Vec::with_capacity(tokens.len());
    for token in tokens {
        let option_id = resolve_option_token(question, token)?;
        if !seen.insert(option_id.clone()) {
            bail!(
                "duplicate option {} for question {}",
                option_id,
                question.id
            );
        }
        selected.push(option_id);
    }
    Ok(selected)
}

fn resolve_option_token(question: &kheish_types::UserQuestion, token: &str) -> Result<String> {
    if let Ok(index) = token.parse::<usize>() {
        if let Some(option) = index
            .checked_sub(1)
            .and_then(|index| question.options.get(index))
        {
            return Ok(option.id.clone());
        }
    }
    question
        .options
        .iter()
        .find(|option| option.id == token || option.label.eq_ignore_ascii_case(token))
        .map(|option| option.id.clone())
        .ok_or_else(|| anyhow!("unknown option {token} for question {}", question.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kheish_types::{UserQuestion, UserQuestionOption};

    fn question(multi_select: bool) -> UserQuestion {
        UserQuestion {
            id: "focus".to_string(),
            header: "Focus".to_string(),
            question: "Which focus?".to_string(),
            options: vec![
                UserQuestionOption {
                    id: "latency".to_string(),
                    label: "Latency".to_string(),
                    description: None,
                    preview: None,
                },
                UserQuestionOption {
                    id: "throughput".to_string(),
                    label: "Throughput".to_string(),
                    description: None,
                    preview: None,
                },
            ],
            multi_select,
        }
    }

    #[test]
    fn parses_interactive_option_selection_tokens() -> Result<()> {
        assert_eq!(
            parse_selected_options(&question(false), "1")?,
            vec!["latency"]
        );
        assert_eq!(
            parse_selected_options(&question(false), "throughput")?,
            vec!["throughput"]
        );
        assert_eq!(
            parse_selected_options(&question(false), "Latency")?,
            vec!["latency"]
        );
        assert_eq!(
            parse_selected_options(&question(true), "latency, 2")?,
            vec!["latency", "throughput"]
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_interactive_option_selection_tokens() {
        let duplicate = parse_selected_options(&question(true), "latency, 1")
            .expect_err("duplicate option should fail");
        assert!(duplicate.to_string().contains("duplicate option latency"));

        let unknown =
            parse_selected_options(&question(true), "memory").expect_err("unknown option fails");
        assert!(unknown.to_string().contains("unknown option memory"));

        let single_select = parse_selected_options(&question(false), "1 2")
            .expect_err("single-select question should reject multiple tokens");
        assert!(single_select.to_string().contains("allows only one option"));
    }
}
