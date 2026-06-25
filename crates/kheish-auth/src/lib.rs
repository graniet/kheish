mod backends;
mod broker;
mod manager;
mod oauth;
mod redaction;
mod store;
mod types;

pub use backends::{
    AnthropicAuthBackend, AuthBackend, DEFAULT_ANTHROPIC_OAUTH_TOKEN_URL,
    DEFAULT_CLAUDE_CODE_CLIENT_ID, DEFAULT_CODEX_CLIENT_ID, DEFAULT_OPENAI_AUTH_ISSUER,
    DEFAULT_OPENAI_CODEX_API_BASE_URL, GenericAuthBackend, GoogleAuthBackend,
    McpOAuthAccountRecordInput, McpOAuthAuthBackend, McpOAuthStoredState, OpenAiAuthBackend,
    OpenRouterAuthBackend, XAiAuthBackend, default_claude_code_credentials_path,
};
pub use broker::{
    AuthSubject, AuthSubjectKind, AuthSubjectStatus, CredentialBroker, CredentialGrant,
    CredentialLease, CredentialLeaseAudience, CredentialLeaseStatus,
    DEFAULT_CONNECTOR_LEASE_TTL_MS, DEFAULT_ROUTE_LEASE_TTL_MS, ExecutionCredentialContext,
};
pub use manager::{AuthManager, ManagedRequestAuthProvider, RequestAuthProvider};
pub use oauth::{
    McpOAuthAuthorizationRequest, McpOAuthDiscovery, McpOAuthTokenSet,
    build_mcp_oauth_authorization_request, canonical_resource_url, discover_mcp_oauth,
    dynamic_register_mcp_oauth_client, exchange_mcp_oauth_code, mcp_oauth_account_input,
    pkce_s256_challenge, random_urlsafe,
};
pub use redaction::{
    debug_redaction_tokens, register_ephemeral_debug_redaction_token,
    replace_auth_store_debug_redaction_tokens,
};
pub use store::{
    AUTH_STORE_MASTER_KEY_ENV, AUTH_STORE_MASTER_KEY_FILE_ENV, FileAuthStore,
    generate_auth_store_master_key_base64, load_auth_store_master_key_from_env,
    parse_auth_store_master_key,
};
pub use types::{
    AuthMode, AuthProvider, AuthSlotId, AuthSlotRecord, AuthSlotStatus, AuthStoreSnapshot,
    ResolvedAuthMaterial,
};

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use anyhow::Result;
    use axum::{Json, Router, body::Bytes, extract::State, routing::post};
    use serde_json::json;
    use tempfile::tempdir;
    use tokio::net::TcpListener;

    use crate::{
        AUTH_STORE_MASTER_KEY_ENV, AuthManager, AuthSlotId, ExecutionCredentialContext,
        store::auth_store_env_lock,
    };

    fn ensure_auth_store_master_key() -> std::sync::MutexGuard<'static, ()> {
        let guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        guard
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_store_round_trip_static_api_key() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let status = manager
            .store_openai_api_key(
                AuthSlotId::new("openai-default"),
                "sk-test",
                Some("org-test".to_string()),
                Some("proj-test".to_string()),
            )
            .await?;
        assert_eq!(status.summary, "api_key");

        let resolved = manager
            .resolve(&AuthSlotId::new("openai-default"), false)
            .await?;
        let expected_authorization = format!("Bearer {}{}", "sk-", "test");
        assert_eq!(
            resolved.headers.get("Authorization"),
            Some(&expected_authorization)
        );
        assert_eq!(
            resolved.headers.get("OpenAI-Organization"),
            Some(&"org-test".to_string())
        );
        assert_eq!(
            resolved.headers.get("OpenAI-Project"),
            Some(&"proj-test".to_string())
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolve_brokered_attaches_route_grant_id_to_material() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        manager
            .store_openai_api_key(AuthSlotId::new("openai-default"), "sk-test", None, None)
            .await?;

        let material = manager
            .resolve_brokered(
                &AuthSlotId::new("openai-default"),
                "primary-openai",
                &ExecutionCredentialContext {
                    session_id: Some("session-1".to_string()),
                    agent_id: Some("agent-1".to_string()),
                    principal_id: Some("agent:agent-1".to_string()),
                    ..ExecutionCredentialContext::default()
                },
                false,
            )
            .await?;

        assert!(material.grant_id.is_some());
        let lease_id = material.lease_id.as_deref().expect("route lease id");
        manager.ensure_resolved_material_active(&material)?;
        assert_eq!(
            manager
                .subject_status("agent:agent-1")
                .map(|status| (status.revoked, status.active_route_lease_ids.is_empty())),
            Some((false, false))
        );
        manager.revoke_lease(lease_id, crate::now_ms().saturating_add(60_000))?;
        let error = manager
            .ensure_resolved_material_active(&material)
            .expect_err("revoked route lease should block use");
        assert!(error.to_string().contains("not active"));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retired_auth_store_tokens_remain_debug_redacted_after_rotation_and_delete()
    -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let slot = AuthSlotId::new("sidecars.webhook.routes");
        manager
            .store_generic_secret(slot.clone(), "old-opaque-route-canary")
            .await?;
        manager
            .store_generic_secret(slot.clone(), "new-opaque-route-canary")
            .await?;
        manager.delete(&slot).await?;

        let tokens = crate::debug_redaction_tokens();
        assert!(tokens.contains(&"old-opaque-route-canary".to_string()));
        assert!(tokens.contains(&"new-opaque-route-canary".to_string()));
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolve_mcp_brokered_enforces_slot_binding_and_scope() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        manager
            .store_mcp_oauth_account(crate::McpOAuthAccountRecordInput {
                slot_id: AuthSlotId::new("mcp.oauth.notion"),
                server_name: "notion".to_string(),
                resource: "https://mcp.notion.com/mcp".to_string(),
                issuer: "https://auth.example.test".to_string(),
                authorization_endpoint: "https://auth.example.test/authorize".to_string(),
                token_endpoint: "https://auth.example.test/token".to_string(),
                client_id: "client-test".to_string(),
                client_secret: None,
                access_token: "access-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                expires_at_ms: Some(crate::now_ms().saturating_add(3_600_000)),
                scopes: vec!["read".to_string()],
            })
            .await?;
        let context = ExecutionCredentialContext {
            session_id: Some("session-1".to_string()),
            credential_scope: kheish_types::CredentialScope {
                mcp_server_allow: vec!["notion".to_string()],
                ..Default::default()
            },
            ..ExecutionCredentialContext::default()
        };

        let material = manager
            .resolve_mcp_brokered(
                &AuthSlotId::new("mcp.oauth.notion"),
                "notion",
                "https://mcp.notion.com/mcp",
                &["read".to_string()],
                &context,
                false,
            )
            .await?;
        assert_eq!(
            material.headers.get("Authorization"),
            Some(&"Bearer access-token".to_string())
        );
        assert!(material.grant_id.is_some());
        let status = manager
            .subject_status("session:session-1")
            .expect("subject status");
        assert!(status.active_route_lease_ids.is_empty());
        assert!(!status.active_mcp_lease_ids.is_empty());

        let server_error = manager
            .resolve_mcp_brokered(
                &AuthSlotId::new("mcp.oauth.notion"),
                "slack",
                "https://mcp.notion.com/mcp",
                &["read".to_string()],
                &context,
                false,
            )
            .await
            .expect_err("server mismatch should fail");
        assert!(server_error.to_string().contains("bound to server"));

        let scope_error = manager
            .resolve_mcp_brokered(
                &AuthSlotId::new("mcp.oauth.notion"),
                "notion",
                "https://mcp.notion.com/mcp",
                &["write".to_string()],
                &context,
                false,
            )
            .await
            .expect_err("scope escalation should fail");
        assert!(
            scope_error
                .to_string()
                .contains("does not include requested scope")
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replacing_or_deleting_slots_revokes_active_route_and_mcp_leases() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let route_slot = AuthSlotId::new("openai-default");
        manager
            .store_openai_api_key(route_slot.clone(), "sk-old", None, None)
            .await?;

        let route_context = ExecutionCredentialContext {
            session_id: Some("session-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            principal_id: Some("agent:agent-1".to_string()),
            ..ExecutionCredentialContext::default()
        };
        manager
            .resolve_brokered(&route_slot, "primary-openai", &route_context, false)
            .await?;
        let old_route_lease_id = manager
            .subject_status("agent:agent-1")
            .expect("route subject")
            .active_route_lease_ids
            .first()
            .expect("active route lease")
            .clone();

        manager
            .store_openai_api_key(route_slot.clone(), "sk-new", None, None)
            .await?;
        let old_route_status = manager
            .lease_status(&old_route_lease_id)
            .expect("old route lease status");
        assert!(old_route_status.revoked);
        assert!(!old_route_status.active);

        manager
            .resolve_brokered(&route_slot, "primary-openai", &route_context, false)
            .await?;
        let new_route_lease_id = manager
            .subject_status("agent:agent-1")
            .expect("route subject after rotation")
            .active_route_lease_ids
            .first()
            .expect("new active route lease")
            .clone();
        assert_ne!(old_route_lease_id, new_route_lease_id);

        assert!(manager.delete(&route_slot).await?);
        let new_route_status = manager
            .lease_status(&new_route_lease_id)
            .expect("new route lease status");
        assert!(new_route_status.revoked);
        assert!(!new_route_status.active);

        let mcp_slot = AuthSlotId::new("mcp.oauth.notion");
        manager
            .store_mcp_oauth_account(crate::McpOAuthAccountRecordInput {
                slot_id: mcp_slot.clone(),
                server_name: "notion".to_string(),
                resource: "https://mcp.notion.com/mcp".to_string(),
                issuer: "https://auth.example.test".to_string(),
                authorization_endpoint: "https://auth.example.test/authorize".to_string(),
                token_endpoint: "https://auth.example.test/token".to_string(),
                client_id: "client-test".to_string(),
                client_secret: None,
                access_token: "access-token".to_string(),
                refresh_token: Some("refresh-token".to_string()),
                expires_at_ms: Some(crate::now_ms().saturating_add(3_600_000)),
                scopes: vec!["read".to_string()],
            })
            .await?;
        let mcp_context = ExecutionCredentialContext {
            session_id: Some("session-2".to_string()),
            credential_scope: kheish_types::CredentialScope {
                mcp_server_allow: vec!["notion".to_string()],
                ..Default::default()
            },
            ..ExecutionCredentialContext::default()
        };
        manager
            .resolve_mcp_brokered(
                &mcp_slot,
                "notion",
                "https://mcp.notion.com/mcp",
                &["read".to_string()],
                &mcp_context,
                false,
            )
            .await?;
        let mcp_lease_id = manager
            .subject_status("session:session-2")
            .expect("mcp subject")
            .active_mcp_lease_ids
            .first()
            .expect("active mcp lease")
            .clone();

        assert!(manager.delete(&mcp_slot).await?);
        let mcp_status = manager
            .lease_status(&mcp_lease_id)
            .expect("mcp lease status");
        assert!(mcp_status.revoked);
        assert!(!mcp_status.active);

        let connector_slot = AuthSlotId::new("sidecars.webhook.routes");
        manager
            .store_generic_secret(connector_slot.clone(), r#"{"demo":true}"#)
            .await?;
        let (connector_token, connector_lease) = manager.issue_connector_lease(
            "webhook",
            &[String::from("WEBHOOK_ROUTES_JSON")],
            std::slice::from_ref(&connector_slot),
            None,
            None,
        )?;
        manager
            .validate_connector_lease(&connector_token, "webhook", "WEBHOOK_ROUTES_JSON")
            .expect("connector lease should validate before slot rotation");
        manager
            .store_generic_secret(connector_slot, r#"{"demo":false}"#)
            .await?;
        let connector_status = manager
            .lease_status(&connector_lease.id)
            .expect("connector lease status");
        assert!(connector_status.revoked);
        assert!(!connector_status.active);
        let connector_error = manager
            .validate_connector_lease(&connector_token, "webhook", "WEBHOOK_ROUTES_JSON")
            .expect_err("connector lease should be rejected after slot rotation");
        assert!(connector_error.to_string().contains("revoked"));

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revoked_slots_fail_closed_until_reprovisioned() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let route_slot = AuthSlotId::new("openai-default");
        manager
            .store_openai_api_key(route_slot.clone(), "sk-live", None, None)
            .await?;
        let route_context = ExecutionCredentialContext {
            session_id: Some("session-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            principal_id: Some("agent:agent-1".to_string()),
            ..ExecutionCredentialContext::default()
        };
        manager
            .resolve_brokered(&route_slot, "primary-openai", &route_context, false)
            .await?;

        manager.revoke_slot_leases(&route_slot)?;
        let route_error = manager
            .resolve_brokered(&route_slot, "primary-openai", &route_context, false)
            .await
            .expect_err("revoked route slot should fail closed");
        assert!(route_error.to_string().contains("has been revoked"));

        manager
            .store_openai_api_key(route_slot.clone(), "sk-reprovisioned", None, None)
            .await?;
        manager
            .resolve_brokered(&route_slot, "primary-openai", &route_context, false)
            .await?;

        let mcp_slot = AuthSlotId::new("mcp.oauth.notion");
        manager
            .store_mcp_oauth_account(crate::McpOAuthAccountRecordInput {
                slot_id: mcp_slot.clone(),
                server_name: "notion".to_string(),
                resource: "https://mcp.notion.com/mcp".to_string(),
                issuer: "https://auth.example.test".to_string(),
                authorization_endpoint: "https://auth.example.test/authorize".to_string(),
                token_endpoint: "https://auth.example.test/token".to_string(),
                client_id: "client-test".to_string(),
                client_secret: None,
                access_token: "access-token".to_string(),
                refresh_token: None,
                expires_at_ms: Some(crate::now_ms().saturating_add(3_600_000)),
                scopes: vec!["read".to_string()],
            })
            .await?;
        let mcp_context = ExecutionCredentialContext {
            session_id: Some("mcp-session".to_string()),
            credential_scope: kheish_types::CredentialScope {
                mcp_server_allow: vec!["notion".to_string()],
                ..Default::default()
            },
            ..ExecutionCredentialContext::default()
        };
        manager
            .resolve_mcp_brokered(
                &mcp_slot,
                "notion",
                "https://mcp.notion.com/mcp",
                &["read".to_string()],
                &mcp_context,
                false,
            )
            .await?;
        manager.revoke_slot_leases(&mcp_slot)?;
        let mcp_error = manager
            .resolve_mcp_brokered(
                &mcp_slot,
                "notion",
                "https://mcp.notion.com/mcp",
                &["read".to_string()],
                &mcp_context,
                false,
            )
            .await
            .expect_err("revoked MCP OAuth slot should fail closed");
        assert!(mcp_error.to_string().contains("has been revoked"));
        manager
            .store_mcp_oauth_account(crate::McpOAuthAccountRecordInput {
                slot_id: mcp_slot.clone(),
                server_name: "notion".to_string(),
                resource: "https://mcp.notion.com/mcp".to_string(),
                issuer: "https://auth.example.test".to_string(),
                authorization_endpoint: "https://auth.example.test/authorize".to_string(),
                token_endpoint: "https://auth.example.test/token".to_string(),
                client_id: "client-test".to_string(),
                client_secret: None,
                access_token: "access-token-2".to_string(),
                refresh_token: None,
                expires_at_ms: Some(crate::now_ms().saturating_add(3_600_000)),
                scopes: vec!["read".to_string()],
            })
            .await?;
        manager
            .resolve_mcp_brokered(
                &mcp_slot,
                "notion",
                "https://mcp.notion.com/mcp",
                &["read".to_string()],
                &mcp_context,
                false,
            )
            .await?;

        let connector_slot = AuthSlotId::new("sidecars.webhook.routes");
        manager
            .store_generic_secret(connector_slot.clone(), r#"{"demo":true}"#)
            .await?;
        manager.revoke_slot_leases(&connector_slot)?;
        let secret_error = manager
            .secret_value(&connector_slot)
            .expect_err("revoked generic secret should not resolve");
        assert!(secret_error.to_string().contains("has been revoked"));
        let connector_error = manager
            .issue_connector_lease(
                "webhook",
                &[String::from("WEBHOOK_ROUTES_JSON")],
                std::slice::from_ref(&connector_slot),
                None,
                None,
            )
            .expect_err("revoked connector secret should not receive a lease");
        assert!(connector_error.to_string().contains("has been revoked"));

        manager
            .store_generic_secret(connector_slot.clone(), r#"{"demo":false}"#)
            .await?;
        assert_eq!(
            manager.secret_value(&connector_slot)?,
            Some(r#"{"demo":false}"#.to_string())
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_oauth_refresh_rejects_broader_scopes() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        async fn oauth_token() -> Json<serde_json::Value> {
            Json(json!({
                "access_token": "fresh-access-token",
                "refresh_token": "fresh-refresh-token",
                "expires_in": 3600,
                "scope": "read write",
                "token_type": "Bearer"
            }))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new().route("/oauth/token", post(oauth_token));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        manager
            .store_mcp_oauth_account(crate::McpOAuthAccountRecordInput {
                slot_id: AuthSlotId::new("mcp.oauth.test"),
                server_name: "test".to_string(),
                resource: "http://127.0.0.1/mcp".to_string(),
                issuer: format!("http://{address}"),
                authorization_endpoint: format!("http://{address}/oauth/authorize"),
                token_endpoint: format!("http://{address}/oauth/token"),
                client_id: "client-test".to_string(),
                client_secret: Some("client-secret".to_string()),
                access_token: "stale-access-token".to_string(),
                refresh_token: Some("stale-refresh-token".to_string()),
                expires_at_ms: Some(1),
                scopes: vec!["read".to_string()],
            })
            .await?;
        let error = manager
            .resolve(&AuthSlotId::new("mcp.oauth.test"), false)
            .await
            .expect_err("broader refresh scope should fail");
        assert!(error.to_string().contains("unapproved scope `write`"));
        let status = manager
            .status(&AuthSlotId::new("mcp.oauth.test"))
            .await?
            .expect("status");
        assert_eq!(status.details.get("scopes"), Some(&json!(["read"])));
        server.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auth_manager_put_and_delete_leave_memory_unchanged_when_persist_fails() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        manager
            .store_generic_secret(AuthSlotId::new("stable-slot"), "stable-secret")
            .await?;

        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
        }

        let put_error = manager
            .store_generic_secret(AuthSlotId::new("transient-slot"), "transient-secret")
            .await
            .expect_err("writes should fail without a master key");
        let put_error_text = format!("{put_error:#}");
        assert!(
            put_error_text.contains(AUTH_STORE_MASTER_KEY_ENV),
            "unexpected error: {put_error:#}"
        );
        assert!(!manager.has_slot(&AuthSlotId::new("transient-slot")).await);

        let delete_error = manager
            .delete(&AuthSlotId::new("stable-slot"))
            .await
            .expect_err("deletes should fail without a master key");
        let delete_error_text = format!("{delete_error:#}");
        assert!(
            delete_error_text.contains(AUTH_STORE_MASTER_KEY_ENV),
            "unexpected error: {delete_error:#}"
        );
        assert!(manager.has_slot(&AuthSlotId::new("stable-slot")).await);
        assert_eq!(
            manager.secret_value(&AuthSlotId::new("stable-slot"))?,
            Some("stable-secret".to_string())
        );

        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_import_refreshes_tokens_and_resolves_chatgpt_bearer() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        async fn oauth_token(
            headers: axum::http::HeaderMap,
            body: Bytes,
        ) -> Json<serde_json::Value> {
            let content_type = headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            if content_type.contains("application/json") {
                let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                if payload
                    .get("grant_type")
                    .and_then(serde_json::Value::as_str)
                    == Some("refresh_token")
                {
                    return Json(json!({
                        "access_token": "fresh-access-token",
                        "refresh_token": "fresh-refresh-token"
                    }));
                }
            }
            panic!(
                "unexpected OpenAI auth request: {:?}",
                String::from_utf8(body.to_vec())
            );
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new().route("/oauth/token", post(oauth_token));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let temp = tempdir()?;
        let codex_auth_path = temp.path().join("codex-auth.json");
        std::fs::write(
            &codex_auth_path,
            serde_json::to_string_pretty(&json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "refresh_token": "stale-refresh-token",
                    "account_id": "acc-test"
                }
            }))?,
        )?;

        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let record = crate::OpenAiAuthBackend::import_codex_record_with_overrides(
            AuthSlotId::new("openai-codex"),
            &codex_auth_path,
            format!("http://{address}"),
            "client-test",
            "http://example.test/backend-api/codex/responses",
            None,
            None,
        )?;
        manager.put_record(record).await?;

        let resolved = manager
            .resolve(&AuthSlotId::new("openai-codex"), true)
            .await?;
        assert_eq!(
            resolved.headers.get("Authorization"),
            Some(&"Bearer fresh-access-token".to_string())
        );
        assert_eq!(
            resolved.headers.get("ChatGPT-Account-ID"),
            Some(&"acc-test".to_string())
        );
        assert_eq!(
            resolved.base_url_override.as_deref(),
            Some("http://example.test/backend-api/codex/responses")
        );
        server.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn codex_import_uses_existing_access_token_without_refresh() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let codex_auth_path = temp.path().join("codex-auth.json");
        std::fs::write(
            &codex_auth_path,
            serde_json::to_string_pretty(&json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "access_token": "existing-access-token",
                    "refresh_token": "stale-refresh-token",
                    "account_id": "acc-existing"
                }
            }))?,
        )?;

        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let record = crate::OpenAiAuthBackend::import_codex_record_with_overrides(
            AuthSlotId::new("openai-codex"),
            &codex_auth_path,
            "http://127.0.0.1:1",
            "client-test",
            "http://example.test/backend-api/codex/responses",
            None,
            None,
        )?;
        manager.put_record(record).await?;

        let resolved = manager
            .resolve(&AuthSlotId::new("openai-codex"), false)
            .await?;
        assert_eq!(
            resolved.headers.get("Authorization"),
            Some(&"Bearer existing-access-token".to_string())
        );
        assert_eq!(
            resolved.headers.get("ChatGPT-Account-ID"),
            Some(&"acc-existing".to_string())
        );
        assert_eq!(
            resolved.base_url_override.as_deref(),
            Some("http://example.test/backend-api/codex/responses")
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_force_refreshes_share_one_refresh_cycle() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        #[derive(Default)]
        struct Counts {
            refreshes: AtomicUsize,
        }

        async fn oauth_token(
            State(counts): State<Arc<Counts>>,
            headers: axum::http::HeaderMap,
            _body: Bytes,
        ) -> Json<serde_json::Value> {
            let content_type = headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            if content_type.contains("application/json") {
                counts.refreshes.fetch_add(1, Ordering::SeqCst);
                return Json(json!({
                    "access_token": "fresh-access-token",
                    "refresh_token": "fresh-refresh-token"
                }));
            }
            panic!("unexpected non-refresh OpenAI auth request");
        }

        let counts = Arc::new(Counts::default());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new()
            .route("/oauth/token", post(oauth_token))
            .with_state(counts.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let temp = tempdir()?;
        let codex_auth_path = temp.path().join("codex-auth.json");
        std::fs::write(
            &codex_auth_path,
            serde_json::to_string_pretty(&json!({
                "auth_mode": "chatgpt",
                "OPENAI_API_KEY": null,
                "tokens": {
                    "refresh_token": "stale-refresh-token",
                    "account_id": "acc-test"
                }
            }))?,
        )?;

        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let record = crate::OpenAiAuthBackend::import_codex_record_with_overrides(
            AuthSlotId::new("openai-codex"),
            &codex_auth_path,
            format!("http://{address}"),
            "client-test",
            "http://example.test/backend-api/codex/responses",
            None,
            None,
        )?;
        manager.put_record(record).await?;

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let manager = manager.clone();
            tasks.push(tokio::spawn(async move {
                manager
                    .resolve(&AuthSlotId::new("openai-codex"), true)
                    .await
            }));
        }

        for task in tasks {
            let resolved = task.await??;
            assert_eq!(
                resolved.headers.get("Authorization"),
                Some(&"Bearer fresh-access-token".to_string())
            );
        }

        assert_eq!(counts.refreshes.load(Ordering::SeqCst), 1);
        server.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn claude_code_import_refreshes_tokens() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        async fn oauth_token(
            headers: axum::http::HeaderMap,
            body: Bytes,
        ) -> Json<serde_json::Value> {
            let content_type = headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert!(content_type.contains("application/json"));
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            assert_eq!(payload["grant_type"], "refresh_token");
            assert_eq!(payload["refresh_token"], "stale-refresh-token");
            Json(json!({
                "access_token": "fresh-access-token",
                "refresh_token": "fresh-refresh-token",
                "expires_in": 3600,
                "scope": "user:profile user:inference"
            }))
        }

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new().route("/oauth/token", post(oauth_token));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let temp = tempdir()?;
        let credentials_path = temp.path().join(".credentials.json");
        std::fs::write(
            &credentials_path,
            serde_json::to_string_pretty(&json!({
                "claudeAiOauth": {
                    "accessToken": "stale-access-token",
                    "refreshToken": "stale-refresh-token",
                    "expiresAt": 1,
                    "scopes": ["user:profile", "user:inference"],
                    "subscriptionType": "pro",
                    "rateLimitTier": "tier-1"
                }
            }))?,
        )?;

        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let record = crate::AnthropicAuthBackend::import_claude_code_record_with_overrides(
            AuthSlotId::new("anthropic-claude-code"),
            &credentials_path,
            format!("http://{address}/oauth/token"),
            "client-test",
        )?;
        manager.put_record(record).await?;

        let resolved = manager
            .resolve(&AuthSlotId::new("anthropic-claude-code"), false)
            .await?;
        assert_eq!(
            resolved.headers.get("Authorization"),
            Some(&"Bearer fresh-access-token".to_string())
        );
        server.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_anthropic_force_refreshes_share_one_refresh_cycle() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        #[derive(Default)]
        struct Counts {
            refreshes: AtomicUsize,
        }

        async fn oauth_token(
            State(counts): State<Arc<Counts>>,
            _headers: axum::http::HeaderMap,
            _body: Bytes,
        ) -> Json<serde_json::Value> {
            counts.refreshes.fetch_add(1, Ordering::SeqCst);
            Json(json!({
                "access_token": "fresh-access-token",
                "refresh_token": "fresh-refresh-token",
                "expires_in": 3600,
                "scope": "user:profile user:inference"
            }))
        }

        let counts = Arc::new(Counts::default());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let router = Router::new()
            .route("/oauth/token", post(oauth_token))
            .with_state(counts.clone());
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let temp = tempdir()?;
        let credentials_path = temp.path().join(".credentials.json");
        std::fs::write(
            &credentials_path,
            serde_json::to_string_pretty(&json!({
                "claudeAiOauth": {
                    "accessToken": "stale-access-token",
                    "refreshToken": "stale-refresh-token",
                    "expiresAt": 1,
                    "scopes": ["user:profile", "user:inference"],
                    "subscriptionType": "pro",
                    "rateLimitTier": "tier-1"
                }
            }))?,
        )?;

        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let record = crate::AnthropicAuthBackend::import_claude_code_record_with_overrides(
            AuthSlotId::new("anthropic-claude-code"),
            &credentials_path,
            format!("http://{address}/oauth/token"),
            "client-test",
        )?;
        manager.put_record(record).await?;

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let manager = manager.clone();
            tasks.push(tokio::spawn(async move {
                manager
                    .resolve(&AuthSlotId::new("anthropic-claude-code"), true)
                    .await
            }));
        }

        for task in tasks {
            let resolved = task.await??;
            assert_eq!(
                resolved.headers.get("Authorization"),
                Some(&"Bearer fresh-access-token".to_string())
            );
        }

        assert_eq!(counts.refreshes.load(Ordering::SeqCst), 1);
        server.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_store_round_trip_google_api_key() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let status = manager
            .store_google_api_key(AuthSlotId::new("google-default"), "google-secret")
            .await?;
        assert_eq!(status.summary, "api_key");

        let resolved = manager
            .resolve(&AuthSlotId::new("google-default"), false)
            .await?;
        assert_eq!(
            resolved.headers.get("x-goog-api-key"),
            Some(&"google-secret".to_string())
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_store_round_trip_openrouter_api_key() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        manager
            .store_openrouter_api_key(AuthSlotId::new("openrouter-default"), "or-secret")
            .await?;

        let resolved = manager
            .resolve(&AuthSlotId::new("openrouter-default"), false)
            .await?;
        assert_eq!(
            resolved.headers.get("Authorization"),
            Some(&"Bearer or-secret".to_string())
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_store_round_trip_xai_api_key() -> Result<()> {
        let _guard = ensure_auth_store_master_key();
        let temp = tempdir()?;
        let manager = AuthManager::new(temp.path().join("auth-store.json"))?;
        let status = manager
            .store_xai_api_key(AuthSlotId::new("xai-default"), "xai-secret")
            .await?;
        assert_eq!(status.summary, "api_key");

        let resolved = manager
            .resolve(&AuthSlotId::new("xai-default"), false)
            .await?;
        assert_eq!(
            resolved.headers.get("Authorization"),
            Some(&"Bearer xai-secret".to_string())
        );
        Ok(())
    }
}
