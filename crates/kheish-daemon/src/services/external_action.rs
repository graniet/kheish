//! Signed append-only audit storage for external action traces.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hmac::{Hmac, Mac};
use kheish_auth::{
    AUTH_STORE_MASTER_KEY_ENV, AUTH_STORE_MASTER_KEY_FILE_ENV, load_auth_store_master_key_from_env,
};
use kheish_runtime::{TraceEvent, TraceEventKind};
use kheish_session::{
    decode_safe_storage_name, prepare_storage_path_for_write, resolve_storage_path_for_read,
    write_json_pretty_atomically,
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

const EXTERNAL_ACTION_AUDIT_SIGNING_KEY_ENV: &str = "KHEISH_EXTERNAL_ACTION_AUDIT_SIGNING_KEY";
const EXTERNAL_ACTION_AUDIT_SIGNING_KEY_FILE_ENV: &str =
    "KHEISH_EXTERNAL_ACTION_AUDIT_SIGNING_KEY_FILE";
const GENERATED_AUDIT_KEY_FILE_NAME: &str = "audit-signing.key";
const AUDIT_ROOT_DIR_NAME: &str = "external-actions";
const AUDIT_CHECKPOINT_ROOT_DIR_NAME: &str = "external-action-checkpoints";
const ED25519_SIGNATURE_ALG: &str = "ed25519";
const LEGACY_HMAC_SIGNATURE_ALG: &str = "hmac-sha256";
const LEGACY_HMAC_KEY_ID: &str = "legacy-hmac";
const LEGACY_HMAC_AUTH_STORE_SOURCE: &str = "auth-store-master-key";

type HmacSha256 = Hmac<Sha256>;

/// One signed append-only audit entry derived from one external-action trace event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActionAuditRecord {
    /// The stable action identifier.
    pub action_id: String,
    /// The event timestamp in milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
    /// The session identifier when one session owned the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The agent identifier when one agent owned the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The daemon run identifier when one run owned the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The originating tool call identifier when one tool invocation owned the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The stable principal identifier propagated through the runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    /// The parent principal identifier when the action was delegated explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_principal_id: Option<String>,
    /// The credential grant identifier that authorized the action when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// The recorded external-action phase.
    pub phase: String,
    /// The recorded external-action kind.
    pub kind: String,
    /// The target resource touched by the action.
    pub target: String,
    /// The request digest when the action emitted one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
    /// The response digest when the action emitted one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_digest: Option<String>,
    /// The coarse outcome string when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The previous record hash within the same append-only bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    /// The canonical record hash for this entry.
    pub record_hash: String,
    /// The detached signature algorithm.
    #[serde(default = "default_signature_alg")]
    pub signature_alg: String,
    /// The signature key identifier.
    pub key_id: String,
    /// The detached signature.
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct UnsignedAuditRecord {
    timestamp_ms: u64,
    session_id: Option<String>,
    agent_id: Option<String>,
    run_id: Option<String>,
    tool_call_id: Option<String>,
    principal_id: Option<String>,
    parent_principal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    grant_id: Option<String>,
    phase: String,
    kind: String,
    target: String,
    request_digest: Option<String>,
    response_digest: Option<String>,
    outcome: Option<String>,
    prev_hash: Option<String>,
}

#[derive(Debug, Default)]
struct BucketState {
    initialized: bool,
    last_record_hash: Option<String>,
    record_count: u64,
}

struct VerifiedBucketState {
    records: Vec<ExternalActionAuditRecord>,
    last_record_hash: Option<String>,
    record_count: u64,
    needs_checkpoint_repair: bool,
}

#[derive(Debug, Serialize)]
struct SignableAuditRecord<'a> {
    action_id: &'a str,
    signature_alg: &'a str,
    key_id: &'a str,
    record_hash: &'a str,
    #[serde(flatten)]
    unsigned: &'a UnsignedAuditRecord,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ExternalActionCheckpoint {
    bucket: String,
    record_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_record_hash: Option<String>,
    signature_alg: String,
    key_id: String,
    signature: String,
}

#[derive(Debug, Serialize)]
struct SignableCheckpoint<'a> {
    bucket: &'a str,
    record_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_record_hash: Option<&'a str>,
    signature_alg: &'a str,
    key_id: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct StoredAuditSigningKey {
    algorithm: String,
    secret_key_hex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_hmac_key_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_hmac_key_source: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyHmacPersistence {
    Inline,
    AuthStoreMasterKey,
}

struct AuditSigner {
    signing_key: SigningKey,
    key_id: String,
    legacy_hmac_key: Option<Vec<u8>>,
    legacy_hmac_persistence: Option<LegacyHmacPersistence>,
}

impl AuditSigner {
    fn generate_ed25519() -> Self {
        let mut secret_key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret_key);
        Self::from_signing_key(SigningKey::from_bytes(&secret_key), None, None)
    }

    fn generate_ed25519_with_legacy_hmac(
        legacy_hmac_key: Vec<u8>,
        persistence: LegacyHmacPersistence,
    ) -> Self {
        let mut secret_key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret_key);
        Self::from_signing_key(
            SigningKey::from_bytes(&secret_key),
            Some(legacy_hmac_key),
            Some(persistence),
        )
    }

    fn from_signing_key(
        signing_key: SigningKey,
        legacy_hmac_key: Option<Vec<u8>>,
        legacy_hmac_persistence: Option<LegacyHmacPersistence>,
    ) -> Self {
        Self {
            key_id: hex::encode(signing_key.verifying_key().to_bytes()),
            signing_key,
            legacy_hmac_key,
            legacy_hmac_persistence,
        }
    }

    fn signature_alg(&self) -> &'static str {
        ED25519_SIGNATURE_ALG
    }

    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn sign(
        &self,
        action_id: &str,
        record_hash: &str,
        unsigned: &UnsignedAuditRecord,
    ) -> Result<String> {
        let payload = serde_json::to_vec(&SignableAuditRecord {
            action_id,
            signature_alg: self.signature_alg(),
            key_id: self.key_id(),
            record_hash,
            unsigned,
        })?;
        let signature = self.signing_key.sign(&payload);
        Ok(hex::encode(signature.to_bytes()))
    }

    fn verify(
        &self,
        record: &ExternalActionAuditRecord,
        unsigned: &UnsignedAuditRecord,
    ) -> Result<()> {
        let payload = serde_json::to_vec(&SignableAuditRecord {
            action_id: &record.action_id,
            signature_alg: &record.signature_alg,
            key_id: &record.key_id,
            record_hash: &record.record_hash,
            unsigned,
        })?;
        if is_legacy_hmac_signature_alg(&record.signature_alg) {
            return self.verify_legacy_payload(
                &payload,
                &record.signature,
                &record.action_id,
                "record",
            );
        }
        anyhow::ensure!(
            record.signature_alg == self.signature_alg(),
            "external action audit signature algorithm mismatch for {}",
            record.action_id
        );
        anyhow::ensure!(
            record.key_id == self.key_id(),
            "external action audit key mismatch for {}",
            record.action_id
        );
        let verifying_key: VerifyingKey = self.signing_key.verifying_key();
        let signature_bytes = hex::decode(&record.signature)
            .map_err(|_| anyhow!("invalid external action audit signature encoding"))?;
        let signature = Signature::try_from(signature_bytes.as_slice())
            .map_err(|_| anyhow!("invalid external action audit signature"))?;
        verifying_key.verify(&payload, &signature).map_err(|_| {
            anyhow!(
                "external action audit signature mismatch for {}",
                record.action_id
            )
        })?;
        Ok(())
    }

    fn sign_checkpoint(
        &self,
        bucket: &str,
        record_count: u64,
        last_record_hash: Option<&str>,
    ) -> Result<ExternalActionCheckpoint> {
        let payload = serde_json::to_vec(&SignableCheckpoint {
            bucket,
            record_count,
            last_record_hash,
            signature_alg: self.signature_alg(),
            key_id: self.key_id(),
        })?;
        let signature = hex::encode(self.signing_key.sign(&payload).to_bytes());
        Ok(ExternalActionCheckpoint {
            bucket: bucket.to_string(),
            record_count,
            last_record_hash: last_record_hash.map(ToString::to_string),
            signature_alg: self.signature_alg().to_string(),
            key_id: self.key_id().to_string(),
            signature,
        })
    }

    fn verify_checkpoint(&self, checkpoint: &ExternalActionCheckpoint) -> Result<()> {
        let payload = serde_json::to_vec(&SignableCheckpoint {
            bucket: &checkpoint.bucket,
            record_count: checkpoint.record_count,
            last_record_hash: checkpoint.last_record_hash.as_deref(),
            signature_alg: &checkpoint.signature_alg,
            key_id: &checkpoint.key_id,
        })?;
        if is_legacy_hmac_signature_alg(&checkpoint.signature_alg) {
            return self.verify_legacy_payload(
                &payload,
                &checkpoint.signature,
                &checkpoint.bucket,
                "checkpoint",
            );
        }
        anyhow::ensure!(
            checkpoint.signature_alg == self.signature_alg(),
            "external action audit checkpoint signature algorithm mismatch for {}",
            checkpoint.bucket
        );
        anyhow::ensure!(
            checkpoint.key_id == self.key_id(),
            "external action audit checkpoint key mismatch for {}",
            checkpoint.bucket
        );
        let verifying_key: VerifyingKey = self.signing_key.verifying_key();
        let signature_bytes = hex::decode(&checkpoint.signature)
            .map_err(|_| anyhow!("invalid external action audit checkpoint signature encoding"))?;
        let signature = Signature::try_from(signature_bytes.as_slice())
            .map_err(|_| anyhow!("invalid external action audit checkpoint signature"))?;
        verifying_key.verify(&payload, &signature).map_err(|_| {
            anyhow!(
                "external action audit checkpoint signature mismatch for {}",
                checkpoint.bucket
            )
        })?;
        Ok(())
    }

    fn verify_legacy_payload(
        &self,
        payload: &[u8],
        signature_hex: &str,
        label_id: &str,
        label: &str,
    ) -> Result<()> {
        let key = self.legacy_hmac_key.as_deref().ok_or_else(|| {
            anyhow!("external action audit {label} {label_id} requires a legacy HMAC migration key")
        })?;
        let signature = hex::decode(signature_hex)
            .map_err(|_| anyhow!("invalid external action audit {label} signature encoding"))?;
        let mut mac = HmacSha256::new_from_slice(key)
            .map_err(|_| anyhow!("invalid external action audit legacy HMAC key"))?;
        mac.update(payload);
        mac.verify_slice(&signature).map_err(|_| {
            anyhow!("external action audit legacy {label} signature mismatch for {label_id}")
        })
    }

    fn has_legacy_hmac_key(&self) -> bool {
        self.legacy_hmac_key.is_some()
    }

    fn stripped_legacy_hmac(&self) -> Self {
        Self::from_signing_key(self.signing_key.clone(), None, None)
    }

    fn to_stored(&self) -> StoredAuditSigningKey {
        StoredAuditSigningKey {
            algorithm: ED25519_SIGNATURE_ALG.to_string(),
            secret_key_hex: hex::encode(self.signing_key.to_bytes()),
            legacy_hmac_key_hex: match self.legacy_hmac_persistence {
                Some(LegacyHmacPersistence::Inline) => {
                    self.legacy_hmac_key.as_ref().map(hex::encode)
                }
                _ => None,
            },
            legacy_hmac_key_source: match self.legacy_hmac_persistence {
                Some(LegacyHmacPersistence::AuthStoreMasterKey) => {
                    Some(LEGACY_HMAC_AUTH_STORE_SOURCE.to_string())
                }
                _ => None,
            },
        }
    }
}

/// Durable signed audit storage used by daemon observers.
pub(crate) struct ExternalActionService {
    root: PathBuf,
    checkpoint_root: PathBuf,
    signer: AuditSigner,
    buckets: Mutex<BTreeMap<String, Arc<Mutex<BucketState>>>>,
}

impl ExternalActionService {
    /// Opens one audit service rooted under the provided daemon state directory.
    pub(crate) fn new(state_root: impl Into<PathBuf>) -> Result<Self> {
        let state_root = state_root.into();
        fs::create_dir_all(state_root.join(AUDIT_ROOT_DIR_NAME)).with_context(|| {
            format!(
                "failed to create external action audit root {}",
                state_root.join(AUDIT_ROOT_DIR_NAME).display()
            )
        })?;
        fs::create_dir_all(state_root.join(AUDIT_CHECKPOINT_ROOT_DIR_NAME)).with_context(|| {
            format!(
                "failed to create external action audit checkpoint root {}",
                state_root.join(AUDIT_CHECKPOINT_ROOT_DIR_NAME).display()
            )
        })?;
        let signer = load_or_create_signer(&state_root)?;
        Ok(Self {
            root: state_root.join(AUDIT_ROOT_DIR_NAME),
            checkpoint_root: state_root.join(AUDIT_CHECKPOINT_ROOT_DIR_NAME),
            signer,
            buckets: Mutex::new(BTreeMap::new()),
        })
    }

    /// Appends one signed audit entry when the provided trace represents one external action.
    pub(crate) fn append_trace(
        &self,
        trace: &TraceEvent,
    ) -> Result<Option<ExternalActionAuditRecord>> {
        let TraceEventKind::ExternalAction {
            phase,
            kind,
            target,
            request_digest,
            response_digest,
            outcome,
        } = &trace.kind
        else {
            return Ok(None);
        };

        let bucket = audit_bucket(trace);
        let bucket_state = self.bucket_state(&bucket)?;
        let mut state = bucket_state
            .lock()
            .map_err(|_| anyhow!("external action bucket mutex poisoned"))?;
        if !state.initialized {
            let verified = read_verified_records_with_checkpoint(
                &self.read_path_for_bucket(&bucket),
                &self.read_checkpoint_path_for_bucket(&bucket),
                &self.signer,
                &bucket,
            )?;
            state.last_record_hash = verified.last_record_hash;
            state.record_count = verified.record_count;
            state.initialized = true;
        }
        let prev_hash = state.last_record_hash.clone();
        let unsigned = UnsignedAuditRecord {
            timestamp_ms: trace.timestamp_ms,
            session_id: trace.session_id.clone(),
            agent_id: trace.agent_id.clone(),
            run_id: trace.run_id.clone(),
            tool_call_id: trace.tool_call_id.clone(),
            principal_id: trace.principal_id.clone(),
            parent_principal_id: trace.parent_principal_id.clone(),
            grant_id: trace.grant_id.clone(),
            phase: phase.clone(),
            kind: kind.clone(),
            target: target.clone(),
            request_digest: request_digest.clone(),
            response_digest: response_digest.clone(),
            outcome: outcome.clone(),
            prev_hash: prev_hash.clone(),
        };
        let record_hash = record_hash(&unsigned)?;
        let action_id = record_hash.clone();
        let signature = self.signer.sign(&action_id, &record_hash, &unsigned)?;
        let record = ExternalActionAuditRecord {
            action_id,
            timestamp_ms: unsigned.timestamp_ms,
            session_id: unsigned.session_id,
            agent_id: unsigned.agent_id,
            run_id: unsigned.run_id,
            tool_call_id: unsigned.tool_call_id,
            principal_id: unsigned.principal_id,
            parent_principal_id: unsigned.parent_principal_id,
            grant_id: unsigned.grant_id,
            phase: unsigned.phase,
            kind: unsigned.kind,
            target: unsigned.target,
            request_digest: unsigned.request_digest,
            response_digest: unsigned.response_digest,
            outcome: unsigned.outcome,
            prev_hash,
            record_hash: record_hash.clone(),
            signature_alg: self.signer.signature_alg().to_string(),
            key_id: self.signer.key_id().to_string(),
            signature,
        };
        let record_count = state.record_count + 1;
        self.append_record(&bucket, &record)?;
        state.last_record_hash = Some(record_hash);
        state.record_count = record_count;
        self.write_checkpoint(
            &bucket,
            state.record_count,
            state.last_record_hash.as_deref(),
        )?;
        Ok(Some(record))
    }

    pub(crate) fn records_for_run(&self, run_id: &str) -> Result<Vec<ExternalActionAuditRecord>> {
        let verified = self.read_verified_bucket(run_id)?;
        if verified.needs_checkpoint_repair {
            self.write_checkpoint(
                run_id,
                verified.record_count,
                verified.last_record_hash.as_deref(),
            )?;
        }
        Ok(verified.records)
    }

    fn append_record(&self, bucket: &str, record: &ExternalActionAuditRecord) -> Result<()> {
        let path = self.write_path_for_bucket(bucket)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let created = !path.exists();
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        writeln!(file, "{}", serde_json::to_string(record)?)
            .with_context(|| format!("failed to append {}", path.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to sync {}", path.display()))?;
        if created {
            sync_parent_dir(&path)?;
        }
        Ok(())
    }

    fn read_path_for_bucket(&self, bucket: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.root, bucket, "jsonl")
    }

    fn write_path_for_bucket(&self, bucket: &str) -> Result<PathBuf> {
        prepare_storage_path_for_write(&self.root, bucket, "jsonl")
    }

    fn read_checkpoint_path_for_bucket(&self, bucket: &str) -> PathBuf {
        resolve_storage_path_for_read(&self.checkpoint_root, bucket, "json")
    }

    fn write_checkpoint_path_for_bucket(&self, bucket: &str) -> Result<PathBuf> {
        prepare_storage_path_for_write(&self.checkpoint_root, bucket, "json")
    }

    fn write_checkpoint(
        &self,
        bucket: &str,
        record_count: u64,
        last_record_hash: Option<&str>,
    ) -> Result<()> {
        let checkpoint = self
            .signer
            .sign_checkpoint(bucket, record_count, last_record_hash)?;
        let path = self.write_checkpoint_path_for_bucket(bucket)?;
        write_json_atomic(&path, &checkpoint, "external action audit checkpoint")
    }

    fn bucket_state(&self, bucket: &str) -> Result<Arc<Mutex<BucketState>>> {
        let mut buckets = self
            .buckets
            .lock()
            .map_err(|_| anyhow!("external action bucket registry mutex poisoned"))?;
        Ok(buckets
            .entry(bucket.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(BucketState::default())))
            .clone())
    }

    fn read_verified_bucket(&self, bucket: &str) -> Result<VerifiedBucketState> {
        read_verified_records_with_checkpoint(
            &self.read_path_for_bucket(bucket),
            &self.read_checkpoint_path_for_bucket(bucket),
            &self.signer,
            bucket,
        )
    }
}

fn audit_bucket(trace: &TraceEvent) -> String {
    if let Some(run_id) = trace.run_id.as_deref() {
        return run_id.to_string();
    }
    if let Some(session_id) = trace.session_id.as_deref() {
        return format!("session-{session_id}");
    }
    "unscoped".to_string()
}

fn load_or_create_signer(state_root: &Path) -> Result<AuditSigner> {
    if let Some(signer) = load_explicit_signer_from_env()? {
        return Ok(signer);
    }
    let key_path = state_root.join(GENERATED_AUDIT_KEY_FILE_NAME);
    // No external signing key was configured, so the Ed25519 key that makes the audit log
    // tamper-evident lives inside the state root next to the records it signs. Anyone who can write
    // to the state directory can therefore forge and re-sign the log. Warn loudly: the tamper-
    // evidence guarantee only holds if the key is kept outside the state root.
    warn!(
        key_path = %key_path.display(),
        "no external audit signing key is configured, so external-action audit signing falls back \
         to a key under the daemon state root; a tamper-evident audit log kept next to its own \
         signing key can be forged by anyone able to write to the state directory. For a real \
         integrity guarantee, provide a signing key outside the state root via {} or {}.",
        EXTERNAL_ACTION_AUDIT_SIGNING_KEY_ENV,
        EXTERNAL_ACTION_AUDIT_SIGNING_KEY_FILE_ENV
    );
    if key_path.exists() {
        let bytes = fs::read(&key_path)
            .with_context(|| format!("failed to read {}", key_path.display()))?;
        if let Ok(signer) = load_stored_signer_from_config_bytes(&bytes) {
            return upgrade_loaded_signer(state_root, &key_path, signer, false);
        }
        let legacy_hmac_key = legacy_hmac_key_from_raw_bytes(&bytes).with_context(|| {
            format!(
                "external action signing key {} must be Ed25519 JSON or one legacy 32-byte HMAC seed",
                key_path.display()
            )
        })?;
        let signer = AuditSigner::generate_ed25519_with_legacy_hmac(
            legacy_hmac_key,
            LegacyHmacPersistence::Inline,
        );
        return upgrade_loaded_signer(state_root, &key_path, signer, true);
    }
    if audit_root_contains_records(state_root) {
        anyhow::ensure!(
            !audit_root_requires_existing_ed25519_key(state_root)?,
            "external action audit records exist but the state root is missing its Ed25519 signing key; restore {GENERATED_AUDIT_KEY_FILE_NAME} or set {EXTERNAL_ACTION_AUDIT_SIGNING_KEY_ENV}/{EXTERNAL_ACTION_AUDIT_SIGNING_KEY_FILE_ENV}"
        );
        let legacy_hmac_key = load_auth_store_master_key_from_env()?.ok_or_else(|| {
            anyhow!(
                "legacy external action audit records exist but no migration key is configured; set {AUTH_STORE_MASTER_KEY_ENV} or {AUTH_STORE_MASTER_KEY_FILE_ENV}, or restore {GENERATED_AUDIT_KEY_FILE_NAME}"
            )
        })?;
        let signer = AuditSigner::generate_ed25519_with_legacy_hmac(
            legacy_hmac_key.to_vec(),
            LegacyHmacPersistence::AuthStoreMasterKey,
        );
        return upgrade_loaded_signer(state_root, &key_path, signer, false);
    }
    let signer = AuditSigner::generate_ed25519();
    persist_signer(&key_path, &signer, false)?;
    Ok(signer)
}

fn upgrade_loaded_signer(
    state_root: &Path,
    key_path: &Path,
    signer: AuditSigner,
    replace_existing: bool,
) -> Result<AuditSigner> {
    if !signer.has_legacy_hmac_key() {
        return Ok(signer);
    }
    seal_existing_audit_root(state_root, &signer)?;
    let sealed_signer = signer.stripped_legacy_hmac();
    persist_signer(key_path, &sealed_signer, replace_existing)?;
    Ok(sealed_signer)
}

fn persist_signer(path: &Path, signer: &AuditSigner, replace_existing: bool) -> Result<()> {
    let stored = signer.to_stored();
    if replace_existing {
        return write_json_atomic(path, &stored, "external action audit signing key");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(&serde_json::to_vec_pretty(&stored)?)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    sync_parent_dir(path)?;
    Ok(())
}

fn audit_root_contains_records(state_root: &Path) -> bool {
    fn dir_contains_jsonl(dir: &Path) -> bool {
        fs::read_dir(dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    return dir_contains_jsonl(&path);
                }
                path.extension().is_some_and(|ext| ext == "jsonl")
            })
    }

    dir_contains_jsonl(&state_root.join(AUDIT_ROOT_DIR_NAME))
}

fn seal_existing_audit_root(state_root: &Path, signer: &AuditSigner) -> Result<()> {
    let audit_root = state_root.join(AUDIT_ROOT_DIR_NAME);
    let checkpoint_root = state_root.join(AUDIT_CHECKPOINT_ROOT_DIR_NAME);
    for bucket in discover_audit_buckets(&audit_root)? {
        let verified = read_verified_records_with_checkpoint(
            &resolve_storage_path_for_read(&audit_root, &bucket, "jsonl"),
            &resolve_storage_path_for_read(&checkpoint_root, &bucket, "json"),
            signer,
            &bucket,
        )?;
        if !verified.records.is_empty() || verified.needs_checkpoint_repair {
            let checkpoint_path =
                prepare_storage_path_for_write(&checkpoint_root, &bucket, "json")?;
            let checkpoint = signer.sign_checkpoint(
                &bucket,
                verified.record_count,
                verified.last_record_hash.as_deref(),
            )?;
            write_json_atomic(
                &checkpoint_path,
                &checkpoint,
                "external action audit checkpoint",
            )?;
        }
    }
    Ok(())
}

fn discover_audit_buckets(root: &Path) -> Result<Vec<String>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut buckets = BTreeSet::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|value| value.to_str()) != Some("__safe") {
                continue;
            }
            for child in
                fs::read_dir(&path).with_context(|| format!("failed to read {}", path.display()))?
            {
                let child = child?;
                let child_path = child.path();
                if !child_path
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
                {
                    continue;
                }
                let Some(stem) = child_path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                let Some(bucket) = decode_safe_storage_name(stem) else {
                    bail!(
                        "failed to decode external action audit bucket {}",
                        child_path.display()
                    );
                };
                buckets.insert(bucket);
            }
            continue;
        }
        if !path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            buckets.insert(stem.to_string());
        }
    }
    Ok(buckets.into_iter().collect())
}

fn audit_root_requires_existing_ed25519_key(state_root: &Path) -> Result<bool> {
    fn dir_requires_existing_ed25519_key(dir: &Path) -> Result<bool> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(false);
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if dir_requires_existing_ed25519_key(&path)? {
                    return Ok(true);
                }
                continue;
            }
            if !path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                continue;
            }
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            for line in content.lines().filter(|line| !line.trim().is_empty()) {
                if legacy_record_requires_existing_ed25519_key(line)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    dir_requires_existing_ed25519_key(&state_root.join(AUDIT_ROOT_DIR_NAME))
}

fn legacy_record_requires_existing_ed25519_key(line: &str) -> Result<bool> {
    let value: serde_json::Value =
        serde_json::from_str(line).context("failed to decode external action audit record")?;
    let signature_alg = value
        .get("signature_alg")
        .and_then(serde_json::Value::as_str);
    Ok(match signature_alg {
        Some(algorithm) if algorithm == ED25519_SIGNATURE_ALG => true,
        Some(algorithm) if is_legacy_hmac_signature_alg(algorithm) => false,
        Some(_) => true,
        None => false,
    })
}

fn read_verified_records_with_checkpoint(
    path: &Path,
    checkpoint_path: &Path,
    signer: &AuditSigner,
    bucket: &str,
) -> Result<VerifiedBucketState> {
    let records = read_records(path)?;
    let checkpoint = if checkpoint_path.exists() {
        let checkpoint = read_checkpoint(checkpoint_path)?;
        anyhow::ensure!(
            checkpoint.bucket == bucket,
            "external action audit checkpoint bucket mismatch for {}",
            bucket
        );
        signer.verify_checkpoint(&checkpoint)?;
        Some(checkpoint)
    } else {
        None
    };
    let checkpoint_count = checkpoint
        .as_ref()
        .map(|value| value.record_count as usize)
        .unwrap_or(0);
    anyhow::ensure!(
        records.len() >= checkpoint_count,
        "external action audit chain was truncated for {}",
        bucket
    );
    let checkpoint_seals_legacy_records = checkpoint.as_ref().is_some_and(|value| {
        value.signature_alg == signer.signature_alg() && value.key_id == signer.key_id()
    });
    let mut expected_prev_hash = None;
    for (index, record) in records.iter().enumerate() {
        anyhow::ensure!(
            record.prev_hash == expected_prev_hash,
            "external action audit chain is corrupted in {}",
            path.display()
        );
        verify_record_signature(
            record,
            signer,
            checkpoint_seals_legacy_records && index < checkpoint_count,
        )?;
        expected_prev_hash = Some(record.record_hash.clone());
    }
    if let Some(checkpoint) = checkpoint.as_ref() {
        let checkpoint_hash = if checkpoint.record_count == 0 {
            None
        } else {
            records
                .get(checkpoint.record_count as usize - 1)
                .map(|record| record.record_hash.clone())
        };
        anyhow::ensure!(
            checkpoint_hash == checkpoint.last_record_hash,
            "external action audit checkpoint does not match the ledger for {}",
            bucket
        );
    }
    let last_record_hash = expected_prev_hash;
    let record_count = records.len() as u64;
    let needs_checkpoint_repair = !records.is_empty()
        && checkpoint.as_ref().is_none_or(|value| {
            value.record_count != record_count
                || value.last_record_hash != last_record_hash
                || value.signature_alg != signer.signature_alg()
                || value.key_id != signer.key_id()
        });
    Ok(VerifiedBucketState {
        records,
        last_record_hash,
        record_count,
        needs_checkpoint_repair,
    })
}

fn read_checkpoint(path: &Path) -> Result<ExternalActionCheckpoint> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to decode {}", path.display()))?;
    serde_json::from_value(normalize_legacy_signature_fields(value))
        .with_context(|| format!("failed to decode {}", path.display()))
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::File::open(parent)
        .with_context(|| format!("failed to open directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync directory {}", parent.display()))
}

fn write_json_atomic<T>(path: &Path, value: &T, label: &str) -> Result<()>
where
    T: Serialize,
{
    write_json_pretty_atomically(path, value)
        .with_context(|| format!("failed to persist {label} {}", path.display()))
}

fn read_records(path: &Path) -> Result<Vec<ExternalActionAuditRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line)?;
            serde_json::from_value(normalize_legacy_signature_fields(value)).map_err(Into::into)
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn record_hash(unsigned: &UnsignedAuditRecord) -> Result<String> {
    Ok(sha256_hex(&serde_json::to_vec(unsigned)?))
}

fn default_signature_alg() -> String {
    ED25519_SIGNATURE_ALG.to_string()
}

fn is_legacy_hmac_signature_alg(signature_alg: &str) -> bool {
    signature_alg.eq_ignore_ascii_case(LEGACY_HMAC_SIGNATURE_ALG)
        || signature_alg.eq_ignore_ascii_case("hmac_sha256")
}

fn normalize_legacy_signature_fields(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(object) = value.as_object_mut() {
        if !object.contains_key("signature_alg") {
            object.insert(
                "signature_alg".to_string(),
                serde_json::Value::String(LEGACY_HMAC_SIGNATURE_ALG.to_string()),
            );
        }
        if !object.contains_key("key_id") {
            object.insert(
                "key_id".to_string(),
                serde_json::Value::String(LEGACY_HMAC_KEY_ID.to_string()),
            );
        }
    }
    value
}

fn verify_record_signature(
    record: &ExternalActionAuditRecord,
    signer: &AuditSigner,
    sealed_by_checkpoint: bool,
) -> Result<()> {
    let unsigned = UnsignedAuditRecord {
        timestamp_ms: record.timestamp_ms,
        session_id: record.session_id.clone(),
        agent_id: record.agent_id.clone(),
        run_id: record.run_id.clone(),
        tool_call_id: record.tool_call_id.clone(),
        principal_id: record.principal_id.clone(),
        parent_principal_id: record.parent_principal_id.clone(),
        grant_id: record.grant_id.clone(),
        phase: record.phase.clone(),
        kind: record.kind.clone(),
        target: record.target.clone(),
        request_digest: record.request_digest.clone(),
        response_digest: record.response_digest.clone(),
        outcome: record.outcome.clone(),
        prev_hash: record.prev_hash.clone(),
    };
    let expected_hash = record_hash(&unsigned)?;
    anyhow::ensure!(
        record.record_hash == expected_hash,
        "external action audit hash mismatch for {}",
        record.action_id
    );
    anyhow::ensure!(
        record.action_id == record.record_hash,
        "external action audit action id mismatch for {}",
        record.action_id
    );
    if is_legacy_hmac_signature_alg(&record.signature_alg) && !signer.has_legacy_hmac_key() {
        anyhow::ensure!(
            sealed_by_checkpoint,
            "external action audit legacy record {} requires a migration key or a sealed checkpoint",
            record.action_id
        );
        return Ok(());
    }
    signer.verify(record, &unsigned)
}

fn load_explicit_signer_from_env() -> Result<Option<AuditSigner>> {
    let file_env = std::env::var_os(EXTERNAL_ACTION_AUDIT_SIGNING_KEY_FILE_ENV);
    let value_env = std::env::var_os(EXTERNAL_ACTION_AUDIT_SIGNING_KEY_ENV);
    anyhow::ensure!(
        !(file_env.is_some() && value_env.is_some()),
        "{EXTERNAL_ACTION_AUDIT_SIGNING_KEY_ENV} and {EXTERNAL_ACTION_AUDIT_SIGNING_KEY_FILE_ENV} cannot both be set"
    );
    if let Some(path) = file_env {
        let path = PathBuf::from(path);
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        return Ok(Some(
            load_ed25519_signer_from_config_bytes(&bytes).with_context(|| {
                format!("invalid external action signing key {}", path.display())
            })?,
        ));
    }
    let Some(value) = value_env else {
        return Ok(None);
    };
    Ok(Some(load_ed25519_signer_from_config_bytes(
        value.to_string_lossy().as_bytes(),
    )?))
}

fn load_stored_signer_from_config_bytes(bytes: &[u8]) -> Result<AuditSigner> {
    load_stored_signer(serde_json::from_slice(bytes)?)
}

fn load_stored_signer(stored: StoredAuditSigningKey) -> Result<AuditSigner> {
    anyhow::ensure!(
        stored.algorithm == ED25519_SIGNATURE_ALG,
        "unsupported external action signing algorithm {}",
        stored.algorithm
    );
    let (legacy_hmac_key, legacy_hmac_persistence) = match (
        stored.legacy_hmac_key_hex.as_deref(),
        stored.legacy_hmac_key_source.as_deref(),
    ) {
        (Some(_), Some(_)) => {
            bail!(
                "external action signing key cannot declare both inline and sourced legacy HMAC material"
            )
        }
        (Some(key_hex), None) => (
            Some(hex::decode(key_hex).context("invalid inline legacy HMAC key")?),
            Some(LegacyHmacPersistence::Inline),
        ),
        (None, Some(LEGACY_HMAC_AUTH_STORE_SOURCE)) => match load_auth_store_master_key_from_env()?
        {
            Some(key) => (
                Some(key.to_vec()),
                Some(LegacyHmacPersistence::AuthStoreMasterKey),
            ),
            None => (None, None),
        },
        (None, Some(source)) => bail!("unsupported external action legacy HMAC source {source}"),
        (None, None) => (None, None),
    };
    let secret_key = hex::decode(stored.secret_key_hex)?;
    ed25519_signer_from_secret_bytes(&secret_key, legacy_hmac_key, legacy_hmac_persistence)
}

fn load_ed25519_signer_from_config_bytes(bytes: &[u8]) -> Result<AuditSigner> {
    if let Ok(signer) = load_stored_signer_from_config_bytes(bytes) {
        return Ok(signer);
    }
    let trimmed = std::str::from_utf8(bytes).unwrap_or_default().trim();
    if !trimmed.is_empty() {
        if let Ok(decoded) = hex::decode(trimmed) {
            return ed25519_signer_from_secret_bytes(&decoded, Some(decoded.clone()), None);
        }
        if let Ok(decoded) = URL_SAFE_NO_PAD.decode(trimmed) {
            return ed25519_signer_from_secret_bytes(&decoded, Some(decoded.clone()), None);
        }
        if let Ok(decoded) = STANDARD.decode(trimmed) {
            return ed25519_signer_from_secret_bytes(&decoded, Some(decoded.clone()), None);
        }
    }
    ed25519_signer_from_secret_bytes(bytes, Some(bytes.to_vec()), None)
}

fn legacy_hmac_key_from_raw_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(
        bytes.len() == 32,
        "legacy external action HMAC key must be exactly 32 bytes"
    );
    Ok(bytes.to_vec())
}

fn ed25519_signer_from_secret_bytes(
    secret_key: &[u8],
    legacy_hmac_key: Option<Vec<u8>>,
    legacy_hmac_persistence: Option<LegacyHmacPersistence>,
) -> Result<AuditSigner> {
    anyhow::ensure!(
        secret_key.len() == 32,
        "external action Ed25519 signing key must be exactly 32 bytes"
    );
    let signing_key = SigningKey::from_bytes(
        &secret_key
            .try_into()
            .map_err(|_| anyhow!("invalid external action Ed25519 signing key"))?,
    );
    Ok(AuditSigner::from_signing_key(
        signing_key,
        legacy_hmac_key,
        legacy_hmac_persistence,
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    use anyhow::{Result, anyhow};
    use hmac::Mac;
    use kheish_auth::{AUTH_STORE_MASTER_KEY_ENV, AUTH_STORE_MASTER_KEY_FILE_ENV};
    use kheish_session::resolve_storage_path_for_read;
    use tempfile::tempdir;

    use kheish_runtime::{TraceEvent, TraceEventKind};

    use super::{
        AUDIT_CHECKPOINT_ROOT_DIR_NAME, AUDIT_ROOT_DIR_NAME, ED25519_SIGNATURE_ALG,
        ExternalActionAuditRecord, ExternalActionCheckpoint, ExternalActionService,
        GENERATED_AUDIT_KEY_FILE_NAME, HmacSha256, LEGACY_HMAC_KEY_ID, LEGACY_HMAC_SIGNATURE_ALG,
        SignableAuditRecord, SignableCheckpoint, StoredAuditSigningKey, UnsignedAuditRecord,
        read_records, record_hash,
    };

    fn auth_store_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct AuthStoreEnvRestoreGuard {
        master: Option<OsString>,
        master_file: Option<OsString>,
    }

    impl AuthStoreEnvRestoreGuard {
        fn capture() -> Self {
            Self {
                master: std::env::var_os(AUTH_STORE_MASTER_KEY_ENV),
                master_file: std::env::var_os(AUTH_STORE_MASTER_KEY_FILE_ENV),
            }
        }
    }

    impl Drop for AuthStoreEnvRestoreGuard {
        fn drop(&mut self) {
            unsafe {
                match self.master.as_ref() {
                    Some(value) => std::env::set_var(AUTH_STORE_MASTER_KEY_ENV, value),
                    None => std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV),
                }
                match self.master_file.as_ref() {
                    Some(value) => std::env::set_var(AUTH_STORE_MASTER_KEY_FILE_ENV, value),
                    None => std::env::remove_var(AUTH_STORE_MASTER_KEY_FILE_ENV),
                }
            }
        }
    }

    fn sign_legacy_hmac(payload: &[u8], key: &[u8]) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(key)?;
        mac.update(payload);
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    fn make_legacy_record(
        unsigned: UnsignedAuditRecord,
        key: &[u8],
    ) -> Result<ExternalActionAuditRecord> {
        let record_hash = record_hash(&unsigned)?;
        let action_id = record_hash.clone();
        let payload = serde_json::to_vec(&SignableAuditRecord {
            action_id: &action_id,
            signature_alg: LEGACY_HMAC_SIGNATURE_ALG,
            key_id: LEGACY_HMAC_KEY_ID,
            record_hash: &record_hash,
            unsigned: &unsigned,
        })?;
        let signature = sign_legacy_hmac(&payload, key)?;
        Ok(ExternalActionAuditRecord {
            action_id,
            timestamp_ms: unsigned.timestamp_ms,
            session_id: unsigned.session_id,
            agent_id: unsigned.agent_id,
            run_id: unsigned.run_id,
            tool_call_id: unsigned.tool_call_id,
            principal_id: unsigned.principal_id,
            parent_principal_id: unsigned.parent_principal_id,
            grant_id: unsigned.grant_id,
            phase: unsigned.phase,
            kind: unsigned.kind,
            target: unsigned.target,
            request_digest: unsigned.request_digest,
            response_digest: unsigned.response_digest,
            outcome: unsigned.outcome,
            prev_hash: unsigned.prev_hash,
            record_hash,
            signature_alg: LEGACY_HMAC_SIGNATURE_ALG.to_string(),
            key_id: LEGACY_HMAC_KEY_ID.to_string(),
            signature,
        })
    }

    fn make_legacy_checkpoint(
        bucket: &str,
        record_count: u64,
        last_record_hash: Option<&str>,
        key: &[u8],
    ) -> Result<ExternalActionCheckpoint> {
        let payload = serde_json::to_vec(&SignableCheckpoint {
            bucket,
            record_count,
            last_record_hash,
            signature_alg: LEGACY_HMAC_SIGNATURE_ALG,
            key_id: LEGACY_HMAC_KEY_ID,
        })?;
        Ok(ExternalActionCheckpoint {
            bucket: bucket.to_string(),
            record_count,
            last_record_hash: last_record_hash.map(ToString::to_string),
            signature_alg: LEGACY_HMAC_SIGNATURE_ALG.to_string(),
            key_id: LEGACY_HMAC_KEY_ID.to_string(),
            signature: sign_legacy_hmac(&payload, key)?,
        })
    }

    fn write_legacy_run(
        state_root: &std::path::Path,
        run_id: &str,
        key: &[u8],
    ) -> Result<ExternalActionAuditRecord> {
        fs::create_dir_all(state_root.join(AUDIT_ROOT_DIR_NAME))?;
        fs::create_dir_all(state_root.join(AUDIT_CHECKPOINT_ROOT_DIR_NAME))?;
        let record = make_legacy_record(
            UnsignedAuditRecord {
                timestamp_ms: 1,
                session_id: Some("session-legacy".to_string()),
                agent_id: Some("agent-legacy".to_string()),
                run_id: Some(run_id.to_string()),
                tool_call_id: Some("tool-call-legacy".to_string()),
                principal_id: Some("agent:agent-legacy".to_string()),
                parent_principal_id: Some("session:session-legacy".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                phase: "request".to_string(),
                kind: "model_provider".to_string(),
                target: "openai:https://api.openai.com/v1/responses".to_string(),
                request_digest: Some("legacy-req".to_string()),
                response_digest: None,
                outcome: None,
                prev_hash: None,
            },
            key,
        )?;
        let audit_path =
            resolve_storage_path_for_read(&state_root.join(AUDIT_ROOT_DIR_NAME), run_id, "jsonl");
        if let Some(parent) = audit_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &audit_path,
            format!("{}\n", serde_json::to_string(&record)?),
        )?;
        let checkpoint = make_legacy_checkpoint(run_id, 1, Some(record.record_hash.as_str()), key)?;
        let checkpoint_path = resolve_storage_path_for_read(
            &state_root.join(AUDIT_CHECKPOINT_ROOT_DIR_NAME),
            run_id,
            "json",
        );
        if let Some(parent) = checkpoint_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&checkpoint_path, serde_json::to_vec_pretty(&checkpoint)?)?;
        Ok(record)
    }

    #[test]
    fn external_action_records_are_chained_and_signed() -> Result<()> {
        let temp = tempdir()?;
        let service = ExternalActionService::new(temp.path())?;
        let first = service
            .append_trace(&TraceEvent {
                timestamp_ms: 1,
                session_id: Some("session-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                run_id: Some("run-1".to_string()),
                tool_call_id: Some("tool-call-1".to_string()),
                principal_id: Some("agent:agent-1".to_string()),
                parent_principal_id: Some("session:session-1".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "request".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: Some("req-a".to_string()),
                    response_digest: None,
                    outcome: None,
                },
            })?
            .expect("external action should be recorded");
        let second = service
            .append_trace(&TraceEvent {
                timestamp_ms: 2,
                session_id: Some("session-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                run_id: Some("run-1".to_string()),
                tool_call_id: Some("tool-call-1".to_string()),
                principal_id: Some("agent:agent-1".to_string()),
                parent_principal_id: Some("session:session-1".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "response".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: None,
                    response_digest: Some("resp-a".to_string()),
                    outcome: Some("200".to_string()),
                },
            })?
            .expect("external action should be recorded");

        assert!(first.prev_hash.is_none());
        assert_eq!(first.signature_alg, ED25519_SIGNATURE_ALG);
        assert_eq!(first.tool_call_id.as_deref(), Some("tool-call-1"));
        assert_eq!(first.grant_id.as_deref(), Some("grant-route-openai"));
        assert_eq!(
            second.prev_hash.as_deref(),
            Some(first.record_hash.as_str())
        );

        let records = service.records_for_run("run-1")?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], first);
        assert_eq!(records[1], second);
        Ok(())
    }

    #[test]
    fn external_action_records_use_safe_storage_bucket_paths() -> Result<()> {
        let temp = tempdir()?;
        let service = ExternalActionService::new(temp.path())?;
        service.append_trace(&TraceEvent {
            timestamp_ms: 1,
            session_id: Some("session-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            run_id: Some("../unsafe/run".to_string()),
            tool_call_id: None,
            principal_id: Some("agent:agent-1".to_string()),
            parent_principal_id: None,
            grant_id: None,
            kind: TraceEventKind::ExternalAction {
                phase: "request".to_string(),
                kind: "model_provider".to_string(),
                target: "openai:https://api.openai.com".to_string(),
                request_digest: Some("req-a".to_string()),
                response_digest: None,
                outcome: None,
            },
        })?;

        let safe_path = resolve_storage_path_for_read(
            &temp.path().join("external-actions"),
            "../unsafe/run",
            "jsonl",
        );
        assert!(
            safe_path.starts_with(temp.path().join("external-actions").join("__safe")),
            "audit path must stay in the safe namespace: {}",
            safe_path.display()
        );
        assert!(safe_path.exists());
        assert_eq!(service.records_for_run("../unsafe/run")?.len(), 1);
        Ok(())
    }

    #[test]
    fn external_action_append_rejects_tampered_tail() -> Result<()> {
        let temp = tempdir()?;
        let service = ExternalActionService::new(temp.path())?;
        let _ = service.append_trace(&TraceEvent {
            timestamp_ms: 1,
            session_id: Some("session-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            run_id: Some("run-1".to_string()),
            tool_call_id: None,
            principal_id: Some("agent:agent-1".to_string()),
            parent_principal_id: Some("session:session-1".to_string()),
            grant_id: None,
            kind: TraceEventKind::ExternalAction {
                phase: "request".to_string(),
                kind: "model_provider".to_string(),
                target: "openai:https://api.openai.com/v1/responses".to_string(),
                request_digest: Some("req-a".to_string()),
                response_digest: None,
                outcome: None,
            },
        })?;
        let audit_path =
            resolve_storage_path_for_read(&temp.path().join("external-actions"), "run-1", "jsonl");
        let mut records = read_records(&audit_path)?;
        records[0].record_hash = "tampered".to_string();
        fs::write(
            &audit_path,
            format!("{}\n", serde_json::to_string(&records[0])?),
        )?;

        let restarted = ExternalActionService::new(temp.path())?;
        let error = restarted
            .append_trace(&TraceEvent {
                timestamp_ms: 2,
                session_id: Some("session-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                run_id: Some("run-1".to_string()),
                tool_call_id: None,
                principal_id: Some("agent:agent-1".to_string()),
                parent_principal_id: Some("session:session-1".to_string()),
                grant_id: None,
                kind: TraceEventKind::ExternalAction {
                    phase: "response".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: None,
                    response_digest: Some("resp-a".to_string()),
                    outcome: Some("200".to_string()),
                },
            })
            .expect_err("tampered audit tails should fail closed");
        let message = error.to_string();
        assert!(
            message.contains("hash mismatch") || message.contains("signature mismatch"),
            "unexpected tamper error: {message}"
        );
        Ok(())
    }

    #[test]
    fn external_action_checkpoint_rejects_truncated_ledgers_after_restart() -> Result<()> {
        let temp = tempdir()?;
        let service = ExternalActionService::new(temp.path())?;
        for timestamp_ms in [1, 2] {
            service.append_trace(&TraceEvent {
                timestamp_ms,
                session_id: Some("session-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                run_id: Some("run-truncated".to_string()),
                tool_call_id: None,
                principal_id: Some("agent:agent-1".to_string()),
                parent_principal_id: Some("session:session-1".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "request".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: Some(format!("req-{timestamp_ms}")),
                    response_digest: None,
                    outcome: None,
                },
            })?;
        }
        let audit_path = resolve_storage_path_for_read(
            &temp.path().join("external-actions"),
            "run-truncated",
            "jsonl",
        );
        let original = fs::read_to_string(&audit_path)?;
        let first_line = original
            .lines()
            .next()
            .ok_or_else(|| anyhow!("expected audit record"))?;
        fs::write(&audit_path, format!("{first_line}\n"))?;

        let restarted = ExternalActionService::new(temp.path())?;
        let error = restarted
            .records_for_run("run-truncated")
            .expect_err("truncated signed audit ledgers should fail closed");
        assert!(
            error.to_string().contains("truncated"),
            "unexpected truncation error: {error}"
        );
        Ok(())
    }

    #[test]
    fn external_action_append_recovers_when_checkpoint_lags_ledger() -> Result<()> {
        let temp = tempdir()?;
        let service = ExternalActionService::new(temp.path())?;
        let first = service
            .append_trace(&TraceEvent {
                timestamp_ms: 1,
                session_id: Some("session-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                run_id: Some("run-repair".to_string()),
                tool_call_id: None,
                principal_id: Some("agent:agent-1".to_string()),
                parent_principal_id: Some("session:session-1".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "request".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: Some("req-1".to_string()),
                    response_digest: None,
                    outcome: None,
                },
            })?
            .expect("first audit record should be stored");
        let _second = service
            .append_trace(&TraceEvent {
                timestamp_ms: 2,
                session_id: Some("session-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                run_id: Some("run-repair".to_string()),
                tool_call_id: None,
                principal_id: Some("agent:agent-1".to_string()),
                parent_principal_id: Some("session:session-1".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "response".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: None,
                    response_digest: Some("resp-2".to_string()),
                    outcome: Some("200".to_string()),
                },
            })?
            .expect("second audit record should be stored");
        let checkpoint_path = resolve_storage_path_for_read(
            &temp.path().join(AUDIT_CHECKPOINT_ROOT_DIR_NAME),
            "run-repair",
            "json",
        );
        let stale_checkpoint =
            service
                .signer
                .sign_checkpoint("run-repair", 1, Some(first.record_hash.as_str()))?;
        fs::write(
            &checkpoint_path,
            serde_json::to_vec_pretty(&stale_checkpoint)?,
        )?;
        drop(service);

        let restarted = ExternalActionService::new(temp.path())?;
        let third = restarted
            .append_trace(&TraceEvent {
                timestamp_ms: 3,
                session_id: Some("session-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                run_id: Some("run-repair".to_string()),
                tool_call_id: None,
                principal_id: Some("agent:agent-1".to_string()),
                parent_principal_id: Some("session:session-1".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "response".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: None,
                    response_digest: Some("resp-3".to_string()),
                    outcome: Some("200".to_string()),
                },
            })?
            .expect("stale checkpoints should not block append");
        let records = restarted.records_for_run("run-repair")?;
        assert_eq!(records.len(), 3);
        assert_eq!(records[2], third);
        let repaired: ExternalActionCheckpoint =
            serde_json::from_slice(&fs::read(&checkpoint_path)?)?;
        assert_eq!(repaired.record_count, 3);
        assert_eq!(
            repaired.last_record_hash.as_deref(),
            Some(records[2].record_hash.as_str())
        );
        assert_eq!(repaired.signature_alg, ED25519_SIGNATURE_ALG);
        Ok(())
    }

    #[test]
    fn external_action_records_for_run_backfill_missing_checkpoints() -> Result<()> {
        let temp = tempdir()?;
        let service = ExternalActionService::new(temp.path())?;
        let _record = service
            .append_trace(&TraceEvent {
                timestamp_ms: 1,
                session_id: Some("session-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                run_id: Some("run-missing-checkpoint".to_string()),
                tool_call_id: None,
                principal_id: Some("agent:agent-1".to_string()),
                parent_principal_id: Some("session:session-1".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "request".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: Some("req-1".to_string()),
                    response_digest: None,
                    outcome: None,
                },
            })?
            .expect("audit record should be stored");
        let checkpoint_path = resolve_storage_path_for_read(
            &temp.path().join(AUDIT_CHECKPOINT_ROOT_DIR_NAME),
            "run-missing-checkpoint",
            "json",
        );
        fs::remove_file(&checkpoint_path)?;
        drop(service);

        let restarted = ExternalActionService::new(temp.path())?;
        let records = restarted.records_for_run("run-missing-checkpoint")?;
        assert_eq!(records.len(), 1);
        let repaired: ExternalActionCheckpoint =
            serde_json::from_slice(&fs::read(&checkpoint_path)?)?;
        assert_eq!(repaired.record_count, 1);
        assert_eq!(
            repaired.last_record_hash.as_deref(),
            Some(records[0].record_hash.as_str())
        );
        assert_eq!(repaired.signature_alg, ED25519_SIGNATURE_ALG);
        Ok(())
    }

    #[test]
    fn external_action_service_migrates_legacy_hmac_key_files() -> Result<()> {
        let temp = tempdir()?;
        let legacy_key = [7u8; 32];
        let legacy_record = write_legacy_run(temp.path(), "run-legacy-key-file", &legacy_key)?;
        fs::write(temp.path().join(GENERATED_AUDIT_KEY_FILE_NAME), legacy_key)?;

        let service = ExternalActionService::new(temp.path())?;
        let stored: StoredAuditSigningKey =
            serde_json::from_slice(&fs::read(temp.path().join(GENERATED_AUDIT_KEY_FILE_NAME))?)?;
        assert_eq!(stored.algorithm, ED25519_SIGNATURE_ALG);
        assert!(stored.legacy_hmac_key_hex.is_none());
        assert!(stored.legacy_hmac_key_source.is_none());

        let appended = service
            .append_trace(&TraceEvent {
                timestamp_ms: 2,
                session_id: Some("session-legacy".to_string()),
                agent_id: Some("agent-legacy".to_string()),
                run_id: Some("run-legacy-key-file".to_string()),
                tool_call_id: Some("tool-call-legacy".to_string()),
                principal_id: Some("agent:agent-legacy".to_string()),
                parent_principal_id: Some("session:session-legacy".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "response".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: None,
                    response_digest: Some("legacy-resp".to_string()),
                    outcome: Some("200".to_string()),
                },
            })?
            .expect("legacy bucket append should succeed");
        assert_eq!(appended.signature_alg, ED25519_SIGNATURE_ALG);

        let records = service.records_for_run("run-legacy-key-file")?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], legacy_record);
        assert_eq!(records[0].signature_alg, LEGACY_HMAC_SIGNATURE_ALG);
        assert_eq!(records[1].signature_alg, ED25519_SIGNATURE_ALG);
        let checkpoint_path = resolve_storage_path_for_read(
            &temp.path().join(AUDIT_CHECKPOINT_ROOT_DIR_NAME),
            "run-legacy-key-file",
            "json",
        );
        let checkpoint: ExternalActionCheckpoint =
            serde_json::from_slice(&fs::read(&checkpoint_path)?)?;
        assert_eq!(checkpoint.signature_alg, ED25519_SIGNATURE_ALG);

        drop(service);
        let restarted = ExternalActionService::new(temp.path())?;
        assert_eq!(restarted.records_for_run("run-legacy-key-file")?.len(), 2);
        Ok(())
    }

    #[test]
    fn external_action_service_migrates_legacy_records_without_key_using_auth_store_master_key()
    -> Result<()> {
        let _env_guard = auth_store_env_lock()
            .lock()
            .expect("auth store env mutex poisoned");
        let temp = tempdir()?;
        let legacy_key = b"0123456789abcdef0123456789abcdef";
        let legacy_record = write_legacy_run(temp.path(), "run-legacy-no-key", legacy_key)?;
        let _restore = AuthStoreEnvRestoreGuard::capture();
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                String::from_utf8_lossy(legacy_key).as_ref(),
            );
            std::env::remove_var(AUTH_STORE_MASTER_KEY_FILE_ENV);
        }

        let service = ExternalActionService::new(temp.path())?;
        let stored: StoredAuditSigningKey =
            serde_json::from_slice(&fs::read(temp.path().join(GENERATED_AUDIT_KEY_FILE_NAME))?)?;
        assert_eq!(stored.algorithm, ED25519_SIGNATURE_ALG);
        assert!(stored.legacy_hmac_key_hex.is_none());
        assert!(stored.legacy_hmac_key_source.is_none());

        let appended = service
            .append_trace(&TraceEvent {
                timestamp_ms: 2,
                session_id: Some("session-legacy".to_string()),
                agent_id: Some("agent-legacy".to_string()),
                run_id: Some("run-legacy-no-key".to_string()),
                tool_call_id: Some("tool-call-legacy".to_string()),
                principal_id: Some("agent:agent-legacy".to_string()),
                parent_principal_id: Some("session:session-legacy".to_string()),
                grant_id: Some("grant-route-openai".to_string()),
                kind: TraceEventKind::ExternalAction {
                    phase: "response".to_string(),
                    kind: "model_provider".to_string(),
                    target: "openai:https://api.openai.com/v1/responses".to_string(),
                    request_digest: None,
                    response_digest: Some("legacy-resp".to_string()),
                    outcome: Some("200".to_string()),
                },
            })?
            .expect("legacy bucket append should succeed");
        assert_eq!(appended.signature_alg, ED25519_SIGNATURE_ALG);
        let records = service.records_for_run("run-legacy-no-key")?;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], legacy_record);
        assert_eq!(records[0].signature_alg, LEGACY_HMAC_SIGNATURE_ALG);
        let checkpoint_path = resolve_storage_path_for_read(
            &temp.path().join(AUDIT_CHECKPOINT_ROOT_DIR_NAME),
            "run-legacy-no-key",
            "json",
        );
        let checkpoint: ExternalActionCheckpoint =
            serde_json::from_slice(&fs::read(&checkpoint_path)?)?;
        assert_eq!(checkpoint.signature_alg, ED25519_SIGNATURE_ALG);

        drop(service);
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
            std::env::remove_var(AUTH_STORE_MASTER_KEY_FILE_ENV);
        }
        let restarted = ExternalActionService::new(temp.path())?;
        assert_eq!(restarted.records_for_run("run-legacy-no-key")?.len(), 2);
        Ok(())
    }
}
