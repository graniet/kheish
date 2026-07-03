//! Runtime connector persistence and resolution service for the daemon control plane.

use parking_lot::RwLock;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::sync::Mutex;

use kheish_auth::{AuthManager, AuthSlotId};
use kheish_session::write_json_pretty_atomically;

use crate::connectors::{
    ConnectorKind, ConnectorRegistry, ConnectorSessionPolicy, ConnectorSettings,
    ExternalConnectorConfig, HttpInputConnectorConfig, SlackConnectorConfig,
    TelegramConnectorConfig, reply_target_references_connector,
};
use crate::personas::PersonaIndex;
use crate::state_files::read_json_or_quarantine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectorConfigSource {
    File,
    Daemon,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ConnectorConfigRecord {
    External {
        source: ConnectorConfigSource,
        config: ExternalConnectorConfig,
    },
    Telegram {
        source: ConnectorConfigSource,
        config: TelegramConnectorConfig,
    },
    Slack {
        source: ConnectorConfigSource,
        config: SlackConnectorConfig,
    },
    Http {
        source: ConnectorConfigSource,
        config: HttpInputConnectorConfig,
    },
}

impl ConnectorConfigRecord {
    pub(crate) fn kind(&self) -> ConnectorKind {
        match self {
            Self::External { .. } => ConnectorKind::External,
            Self::Telegram { .. } => ConnectorKind::Telegram,
            Self::Slack { .. } => ConnectorKind::Slack,
            Self::Http { .. } => ConnectorKind::Http,
        }
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            Self::External { config, .. } => &config.name,
            Self::Telegram { config, .. } => &config.name,
            Self::Slack { config, .. } => &config.name,
            Self::Http { config, .. } => &config.name,
        }
    }

    fn references_secret_ref(&self, secret_ref: &str) -> bool {
        match self {
            Self::External { config, .. } => {
                config.shared_token_secret_ref.as_deref() == Some(secret_ref)
                    || config.child_process.as_ref().is_some_and(|child_process| {
                        child_process
                            .credential_slots
                            .values()
                            .any(|candidate| candidate == secret_ref)
                    })
            }
            Self::Telegram { config, .. } => {
                config.bot_token_secret_ref.as_deref() == Some(secret_ref)
                    || config.secret_token_secret_ref.as_deref() == Some(secret_ref)
            }
            Self::Slack { config, .. } => {
                config.bot_token_secret_ref.as_deref() == Some(secret_ref)
                    || config.signing_secret_secret_ref.as_deref() == Some(secret_ref)
                    || config
                        .team_bot_tokens
                        .iter()
                        .any(|team| team.bot_token_secret_ref.as_deref() == Some(secret_ref))
            }
            Self::Http { config, .. } => {
                config.bearer_token_secret_ref.as_deref() == Some(secret_ref)
                    || config.hmac_secret_secret_ref.as_deref() == Some(secret_ref)
            }
        }
    }

    fn secret_refs(&self) -> Vec<&str> {
        let mut refs = Vec::new();
        match self {
            Self::External { config, .. } => {
                if let Some(secret_ref) = config.shared_token_secret_ref.as_deref() {
                    refs.push(secret_ref);
                }
                if let Some(child_process) = config.child_process.as_ref() {
                    refs.extend(child_process.credential_slots.values().map(String::as_str));
                }
            }
            Self::Telegram { config, .. } => {
                if let Some(secret_ref) = config.bot_token_secret_ref.as_deref() {
                    refs.push(secret_ref);
                }
                if let Some(secret_ref) = config.secret_token_secret_ref.as_deref() {
                    refs.push(secret_ref);
                }
            }
            Self::Slack { config, .. } => {
                if let Some(secret_ref) = config.bot_token_secret_ref.as_deref() {
                    refs.push(secret_ref);
                }
                if let Some(secret_ref) = config.signing_secret_secret_ref.as_deref() {
                    refs.push(secret_ref);
                }
                refs.extend(
                    config
                        .team_bot_tokens
                        .iter()
                        .filter_map(|team| team.bot_token_secret_ref.as_deref()),
                );
            }
            Self::Http { config, .. } => {
                if let Some(secret_ref) = config.bearer_token_secret_ref.as_deref() {
                    refs.push(secret_ref);
                }
                if let Some(secret_ref) = config.hmac_secret_secret_ref.as_deref() {
                    refs.push(secret_ref);
                }
            }
        }
        refs
    }

    fn session_policy(&self) -> &ConnectorSessionPolicy {
        match self {
            Self::External { config, .. } => &config.session_policy,
            Self::Telegram { config, .. } => &config.session_policy,
            Self::Slack { config, .. } => &config.session_policy,
            Self::Http { config, .. } => &config.session_policy,
        }
    }
}

#[derive(Clone, Debug)]
struct FileConnectorStore {
    path: PathBuf,
}

impl FileConnectorStore {
    fn new(state_root: &Path) -> Self {
        Self {
            path: state_root.join("runtime-connectors.json"),
        }
    }

    fn load(&self) -> Result<ConnectorSettings> {
        Ok(read_json_or_quarantine(&self.path, "runtime connector settings")?.unwrap_or_default())
    }

    fn save(&self, settings: &ConnectorSettings) -> Result<()> {
        if settings == &ConnectorSettings::default() {
            match fs::remove_file(&self.path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to delete empty runtime connector settings {}",
                            self.path.display()
                        )
                    });
                }
            }
        }
        write_json_pretty_atomically(&self.path, settings)
    }
}

/// Owns daemon-managed connector settings and keeps the resolved runtime registry in sync.
pub(crate) struct ConnectorService {
    store: FileConnectorStore,
    auth_manager: Arc<AuthManager>,
    file_settings: ConnectorSettings,
    daemon_settings: Mutex<ConnectorSettings>,
    runtime_disabled_connectors: RwLock<BTreeSet<(ConnectorKind, String)>>,
    registry: Arc<ConnectorRegistry>,
}

impl ConnectorService {
    /// Loads file-backed and daemon-managed connector settings and builds one shared registry.
    pub(crate) fn load(
        state_root: &Path,
        file_path: Option<&Path>,
        auth_manager: Arc<AuthManager>,
    ) -> Result<Self> {
        let store = FileConnectorStore::new(state_root);
        let file_settings = load_file_connector_settings(file_path)?;
        let daemon_settings = store.load()?;
        validate_no_cross_source_name_collisions(&file_settings, &daemon_settings)?;
        let mut resolvable_file_settings = file_settings.clone();
        let mut resolvable_daemon_settings = daemon_settings.clone();
        retain_connectors_not_referencing_revoked_secret_ref(
            &mut resolvable_file_settings,
            auth_manager.as_ref(),
        );
        retain_connectors_not_referencing_revoked_secret_ref(
            &mut resolvable_daemon_settings,
            auth_manager.as_ref(),
        );
        let merged =
            merged_connector_settings(&resolvable_file_settings, &resolvable_daemon_settings);
        let registry = Arc::new(ConnectorRegistry::resolve(merged, auth_manager.as_ref())?);
        Ok(Self {
            store,
            auth_manager,
            file_settings,
            daemon_settings: Mutex::new(daemon_settings),
            runtime_disabled_connectors: RwLock::new(BTreeSet::new()),
            registry,
        })
    }

    /// Returns the shared resolved connector registry used by runtime delivery and ingress.
    pub(crate) fn registry(&self) -> Arc<ConnectorRegistry> {
        self.registry.clone()
    }

    /// Re-resolves every connector against the current secret store contents.
    pub(crate) async fn reload_resolved(&self) -> Result<()> {
        let daemon_settings = self.daemon_settings.lock().await.clone();
        let merged = self.resolvable_merged_settings(&daemon_settings);
        self.registry.rebuild(merged, self.auth_manager.as_ref())
    }

    /// Persistently removes daemon-managed connectors that depend on one revoked secret.
    pub(crate) async fn disable_secret_ref_connectors(&self, secret_ref: &str) -> Result<usize> {
        let mut guard = self.daemon_settings.lock().await;
        let mut daemon_settings = guard.clone();
        let referenced = connector_records(&self.file_settings, ConnectorConfigSource::File)
            .into_iter()
            .chain(connector_records(
                &daemon_settings,
                ConnectorConfigSource::Daemon,
            ))
            .filter(|record| record.references_secret_ref(secret_ref))
            .map(|record| connector_runtime_id(&record))
            .collect::<Vec<_>>();
        if !referenced.is_empty() {
            let mut disabled = self.runtime_disabled_connectors.write();
            disabled.extend(referenced);
        }
        let before = connector_count(&daemon_settings);
        retain_connectors_not_referencing_secret_ref(&mut daemon_settings, secret_ref);
        let removed = before.saturating_sub(connector_count(&daemon_settings));
        let live_settings = self.resolvable_merged_settings(&daemon_settings);
        let _validated =
            ConnectorRegistry::resolve(live_settings.clone(), self.auth_manager.as_ref())?;
        self.registry
            .rebuild(live_settings, self.auth_manager.as_ref())?;
        if removed == 0 {
            return Ok(0);
        }
        self.persist_and_rebuild_resolvable(&mut guard, daemon_settings)
            .await?;
        Ok(removed)
    }

    pub(crate) async fn connectors_referencing_secret_ref(
        &self,
        secret_ref: &str,
    ) -> Vec<ConnectorConfigRecord> {
        self.list_connectors()
            .await
            .into_iter()
            .filter(|record| record.references_secret_ref(secret_ref))
            .collect()
    }

    /// Lists every known connector across file-backed and daemon-managed sources.
    pub(crate) async fn list_connectors(&self) -> Vec<ConnectorConfigRecord> {
        let daemon_settings = self.daemon_settings.lock().await.clone();
        let mut records = connector_records(&self.file_settings, ConnectorConfigSource::File);
        records.extend(connector_records(
            &daemon_settings,
            ConnectorConfigSource::Daemon,
        ));
        records.sort_by_key(connector_record_sort_key);
        records
    }

    /// Returns one known connector by kind and name.
    pub(crate) async fn connector(&self, kind: &str, name: &str) -> Option<ConnectorConfigRecord> {
        let kind = ConnectorKind::parse(kind).ok()?;
        self.list_connectors()
            .await
            .into_iter()
            .find(|record| record.kind() == kind && record.name() == name)
    }

    /// Returns true when any known connector references the provided secret-store slot.
    pub(crate) async fn uses_secret_ref(&self, secret_ref: &str) -> bool {
        self.list_connectors()
            .await
            .into_iter()
            .any(|record| record.references_secret_ref(secret_ref))
    }

    /// Validates that every connector session-policy persona reference resolves during boot.
    pub(crate) async fn validate_session_policies(
        &self,
        persona_index: &PersonaIndex,
    ) -> Result<()> {
        for record in self.list_connectors().await {
            let Some(persona_id) = record.session_policy().persona_id.as_deref() else {
                continue;
            };
            anyhow::ensure!(
                persona_index.personas.contains_key(persona_id),
                "{} connector {} references unknown persona {}",
                record.kind(),
                record.name(),
                persona_id
            );
        }
        Ok(())
    }

    /// Creates or replaces one daemon-managed Telegram connector.
    pub(crate) async fn put_external_connector(
        &self,
        config: ExternalConnectorConfig,
    ) -> Result<ExternalConnectorConfig> {
        self.put_managed_connector("external", &config.name, |settings| {
            upsert_named(&mut settings.external_connectors, config.clone(), |value| {
                &value.name
            });
        })
        .await?;
        Ok(config)
    }

    /// Creates or replaces one daemon-managed Telegram connector.
    pub(crate) async fn put_telegram_connector(
        &self,
        config: TelegramConnectorConfig,
    ) -> Result<TelegramConnectorConfig> {
        self.put_managed_connector("telegram", &config.name, |settings| {
            upsert_named(&mut settings.telegram_connectors, config.clone(), |value| {
                &value.name
            });
        })
        .await?;
        Ok(config)
    }

    /// Creates or replaces one daemon-managed Slack connector.
    pub(crate) async fn put_slack_connector(
        &self,
        config: SlackConnectorConfig,
    ) -> Result<SlackConnectorConfig> {
        self.put_managed_connector("slack", &config.name, |settings| {
            upsert_named(&mut settings.slack_connectors, config.clone(), |value| {
                &value.name
            });
        })
        .await?;
        Ok(config)
    }

    /// Creates or replaces one daemon-managed HTTP connector.
    pub(crate) async fn put_http_connector(
        &self,
        config: HttpInputConnectorConfig,
    ) -> Result<HttpInputConnectorConfig> {
        self.put_managed_connector("http", &config.name, |settings| {
            upsert_named(&mut settings.http_connectors, config.clone(), |value| {
                &value.name
            });
        })
        .await?;
        Ok(config)
    }

    /// Creates one daemon-managed HTTP connector only when no connector with that name exists.
    pub(crate) async fn put_http_connector_if_absent(
        &self,
        config: HttpInputConnectorConfig,
    ) -> Result<HttpInputConnectorConfig> {
        self.put_managed_connector_if_absent("http", &config.name, |settings| {
            upsert_named(&mut settings.http_connectors, config.clone(), |value| {
                &value.name
            });
        })
        .await?;
        Ok(config)
    }

    /// Deletes one daemon-managed connector when it exists.
    pub(crate) async fn delete_connector(&self, kind: &str, name: &str) -> Result<bool> {
        let kind = ConnectorKind::parse(kind)?;
        self.reject_file_backed_connector_collision(kind, name)?;
        let mut guard = self.daemon_settings.lock().await;
        let mut daemon_settings = guard.clone();
        self.reject_connector_reply_target_dependencies(kind, name, &daemon_settings)?;
        let removed = match kind {
            ConnectorKind::External => {
                remove_named(&mut daemon_settings.external_connectors, name, |value| {
                    &value.name
                })
            }
            ConnectorKind::Telegram => {
                remove_named(&mut daemon_settings.telegram_connectors, name, |value| {
                    &value.name
                })
            }
            ConnectorKind::Slack => {
                remove_named(&mut daemon_settings.slack_connectors, name, |value| {
                    &value.name
                })
            }
            ConnectorKind::Http => {
                remove_named(&mut daemon_settings.http_connectors, name, |value| {
                    &value.name
                })
            }
        };
        if !removed {
            return Ok(false);
        }
        self.persist_and_rebuild(&mut guard, daemon_settings)
            .await?;
        Ok(true)
    }

    fn reject_file_backed_connector_collision(
        &self,
        kind: ConnectorKind,
        name: &str,
    ) -> Result<()> {
        let file_names = connector_name_set(&self.file_settings, kind);
        if file_names.contains(name) {
            bail!(
                "{kind} connector {name} is file-backed and cannot be mutated through the daemon control plane"
            );
        }
        Ok(())
    }

    fn reject_connector_reply_target_dependencies(
        &self,
        kind: ConnectorKind,
        name: &str,
        daemon_settings: &ConnectorSettings,
    ) -> Result<()> {
        if kind == ConnectorKind::Http {
            return Ok(());
        }
        let self_label = format!("{kind}/{name}");
        let dependents = connector_reply_target_dependents(kind, name, &self.file_settings)
            .into_iter()
            .chain(connector_reply_target_dependents(
                kind,
                name,
                daemon_settings,
            ))
            .filter(|dependent| dependent != &self_label)
            .collect::<Vec<_>>();
        if !dependents.is_empty() {
            bail!(
                "cannot delete {kind}/{name}; it is still referenced by {}",
                dependents.join(", ")
            );
        }
        Ok(())
    }

    async fn put_managed_connector<F>(&self, kind: &str, name: &str, mutate: F) -> Result<()>
    where
        F: FnOnce(&mut ConnectorSettings),
    {
        let kind = ConnectorKind::parse(kind)?;
        self.reject_file_backed_connector_collision(kind, name)?;
        let mut guard = self.daemon_settings.lock().await;
        let mut next = guard.clone();
        mutate(&mut next);
        self.runtime_disabled_connectors
            .write()
            .remove(&(kind, name.to_string()));
        self.persist_and_rebuild(&mut guard, next).await
    }

    async fn put_managed_connector_if_absent<F>(
        &self,
        kind: &str,
        name: &str,
        mutate: F,
    ) -> Result<()>
    where
        F: FnOnce(&mut ConnectorSettings),
    {
        let kind = ConnectorKind::parse(kind)?;
        self.reject_file_backed_connector_collision(kind, name)?;
        let mut guard = self.daemon_settings.lock().await;
        if connector_name_set(&guard, kind).contains(name) {
            bail!("{kind} connector {name} already exists");
        }
        let mut next = guard.clone();
        mutate(&mut next);
        self.runtime_disabled_connectors
            .write()
            .remove(&(kind, name.to_string()));
        self.persist_and_rebuild(&mut guard, next).await
    }

    async fn persist_and_rebuild(
        &self,
        current: &mut tokio::sync::MutexGuard<'_, ConnectorSettings>,
        daemon_settings: ConnectorSettings,
    ) -> Result<()> {
        validate_no_cross_source_name_collisions(&self.file_settings, &daemon_settings)?;
        let merged = self.resolvable_merged_settings(&daemon_settings);
        let resolved = ConnectorRegistry::resolve(merged, self.auth_manager.as_ref())?;
        self.store.save(&daemon_settings)?;
        self.registry.replace_with_resolved(resolved);
        **current = daemon_settings;
        Ok(())
    }

    async fn persist_and_rebuild_resolvable(
        &self,
        current: &mut tokio::sync::MutexGuard<'_, ConnectorSettings>,
        daemon_settings: ConnectorSettings,
    ) -> Result<()> {
        validate_no_cross_source_name_collisions(&self.file_settings, &daemon_settings)?;
        let merged = self.resolvable_merged_settings(&daemon_settings);
        let resolved = ConnectorRegistry::resolve(merged, self.auth_manager.as_ref())?;
        self.store.save(&daemon_settings)?;
        self.registry.replace_with_resolved(resolved);
        **current = daemon_settings;
        Ok(())
    }

    fn resolvable_merged_settings(&self, daemon_settings: &ConnectorSettings) -> ConnectorSettings {
        let mut file_settings = self.file_settings.clone();
        let mut daemon_settings = daemon_settings.clone();
        retain_connectors_not_referencing_revoked_secret_ref(
            &mut file_settings,
            self.auth_manager.as_ref(),
        );
        retain_connectors_not_referencing_revoked_secret_ref(
            &mut daemon_settings,
            self.auth_manager.as_ref(),
        );
        let disabled = self.runtime_disabled_connectors.read();
        retain_connectors_not_runtime_disabled(&mut file_settings, &disabled);
        retain_connectors_not_runtime_disabled(&mut daemon_settings, &disabled);
        merged_connector_settings(&file_settings, &daemon_settings)
    }
}

fn load_file_connector_settings(path: Option<&Path>) -> Result<ConnectorSettings> {
    let Some(path) = path else {
        return Ok(ConnectorSettings::default());
    };
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read connector config {}", path.display()))?;
    let settings = toml::from_str::<ConnectorSettings>(&raw)
        .with_context(|| format!("failed to parse connector config {}", path.display()))?;
    validate_unique_connector_names(&settings)?;
    Ok(settings)
}

fn merged_connector_settings(
    file_settings: &ConnectorSettings,
    daemon_settings: &ConnectorSettings,
) -> ConnectorSettings {
    let mut merged = file_settings.clone();
    merged
        .external_connectors
        .extend(daemon_settings.external_connectors.clone());
    merged
        .telegram_connectors
        .extend(daemon_settings.telegram_connectors.clone());
    merged
        .slack_connectors
        .extend(daemon_settings.slack_connectors.clone());
    merged
        .http_connectors
        .extend(daemon_settings.http_connectors.clone());
    merged
}

fn connector_count(settings: &ConnectorSettings) -> usize {
    settings.external_connectors.len()
        + settings.telegram_connectors.len()
        + settings.slack_connectors.len()
        + settings.http_connectors.len()
}

fn connector_runtime_id(record: &ConnectorConfigRecord) -> (ConnectorKind, String) {
    (record.kind(), record.name().to_string())
}

fn retain_connectors_not_runtime_disabled(
    settings: &mut ConnectorSettings,
    disabled: &BTreeSet<(ConnectorKind, String)>,
) {
    settings
        .external_connectors
        .retain(|config| !disabled.contains(&(ConnectorKind::External, config.name.clone())));
    settings
        .http_connectors
        .retain(|config| !disabled.contains(&(ConnectorKind::Http, config.name.clone())));
    settings
        .slack_connectors
        .retain(|config| !disabled.contains(&(ConnectorKind::Slack, config.name.clone())));
    settings
        .telegram_connectors
        .retain(|config| !disabled.contains(&(ConnectorKind::Telegram, config.name.clone())));
}

fn retain_connectors_not_referencing_secret_ref(
    settings: &mut ConnectorSettings,
    secret_ref: &str,
) {
    settings.external_connectors.retain(|config| {
        !ConnectorConfigRecord::External {
            source: ConnectorConfigSource::Daemon,
            config: config.clone(),
        }
        .references_secret_ref(secret_ref)
    });
    settings.telegram_connectors.retain(|config| {
        !ConnectorConfigRecord::Telegram {
            source: ConnectorConfigSource::Daemon,
            config: config.clone(),
        }
        .references_secret_ref(secret_ref)
    });
    settings.slack_connectors.retain(|config| {
        !ConnectorConfigRecord::Slack {
            source: ConnectorConfigSource::Daemon,
            config: config.clone(),
        }
        .references_secret_ref(secret_ref)
    });
    settings.http_connectors.retain(|config| {
        !ConnectorConfigRecord::Http {
            source: ConnectorConfigSource::Daemon,
            config: config.clone(),
        }
        .references_secret_ref(secret_ref)
    });
}

fn retain_connectors_not_referencing_revoked_secret_ref(
    settings: &mut ConnectorSettings,
    auth_manager: &AuthManager,
) {
    settings.external_connectors.retain(|config| {
        !ConnectorConfigRecord::External {
            source: ConnectorConfigSource::Daemon,
            config: config.clone(),
        }
        .secret_refs()
        .into_iter()
        .any(|secret_ref| auth_manager.is_slot_revoked(&AuthSlotId::new(secret_ref)))
    });
    settings.telegram_connectors.retain(|config| {
        !ConnectorConfigRecord::Telegram {
            source: ConnectorConfigSource::Daemon,
            config: config.clone(),
        }
        .secret_refs()
        .into_iter()
        .any(|secret_ref| auth_manager.is_slot_revoked(&AuthSlotId::new(secret_ref)))
    });
    settings.slack_connectors.retain(|config| {
        !ConnectorConfigRecord::Slack {
            source: ConnectorConfigSource::Daemon,
            config: config.clone(),
        }
        .secret_refs()
        .into_iter()
        .any(|secret_ref| auth_manager.is_slot_revoked(&AuthSlotId::new(secret_ref)))
    });
    settings.http_connectors.retain(|config| {
        !ConnectorConfigRecord::Http {
            source: ConnectorConfigSource::Daemon,
            config: config.clone(),
        }
        .secret_refs()
        .into_iter()
        .any(|secret_ref| auth_manager.is_slot_revoked(&AuthSlotId::new(secret_ref)))
    });
}

fn validate_no_cross_source_name_collisions(
    file_settings: &ConnectorSettings,
    daemon_settings: &ConnectorSettings,
) -> Result<()> {
    validate_unique_connector_names(file_settings)?;
    validate_unique_connector_names(daemon_settings)?;
    validate_no_name_collisions(
        "external",
        file_settings
            .external_connectors
            .iter()
            .map(|value| value.name.as_str()),
        daemon_settings
            .external_connectors
            .iter()
            .map(|value| value.name.as_str()),
    )?;
    validate_no_name_collisions(
        "telegram",
        file_settings
            .telegram_connectors
            .iter()
            .map(|value| value.name.as_str()),
        daemon_settings
            .telegram_connectors
            .iter()
            .map(|value| value.name.as_str()),
    )?;
    validate_no_name_collisions(
        "slack",
        file_settings
            .slack_connectors
            .iter()
            .map(|value| value.name.as_str()),
        daemon_settings
            .slack_connectors
            .iter()
            .map(|value| value.name.as_str()),
    )?;
    validate_no_name_collisions(
        "http",
        file_settings
            .http_connectors
            .iter()
            .map(|value| value.name.as_str()),
        daemon_settings
            .http_connectors
            .iter()
            .map(|value| value.name.as_str()),
    )?;
    Ok(())
}

fn validate_unique_connector_names(settings: &ConnectorSettings) -> Result<()> {
    validate_unique_names(
        "external",
        settings
            .external_connectors
            .iter()
            .map(|value| value.name.as_str()),
    )?;
    validate_unique_names(
        "telegram",
        settings
            .telegram_connectors
            .iter()
            .map(|value| value.name.as_str()),
    )?;
    validate_unique_names(
        "slack",
        settings
            .slack_connectors
            .iter()
            .map(|value| value.name.as_str()),
    )?;
    validate_unique_names(
        "http",
        settings
            .http_connectors
            .iter()
            .map(|value| value.name.as_str()),
    )?;
    Ok(())
}

fn validate_unique_names<'a>(
    kind: &'static str,
    names: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        validate_connector_name(kind, name)?;
        if !seen.insert(name.to_string()) {
            bail!("duplicate {kind} connector name {name}");
        }
    }
    Ok(())
}

fn validate_connector_name(kind: &'static str, name: &str) -> Result<()> {
    if name.contains(':') {
        bail!("{kind} connector name {name} cannot contain ':'");
    }
    Ok(())
}

fn validate_no_name_collisions<'a>(
    kind: &'static str,
    file_names: impl Iterator<Item = &'a str>,
    daemon_names: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let file_names = file_names.collect::<BTreeSet<_>>();
    let daemon_names = daemon_names.collect::<BTreeSet<_>>();
    if let Some(name) = daemon_names
        .into_iter()
        .find(|name| file_names.contains(name))
    {
        bail!("daemon-managed {kind} connector {name} conflicts with a file-backed connector");
    }
    Ok(())
}

fn connector_name_set(settings: &ConnectorSettings, kind: ConnectorKind) -> BTreeSet<String> {
    match kind {
        ConnectorKind::External => settings
            .external_connectors
            .iter()
            .map(|value| value.name.clone())
            .collect(),
        ConnectorKind::Telegram => settings
            .telegram_connectors
            .iter()
            .map(|value| value.name.clone())
            .collect(),
        ConnectorKind::Slack => settings
            .slack_connectors
            .iter()
            .map(|value| value.name.clone())
            .collect(),
        ConnectorKind::Http => settings
            .http_connectors
            .iter()
            .map(|value| value.name.clone())
            .collect(),
    }
}

fn upsert_named<T, F>(items: &mut Vec<T>, value: T, name_of: F)
where
    F: Fn(&T) -> &str,
{
    let name = name_of(&value).to_string();
    if let Some(existing) = items.iter_mut().find(|entry| name_of(entry) == name) {
        *existing = value;
    } else {
        items.push(value);
        items.sort_by(|left, right| name_of(left).cmp(name_of(right)));
    }
}

fn remove_named<T, F>(items: &mut Vec<T>, name: &str, name_of: F) -> bool
where
    F: Fn(&T) -> &str,
{
    let before = items.len();
    items.retain(|entry| name_of(entry) != name);
    before != items.len()
}

fn connector_records(
    settings: &ConnectorSettings,
    source: ConnectorConfigSource,
) -> Vec<ConnectorConfigRecord> {
    let mut records = Vec::new();
    records.extend(
        settings
            .external_connectors
            .iter()
            .cloned()
            .map(|config| ConnectorConfigRecord::External { source, config }),
    );
    records.extend(
        settings
            .telegram_connectors
            .iter()
            .cloned()
            .map(|config| ConnectorConfigRecord::Telegram { source, config }),
    );
    records.extend(
        settings
            .slack_connectors
            .iter()
            .cloned()
            .map(|config| ConnectorConfigRecord::Slack { source, config }),
    );
    records.extend(
        settings
            .http_connectors
            .iter()
            .cloned()
            .map(|config| ConnectorConfigRecord::Http { source, config }),
    );
    records
}

fn connector_reply_target_dependents(
    kind: ConnectorKind,
    name: &str,
    settings: &ConnectorSettings,
) -> Vec<String> {
    let mut dependents = Vec::new();
    for connector in &settings.external_connectors {
        if connector
            .additional_reply_targets
            .iter()
            .any(|target| reply_target_references_connector(target, kind, name))
        {
            dependents.push(format!("{}/{}", ConnectorKind::External, connector.name));
        }
    }
    for connector in &settings.telegram_connectors {
        if connector
            .additional_reply_targets
            .iter()
            .any(|target| reply_target_references_connector(target, kind, name))
        {
            dependents.push(format!("{}/{}", ConnectorKind::Telegram, connector.name));
        }
    }
    for connector in &settings.slack_connectors {
        if connector
            .additional_reply_targets
            .iter()
            .any(|target| reply_target_references_connector(target, kind, name))
        {
            dependents.push(format!("{}/{}", ConnectorKind::Slack, connector.name));
        }
    }
    for connector in &settings.http_connectors {
        if connector
            .default_reply_targets
            .iter()
            .any(|target| reply_target_references_connector(target, kind, name))
        {
            dependents.push(format!("{}/{}", ConnectorKind::Http, connector.name));
        }
    }
    dependents
}

fn connector_record_sort_key(record: &ConnectorConfigRecord) -> (String, String) {
    (
        record.kind().as_str().to_string(),
        record.name().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use anyhow::Result;
    use tempfile::tempdir;

    use crate::ConnectorSessionPolicy;
    use kheish_auth::{AUTH_STORE_MASTER_KEY_ENV, AuthManager, AuthSlotId};

    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn connector_service_persists_managed_connectors_and_rebuilds_registry() -> Result<()> {
        let temp = tempdir()?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;
        auth_manager
            .store_generic_secret(AuthSlotId::new("telegram.bot"), "token-123")
            .await?;
        let service = ConnectorService::load(temp.path(), None, auth_manager.clone())?;
        service
            .put_telegram_connector(TelegramConnectorConfig {
                name: "bot".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: Some("telegram.bot".to_string()),
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: crate::connectors::TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            })
            .await?;

        let listed = service.list_connectors().await;
        assert_eq!(listed.len(), 1);
        assert!(service.registry().telegram("bot").is_some());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connector_service_reload_resolved_updates_secret_backed_connectors() -> Result<()> {
        let temp = tempdir()?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;
        auth_manager
            .store_generic_secret(AuthSlotId::new("telegram.bot"), "token-123")
            .await?;
        let service = ConnectorService::load(temp.path(), None, auth_manager.clone())?;
        service
            .put_telegram_connector(TelegramConnectorConfig {
                name: "bot".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: Some("telegram.bot".to_string()),
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: crate::connectors::TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            })
            .await?;

        assert_eq!(
            service
                .registry()
                .telegram("bot")
                .and_then(|connector| connector.bot_token),
            Some("token-123".to_string())
        );

        auth_manager
            .store_generic_secret(AuthSlotId::new("telegram.bot"), "token-456")
            .await?;
        service.reload_resolved().await?;

        assert_eq!(
            service
                .registry()
                .telegram("bot")
                .and_then(|connector| connector.bot_token),
            Some("token-456".to_string())
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connector_service_keeps_prior_state_when_validation_fails() -> Result<()> {
        let temp = tempdir()?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;
        auth_manager
            .store_generic_secret(AuthSlotId::new("telegram.bot"), "token-123")
            .await?;
        let service = ConnectorService::load(temp.path(), None, auth_manager.clone())?;
        service
            .put_telegram_connector(TelegramConnectorConfig {
                name: "bot".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: Some("telegram.bot".to_string()),
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: crate::connectors::TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            })
            .await?;

        let error = service
            .put_telegram_connector(TelegramConnectorConfig {
                name: "bot".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: Some("missing.telegram.bot".to_string()),
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: crate::connectors::TelegramIngressMode::Polling,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            })
            .await
            .expect_err("invalid connector update should fail");
        assert!(
            error
                .to_string()
                .contains("missing secret-store slot missing.telegram.bot"),
            "unexpected error: {error:#}"
        );

        let listed = service.list_connectors().await;
        assert_eq!(listed.len(), 1);
        match &listed[0] {
            ConnectorConfigRecord::Telegram { config, .. } => {
                assert_eq!(config.bot_token_secret_ref.as_deref(), Some("telegram.bot"));
                assert_eq!(
                    config.ingress_mode,
                    crate::connectors::TelegramIngressMode::Webhook
                );
            }
            other => panic!("unexpected connector record: {other:?}"),
        }
        assert_eq!(
            service
                .registry()
                .telegram("bot")
                .and_then(|connector| connector.bot_token),
            Some("token-123".to_string())
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connector_service_uses_secret_ref_detects_all_connector_kinds() -> Result<()> {
        let temp = tempdir()?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;
        for (slot, value) in [
            ("telegram.bot", "telegram-token"),
            ("slack.bot", "slack-token"),
            ("slack.signing", "signing-token"),
            ("slack.team", "team-slack-token"),
            ("http.bearer", "bearer-token"),
            ("http.hmac", "hmac-token"),
            ("sidecars.webhook.routes", "routes"),
        ] {
            auth_manager
                .store_generic_secret(AuthSlotId::new(slot), value)
                .await?;
        }
        let service = ConnectorService::load(temp.path(), None, auth_manager.clone())?;
        service
            .put_telegram_connector(TelegramConnectorConfig {
                name: "bot".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: Some("telegram.bot".to_string()),
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: crate::connectors::TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            })
            .await?;
        service
            .put_slack_connector(SlackConnectorConfig {
                name: "workspace".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: Some("slack.bot".to_string()),
                signing_secret: None,
                signing_secret_env: None,
                signing_secret_secret_ref: Some("slack.signing".to_string()),
                allow_unauthenticated_ingress: false,
                api_base_url: None,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second:
                    crate::connectors::slack_default_ingress_events_per_second(),
                allowed_api_app_ids: Vec::new(),
                allowed_enterprise_ids: Vec::new(),
                allowed_team_ids: Vec::new(),
                allowed_channel_ids: Vec::new(),
                allowed_file_hosts: Vec::new(),
                team_bot_tokens: vec![crate::connectors::SlackTeamBotTokenConfig {
                    team_id: "T_TEAM".to_string(),
                    bot_token: None,
                    bot_token_env: None,
                    bot_token_secret_ref: Some("slack.team".to_string()),
                }],
            })
            .await?;
        service
            .put_http_connector(HttpInputConnectorConfig {
                name: "ingress".to_string(),
                fixed_session_id: None,
                actor_id: None,
                bearer_token: None,
                bearer_token_env: None,
                bearer_token_secret_ref: Some("http.bearer".to_string()),
                hmac_secret: None,
                hmac_secret_env: None,
                hmac_secret_secret_ref: Some("http.hmac".to_string()),
                allow_unauthenticated_ingress: false,
                require_hmac_signature: true,
                signature_max_age_secs: crate::connectors::http_default_signature_max_age_secs(),
                require_idempotency_key: true,
                ingress_events_per_second:
                    crate::connectors::http_default_ingress_events_per_second(),
                allow_payload_reply_targets: false,
                default_reply_targets: Vec::new(),
                default_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            })
            .await?;
        service
            .put_external_connector(crate::connectors::ExternalConnectorConfig {
                name: "webhook".to_string(),
                platform: "webhook".to_string(),
                mode: crate::connectors::ExternalConnectorMode::ChildProcess,
                base_url: "http://127.0.0.1:8787".to_string(),
                allow_private_network: false,
                shared_token: Some("shared".to_string()),
                shared_token_env: None,
                shared_token_secret_ref: None,
                allow_unauthenticated_ingress: false,
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
                ingress_events_per_second: 100,
                child_process: Some(crate::connectors::ExternalChildProcessConfig {
                    command: "true".to_string(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    credential_slots: BTreeMap::from([(
                        "WEBHOOK_ROUTES_JSON".to_string(),
                        "sidecars.webhook.routes".to_string(),
                    )]),
                    working_dir: None,
                }),
            })
            .await?;

        for slot in [
            "telegram.bot",
            "slack.bot",
            "slack.signing",
            "slack.team",
            "http.bearer",
            "http.hmac",
            "sidecars.webhook.routes",
        ] {
            assert!(service.uses_secret_ref(slot).await, "missing slot {slot}");
        }
        assert!(!service.uses_secret_ref("missing.slot").await);

        assert_eq!(service.disable_secret_ref_connectors("http.hmac").await?, 1);
        assert!(!service.uses_secret_ref("http.hmac").await);
        assert!(!service.uses_secret_ref("http.bearer").await);
        let reloaded = ConnectorService::load(temp.path(), None, auth_manager)?;
        assert!(
            !reloaded.uses_secret_ref("http.hmac").await,
            "secret-backed connector removal should persist across reload"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connector_service_filters_file_backed_connectors_after_secret_revocation() -> Result<()>
    {
        let temp = tempdir()?;
        let config_path = temp.path().join("connectors.toml");
        fs::write(
            &config_path,
            r#"
[[http_connectors]]
name = "ingress"
hmac_secret_secret_ref = "http.hmac"
require_hmac_signature = true
require_idempotency_key = true
"#,
        )?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;
        auth_manager
            .store_generic_secret(AuthSlotId::new("http.hmac"), "hmac-token")
            .await?;
        let service =
            ConnectorService::load(temp.path(), Some(&config_path), auth_manager.clone())?;
        assert!(service.registry().http("ingress").is_some());

        auth_manager.revoke_slot_leases(&AuthSlotId::new("http.hmac"))?;
        assert_eq!(service.disable_secret_ref_connectors("http.hmac").await?, 0);
        assert!(
            service.uses_secret_ref("http.hmac").await,
            "file-backed config remains visible to operators"
        );
        assert!(
            service.registry().http("ingress").is_none(),
            "revoked file-backed connector must be removed from the live registry"
        );

        let reloaded = ConnectorService::load(temp.path(), Some(&config_path), auth_manager)?;
        assert!(
            reloaded.registry().http("ingress").is_none(),
            "revoked file-backed connector must stay disabled across reload"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connector_service_runtime_disables_file_backed_connectors_after_lease_revocation()
    -> Result<()> {
        let temp = tempdir()?;
        let config_path = temp.path().join("connectors.toml");
        fs::write(
            &config_path,
            r#"
[[http_connectors]]
name = "ingress"
hmac_secret_secret_ref = "http.hmac"
require_hmac_signature = true
require_idempotency_key = true
"#,
        )?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;
        auth_manager
            .store_generic_secret(AuthSlotId::new("http.hmac"), "hmac-token")
            .await?;
        let service =
            ConnectorService::load(temp.path(), Some(&config_path), auth_manager.clone())?;
        assert!(service.registry().http("ingress").is_some());

        assert_eq!(service.disable_secret_ref_connectors("http.hmac").await?, 0);
        assert!(
            service.uses_secret_ref("http.hmac").await,
            "file-backed config should remain visible to operators"
        );
        assert!(
            service.registry().http("ingress").is_none(),
            "file-backed connector should leave the live registry after runtime lease revocation"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connector_service_rejects_duplicate_names_in_file_settings() -> Result<()> {
        let temp = tempdir()?;
        let config_path = temp.path().join("connectors.toml");
        fs::write(
            &config_path,
            r#"
[[telegram_connectors]]
name = "ops"
ingress_mode = "webhook"

[[telegram_connectors]]
name = "ops"
ingress_mode = "webhook"
"#,
        )?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;

        let error = match ConnectorService::load(temp.path(), Some(&config_path), auth_manager) {
            Ok(_) => anyhow::bail!("duplicate connector names should be rejected"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("duplicate telegram connector name ops"),
            "unexpected error: {error:#}"
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connector_service_rejects_deleting_a_connector_still_referenced_by_another_connector()
    -> Result<()> {
        let temp = tempdir()?;
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        let auth_manager = AuthManager::new(temp.path().join("auth/global-slots.json"))?;
        auth_manager
            .store_generic_secret(AuthSlotId::new("telegram.bot"), "telegram-token")
            .await?;
        let service = ConnectorService::load(temp.path(), None, auth_manager.clone())?;
        service
            .put_telegram_connector(TelegramConnectorConfig {
                name: "ops".to_string(),
                bot_token: None,
                bot_token_env: None,
                bot_token_secret_ref: Some("telegram.bot".to_string()),
                secret_token: None,
                secret_token_env: None,
                secret_token_secret_ref: None,
                allow_unauthenticated_ingress: true,
                api_base_url: None,
                ingress_mode: crate::connectors::TelegramIngressMode::Webhook,
                polling_timeout_seconds: 30,
                ingress_events_per_second: 100,
                allowed_chat_ids: Vec::new(),
                fixed_session_id: None,
                include_self_output: true,
                additional_reply_targets: Vec::new(),
                additional_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            })
            .await?;
        service
            .put_http_connector(HttpInputConnectorConfig {
                name: "ingress".to_string(),
                fixed_session_id: None,
                actor_id: None,
                bearer_token: None,
                bearer_token_env: None,
                bearer_token_secret_ref: None,
                hmac_secret: None,
                hmac_secret_env: None,
                hmac_secret_secret_ref: None,
                allow_unauthenticated_ingress: true,
                require_hmac_signature: false,
                signature_max_age_secs: crate::connectors::http_default_signature_max_age_secs(),
                require_idempotency_key: true,
                ingress_events_per_second:
                    crate::connectors::http_default_ingress_events_per_second(),
                allow_payload_reply_targets: false,
                default_reply_targets: vec![kheish_types::ReplyHandle {
                    plugin: "telegram".to_string(),
                    address: crate::connectors::encode_telegram_reply_route(
                        &crate::connectors::TelegramReplyRoute {
                            connector: "ops".to_string(),
                            chat_id: 42,
                            message_thread_id: None,
                            reply_to_message_id: None,
                        },
                    ),
                }],
                default_binding_keys: Vec::new(),
                session_policy: ConnectorSessionPolicy::default(),
            })
            .await?;

        let error = service
            .delete_connector("telegram", "ops")
            .await
            .expect_err("connector deletion should fail while another connector references it");
        assert!(
            error
                .to_string()
                .contains("cannot delete telegram/ops; it is still referenced by http/ingress"),
            "unexpected error: {error:#}"
        );
        assert!(service.registry().telegram("ops").is_some());
        assert_eq!(
            service
                .disable_secret_ref_connectors("telegram.bot")
                .await?,
            1
        );
        assert!(
            service.registry().telegram("ops").is_none(),
            "credential revocation should disable the connector even if reply targets reference it"
        );
        Ok(())
    }
}
