//! Channel command handlers.

use anyhow::Result;
use serde_json::Value;

/// Handles `channels ...`.
pub(crate) async fn run_channels_command(
    client: &crate::cli::DaemonHttpClient,
    printer: &crate::cli::Printer,
    command: crate::ChannelsCommand,
) -> Result<()> {
    match command {
        crate::ChannelsCommand::List { query } => {
            let channels = client
                .get_json_with_query::<_, Vec<kheish_daemon::ChannelView>>(
                    "/v1/channels",
                    &kheish_daemon::ChannelListQuery { query },
                )
                .await?;
            printer.print(&channels)
        }
        crate::ChannelsCommand::Get { channel_id } => {
            let channel_id = crate::cli::url_encode_path_segment(&channel_id);
            let channel = client
                .get_json::<kheish_daemon::ChannelView>(&format!("/v1/channels/{channel_id}"))
                .await?;
            printer.print(&channel)
        }
        crate::ChannelsCommand::Create(args) => {
            let autonomy_policy = read_channel_autonomy_policy(&args.autonomy_policy).await?;
            let members = crate::cli::read_optional_json_input(
                args.members_json.as_deref(),
                args.members_file.as_deref(),
            )
            .await?
            .map(serde_json::from_value::<Vec<kheish_daemon::ChannelMemberRequest>>)
            .transpose()?
            .unwrap_or_default();
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?
            .unwrap_or(Value::Null);
            let channel = client
                .post_json::<_, kheish_daemon::ChannelView>(
                    "/v1/channels",
                    &kheish_daemon::CreateChannelRequest {
                        channel_id: args.channel_id,
                        title: args.title,
                        description: args.description,
                        purpose: args.purpose,
                        pinned_asset_ids: args.pinned_asset_ids,
                        created_by: args.created_by,
                        members,
                        autonomy_policy,
                        default_participation_mode: args.default_participation_mode.map(Into::into),
                        metadata,
                    },
                )
                .await?;
            printer.print(&channel)
        }
        crate::ChannelsCommand::Update(args) => {
            let autonomy_policy = read_channel_autonomy_policy(&args.autonomy_policy).await?;
            let metadata = crate::cli::read_optional_json_input(
                args.metadata_json.as_deref(),
                args.metadata_file.as_deref(),
            )
            .await?;
            let channel_id = crate::cli::url_encode_path_segment(&args.channel_id);
            let channel = client
                .put_json::<_, kheish_daemon::ChannelView>(
                    &format!("/v1/channels/{channel_id}"),
                    &kheish_daemon::UpdateChannelRequest {
                        title: args.title,
                        description: args.description,
                        purpose: args.purpose,
                        pinned_asset_ids: args
                            .replace_pinned_assets
                            .then_some(args.pinned_asset_ids),
                        autonomy_policy,
                        default_participation_mode: args.default_participation_mode.map(Into::into),
                        paused: args.paused,
                        metadata,
                    },
                )
                .await?;
            printer.print(&channel)
        }
        crate::ChannelsCommand::Delete { channel_id } => {
            let channel_id = crate::cli::url_encode_path_segment(&channel_id);
            let response = client
                .delete_json::<Value>(&format!("/v1/channels/{channel_id}"))
                .await?;
            printer.print(&response)
        }
        crate::ChannelsCommand::Members { command } => match command {
            crate::ChannelMembersCommand::List { channel_id } => {
                let channel_id = crate::cli::url_encode_path_segment(&channel_id);
                let members = client
                    .get_json::<Vec<kheish_daemon::ChannelMemberView>>(&format!(
                        "/v1/channels/{channel_id}/members"
                    ))
                    .await?;
                printer.print(&members)
            }
            crate::ChannelMembersCommand::Upsert(args) => {
                let channel_id = crate::cli::url_encode_path_segment(&args.channel_id);
                let channel = client
                    .post_json::<_, kheish_daemon::ChannelView>(
                        &format!("/v1/channels/{channel_id}/members"),
                        &kheish_daemon::ChannelMemberRequest {
                            member_id: args.member_id,
                            member_kind: args.member_kind.into(),
                            display_name_mode: args.display_name_mode.map(Into::into),
                            display_name: args.display_name,
                            session_id: args.session_id,
                            actor_id: args.actor_id,
                            role: args.role,
                            expertise_tags: args.expertise_tags,
                            participation_mode: args.participation_mode.map(Into::into),
                            muted: args.muted,
                        },
                    )
                    .await?;
                printer.print(&channel)
            }
            crate::ChannelMembersCommand::Remove {
                channel_id,
                member_id,
            } => {
                let channel_id = crate::cli::url_encode_path_segment(&channel_id);
                let member_id = crate::cli::url_encode_path_segment(&member_id);
                let channel = client
                    .delete_json::<kheish_daemon::ChannelView>(&format!(
                        "/v1/channels/{channel_id}/members/{member_id}"
                    ))
                    .await?;
                printer.print(&channel)
            }
        },
        crate::ChannelsCommand::Messages { command } => match command {
            crate::ChannelMessagesCommand::List {
                channel_id,
                thread_root_message_id,
                query,
                limit,
            } => {
                let channel_id = crate::cli::url_encode_path_segment(&channel_id);
                let messages = client
                    .get_json_with_query::<_, Vec<kheish_daemon::ChannelMessageView>>(
                        &format!("/v1/channels/{channel_id}/messages"),
                        &kheish_daemon::ChannelMessageListQuery {
                            thread_root_message_id,
                            query,
                            limit,
                        },
                    )
                    .await?;
                printer.print(&messages)
            }
            crate::ChannelMessagesCommand::Post(args) => {
                let input_items = crate::cli::read_optional_json_input(
                    args.input_items_json.as_deref(),
                    args.input_items_file.as_deref(),
                )
                .await?
                .map(serde_json::from_value::<Vec<kheish_daemon::SubmitInputItemRequest>>)
                .transpose()?
                .unwrap_or_default();
                let metadata = crate::cli::read_optional_json_input(
                    args.metadata_json.as_deref(),
                    args.metadata_file.as_deref(),
                )
                .await?
                .unwrap_or(Value::Null);
                let content = if args.content.is_empty() {
                    None
                } else {
                    Some(args.content.join(" "))
                };
                let channel_id = crate::cli::url_encode_path_segment(&args.channel_id);
                let message = client
                    .post_json::<_, kheish_daemon::ChannelMessageView>(
                        &format!("/v1/channels/{channel_id}/messages"),
                        &kheish_daemon::PostChannelMessageRequest {
                            sender_actor_id: args.sender_actor_id,
                            sender_display_name: args.sender_display_name,
                            sender_session_id: args.sender_session_id,
                            thread_root_message_id: args.thread_root_message_id,
                            reply_to_message_id: args.reply_to_message_id,
                            addressed_member_ids: args.addressed_member_ids,
                            input_items,
                            content,
                            metadata,
                        },
                    )
                    .await?;
                printer.print(&message)
            }
            crate::ChannelMessagesCommand::React(args) => {
                let channel_id = crate::cli::url_encode_path_segment(&args.channel_id);
                let message_id = crate::cli::url_encode_path_segment(&args.message_id);
                let message = client
                    .post_json::<_, kheish_daemon::ChannelMessageView>(
                        &format!("/v1/channels/{channel_id}/messages/{message_id}/reactions"),
                        &kheish_daemon::SetChannelReactionRequest {
                            actor_id: args.actor_id,
                            emoji: args.emoji,
                        },
                    )
                    .await?;
                printer.print(&message)
            }
            crate::ChannelMessagesCommand::Unreact(args) => {
                let channel_id = crate::cli::url_encode_path_segment(&args.channel_id);
                let message_id = crate::cli::url_encode_path_segment(&args.message_id);
                let message = client
                    .delete_json_with_body::<_, kheish_daemon::ChannelMessageView>(
                        &format!("/v1/channels/{channel_id}/messages/{message_id}/reactions"),
                        &kheish_daemon::SetChannelReactionRequest {
                            actor_id: args.actor_id,
                            emoji: args.emoji,
                        },
                    )
                    .await?;
                printer.print(&message)
            }
        },
        crate::ChannelsCommand::Stimuli { command } => match command {
            crate::ChannelStimuliCommand::List {
                channel_id,
                thread_root_message_id,
                state,
                limit,
            } => {
                let channel_id = crate::cli::url_encode_path_segment(&channel_id);
                let stimuli = client
                    .get_json_with_query::<_, Vec<kheish_daemon::ChannelStimulusView>>(
                        &format!("/v1/channels/{channel_id}/stimuli"),
                        &kheish_daemon::ChannelStimulusListQuery {
                            thread_root_message_id,
                            state: state.map(Into::into),
                            limit,
                        },
                    )
                    .await?;
                printer.print(&stimuli)
            }
            crate::ChannelStimuliCommand::Create(args) => {
                let metadata = crate::cli::read_optional_json_input(
                    args.metadata_json.as_deref(),
                    args.metadata_file.as_deref(),
                )
                .await?
                .unwrap_or(Value::Null);
                let content = args.content.join(" ");
                let channel_id = crate::cli::url_encode_path_segment(&args.channel_id);
                let stimulus = client
                    .post_json::<_, kheish_daemon::ChannelStimulusView>(
                        &format!("/v1/channels/{channel_id}/stimuli"),
                        &kheish_daemon::CreateChannelStimulusRequest {
                            scope: args.scope.into(),
                            thread_root_message_id: args.thread_root_message_id,
                            kind: args.kind.into(),
                            visibility_hint: args.visibility_hint.map(Into::into),
                            content,
                            addressed_member_ids: args.addressed_member_ids,
                            sender_session_id: args.sender_session_id,
                            sender_actor_id: args.sender_actor_id,
                            sender_display_name: args.sender_display_name,
                            source_kind: args.source_kind,
                            source_ref: args.source_ref,
                            dedupe_key: args.dedupe_key,
                            progress_key: args.progress_key,
                            available_at_ms: args.available_at_ms,
                            expires_at_ms: args.expires_at_ms,
                            metadata,
                        },
                    )
                    .await?;
                printer.print(&stimulus)
            }
        },
        crate::ChannelsCommand::ThreadWork {
            channel_id,
            thread_root_message_id,
        } => {
            let channel_id = crate::cli::url_encode_path_segment(&channel_id);
            let thread_work = client
                .get_json_with_query::<_, Vec<kheish_daemon::ChannelThreadWorkStateView>>(
                    &format!("/v1/channels/{channel_id}/thread-work"),
                    &kheish_daemon::ChannelThreadWorkListQuery {
                        thread_root_message_id,
                    },
                )
                .await?;
            printer.print(&thread_work)
        }
        crate::ChannelsCommand::Leases { channel_id } => {
            let channel_id = crate::cli::url_encode_path_segment(&channel_id);
            let leases = client
                .get_json::<Vec<kheish_daemon::ChannelTurnLeaseView>>(&format!(
                    "/v1/channels/{channel_id}/leases"
                ))
                .await?;
            printer.print(&leases)
        }
    }
}

async fn read_channel_autonomy_policy(
    args: &crate::ChannelAutonomyPolicyArgs,
) -> Result<Option<kheish_daemon::ChannelAutonomyPolicy>> {
    let mut policy = crate::cli::read_optional_json_input(
        args.autonomy_policy_json.as_deref(),
        args.autonomy_policy_file.as_deref(),
    )
    .await?
    .map(serde_json::from_value::<kheish_daemon::ChannelAutonomyPolicy>)
    .transpose()?;
    if args.max_parallel_public_speakers.is_some()
        || args.max_agent_replies_per_human_message.is_some()
        || args.member_cooldown_ms.is_some()
        || args.lease_timeout_ms.is_some()
        || args.max_pending_stimuli.is_some()
        || args.max_autonomous_root_posts_per_hour.is_some()
        || args.max_active_autonomous_roots.is_some()
        || args.quiet_period_ms_after_root_post.is_some()
    {
        let mut merged = policy.unwrap_or_default();
        if let Some(value) = args.max_parallel_public_speakers {
            merged.max_parallel_public_speakers = value;
        }
        if let Some(value) = args.max_agent_replies_per_human_message {
            merged.max_agent_replies_per_human_message = value;
        }
        if let Some(value) = args.member_cooldown_ms {
            merged.member_cooldown_ms = value;
        }
        if let Some(value) = args.lease_timeout_ms {
            merged.lease_timeout_ms = value;
        }
        if let Some(value) = args.max_pending_stimuli {
            merged.max_pending_stimuli = value;
        }
        if let Some(value) = args.max_autonomous_root_posts_per_hour {
            merged.max_autonomous_root_posts_per_hour = value;
        }
        if let Some(value) = args.max_active_autonomous_roots {
            merged.max_active_autonomous_roots = value;
        }
        if let Some(value) = args.quiet_period_ms_after_root_post {
            merged.quiet_period_ms_after_root_post = value;
        }
        policy = Some(merged);
    }
    Ok(policy)
}
