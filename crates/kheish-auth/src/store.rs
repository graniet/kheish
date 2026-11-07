use aes_gcm_siv::aead::{Aead, KeyInit, Payload};
use aes_gcm_siv::{Aes256GcmSiv, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rand::RngCore;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::{AuthSlotRecord, AuthStoreSnapshot};

/// Environment variable containing the master key used to encrypt auth stores at rest.
///
/// The value must be either:
/// - 32 raw UTF-8 bytes, or
/// - 32 random bytes encoded as base64.
pub const AUTH_STORE_MASTER_KEY_ENV: &str = "KHEISH_AUTH_STORE_MASTER_KEY";
/// Environment variable containing a file path whose contents hold the auth-store master key.
pub const AUTH_STORE_MASTER_KEY_FILE_ENV: &str = "KHEISH_AUTH_STORE_MASTER_KEY_FILE";
/// Environment variable that, when truthy, makes loading a plaintext auth
/// store a hard error instead of a warning followed by an encrypted rewrite.
pub const AUTH_STORE_REQUIRE_ENCRYPTED_ENV: &str = "KHEISH_AUTH_STORE_REQUIRE_ENCRYPTED";

/// The original envelope: AES-256-GCM-SIV without associated data. Still
/// read, never written.
const AUTH_STORE_ENVELOPE_VERSION_V1: u32 = 1;
/// The current envelope: AES-256-GCM-SIV whose associated data binds the
/// envelope version and label, so neither can be swapped without failing
/// authentication. The file path is deliberately not bound: a store must
/// stay readable after the state root moves.
const AUTH_STORE_ENVELOPE_VERSION_V2: u32 = 2;
/// The label of the global auth store envelope.
const AUTH_STORE_ENVELOPE_LABEL: &str = "kheish.auth-store";
const AUTH_STORE_NONCE_LEN: usize = 12;

/// Associated data of one v2 envelope: a fixed domain prefix, the version,
/// and the label, length-prefixed so no two (version, label) pairs collide.
fn auth_store_envelope_aad(version: u32, label: &str) -> Vec<u8> {
    let mut aad = b"kheish-auth-store-envelope".to_vec();
    aad.extend_from_slice(&version.to_be_bytes());
    aad.extend_from_slice(&(label.len() as u64).to_be_bytes());
    aad.extend_from_slice(label.as_bytes());
    aad
}

/// Reads `KHEISH_AUTH_STORE_REQUIRE_ENCRYPTED`. Unset or empty means `false`.
pub fn auth_store_requires_encryption_from_env() -> Result<bool> {
    let Some(raw) = std::env::var_os(AUTH_STORE_REQUIRE_ENCRYPTED_ENV) else {
        return Ok(false);
    };
    let raw = raw
        .into_string()
        .map_err(|_| anyhow!("{AUTH_STORE_REQUIRE_ENCRYPTED_ENV} must be valid UTF-8"))?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        other => bail!(
            "{AUTH_STORE_REQUIRE_ENCRYPTED_ENV} must be true or false (1/0, yes/no, on/off), got `{other}`"
        ),
    }
}

fn decode_auth_store_master_key_bytes(raw: &str) -> Result<[u8; 32]> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{AUTH_STORE_MASTER_KEY_ENV} cannot be empty");
    }
    if let Ok(decoded) = BASE64_STANDARD.decode(trimmed) {
        if decoded.len() == 32 {
            let mut key = [0_u8; 32];
            key.copy_from_slice(&decoded);
            return Ok(key);
        }
    }
    if trimmed.as_bytes().len() == 32 {
        let mut key = [0_u8; 32];
        key.copy_from_slice(trimmed.as_bytes());
        return Ok(key);
    }
    bail!("{AUTH_STORE_MASTER_KEY_ENV} must be exactly 32 raw bytes or base64-encoded 32 bytes")
}

/// Parses one auth-store master key value from raw operator input.
///
/// The input may be either:
/// - exactly 32 UTF-8 bytes, or
/// - a base64-encoded 32-byte payload.
pub fn parse_auth_store_master_key(raw: &str) -> Result<[u8; 32]> {
    decode_auth_store_master_key_bytes(raw)
}

/// Loads the auth-store master key from `KHEISH_AUTH_STORE_MASTER_KEY` or
/// `KHEISH_AUTH_STORE_MASTER_KEY_FILE` when present.
pub fn load_auth_store_master_key_from_env() -> Result<Option<[u8; 32]>> {
    let raw = std::env::var_os(AUTH_STORE_MASTER_KEY_ENV);
    let raw_file = std::env::var_os(AUTH_STORE_MASTER_KEY_FILE_ENV);
    match (raw, raw_file) {
        (Some(_), Some(_)) => bail!(
            "{AUTH_STORE_MASTER_KEY_ENV} and {AUTH_STORE_MASTER_KEY_FILE_ENV} are mutually exclusive"
        ),
        (Some(raw), None) => {
            let raw = raw
                .into_string()
                .map_err(|_| anyhow!("{AUTH_STORE_MASTER_KEY_ENV} must be valid UTF-8"))?;
            parse_auth_store_master_key(&raw).map(Some)
        }
        (None, Some(path)) => {
            let path = PathBuf::from(path);
            let raw = std::fs::read_to_string(&path).with_context(|| {
                format!(
                    "failed to read auth-store master key file {}",
                    path.display()
                )
            })?;
            parse_auth_store_master_key(&raw).map(Some)
        }
        (None, None) => Ok(None),
    }
}

/// Generates one new base64-encoded auth-store master key.
pub fn generate_auth_store_master_key_base64() -> String {
    let mut key = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    BASE64_STANDARD.encode(key)
}

#[cfg(test)]
pub(crate) fn auth_store_env_lock() -> &'static parking_lot::Mutex<()> {
    static LOCK: std::sync::OnceLock<parking_lot::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| parking_lot::Mutex::new(()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct EncryptedAuthStoreEnvelope {
    version: u32,
    /// Present from v2 on, and bound into the associated data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    nonce: String,
    ciphertext: String,
}

#[derive(Clone, Debug)]
pub struct FileAuthStore {
    path: PathBuf,
}

impl FileAuthStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<AuthStoreSnapshot> {
        if !self.path.exists() {
            return Ok(AuthStoreSnapshot::default());
        }
        let content = std::fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read auth store {}", self.path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse auth store {}", self.path.display()))?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("auth store {} must be a JSON object", self.path.display()))?;
        if object.contains_key("ciphertext") {
            let envelope: EncryptedAuthStoreEnvelope =
                serde_json::from_value(value).with_context(|| {
                    format!(
                        "failed to decode encrypted auth store {}",
                        self.path.display()
                    )
                })?;
            return self.decrypt_snapshot(&envelope);
        }
        if auth_store_requires_encryption_from_env()? {
            bail!(
                "auth store {} holds plaintext credentials and {} is set; encrypt it with {} before starting",
                self.path.display(),
                AUTH_STORE_REQUIRE_ENCRYPTED_ENV,
                AUTH_STORE_MASTER_KEY_ENV
            );
        }
        let snapshot: AuthStoreSnapshot =
            serde_json::from_value(serde_json::Value::Object(object.clone()))
                .with_context(|| format!("failed to decode auth store {}", self.path.display()))?;
        self.migrate_plaintext(&snapshot);
        Ok(snapshot)
    }

    /// Rewrites a plaintext store encrypted when a master key is available,
    /// and warns either way. A failed rewrite leaves the plaintext file in
    /// place and is reported, never fatal: the credentials stay usable.
    fn migrate_plaintext(&self, snapshot: &AuthStoreSnapshot) {
        match self.master_key() {
            Ok(Some(key)) => {
                let written = self.encrypt_snapshot(snapshot, &key).and_then(|envelope| {
                    let content = serde_json::to_string_pretty(&envelope)?;
                    atomic_write_private(&self.path, content.as_bytes())
                });
                match written {
                    Ok(()) => tracing::warn!(
                        path = %self.path.display(),
                        "auth store held plaintext credentials; rewrote it encrypted"
                    ),
                    Err(error) => tracing::warn!(
                        path = %self.path.display(),
                        error = %format!("{error:#}"),
                        "auth store holds plaintext credentials and could not be rewritten encrypted"
                    ),
                }
            }
            Ok(None) => tracing::warn!(
                path = %self.path.display(),
                "auth store holds plaintext credentials; set {} to encrypt it, or {} to refuse plaintext",
                AUTH_STORE_MASTER_KEY_ENV,
                AUTH_STORE_REQUIRE_ENCRYPTED_ENV
            ),
            Err(error) => tracing::warn!(
                path = %self.path.display(),
                error = %format!("{error:#}"),
                "auth store holds plaintext credentials and the master key could not be read"
            ),
        }
    }

    pub fn save(&self, snapshot: &AuthStoreSnapshot) -> Result<()> {
        let key = self.required_master_key()?;
        let envelope = self.encrypt_snapshot(snapshot, &key)?;
        let content = serde_json::to_string_pretty(&envelope)?;
        atomic_write_private(&self.path, content.as_bytes())
            .with_context(|| format!("failed to write auth store {}", self.path.display()))?;
        Ok(())
    }

    pub fn save_records(&self, records: impl IntoIterator<Item = AuthSlotRecord>) -> Result<()> {
        let snapshot = AuthStoreSnapshot {
            slots: records
                .into_iter()
                .map(|record| (record.slot_id.0.clone(), record))
                .collect(),
        };
        self.save(&snapshot)
    }

    fn master_key(&self) -> Result<Option<[u8; 32]>> {
        load_auth_store_master_key_from_env()
    }

    fn required_master_key(&self) -> Result<[u8; 32]> {
        self.master_key()?.ok_or_else(|| {
            anyhow!(
                "{} or {} must be set before writing auth store {}",
                AUTH_STORE_MASTER_KEY_ENV,
                AUTH_STORE_MASTER_KEY_FILE_ENV,
                self.path.display()
            )
        })
    }

    /// Encrypts one snapshot into a v2 envelope.
    fn encrypt_snapshot(
        &self,
        snapshot: &AuthStoreSnapshot,
        key: &[u8; 32],
    ) -> Result<EncryptedAuthStoreEnvelope> {
        let cipher = Aes256GcmSiv::new_from_slice(key).expect("32-byte key should be valid");
        let plaintext = serde_json::to_vec(snapshot)?;
        let mut nonce_bytes = [0_u8; AUTH_STORE_NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let aad =
            auth_store_envelope_aad(AUTH_STORE_ENVELOPE_VERSION_V2, AUTH_STORE_ENVELOPE_LABEL);
        let ciphertext = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext.as_ref(),
                    aad: &aad,
                },
            )
            .map_err(|error| anyhow!("failed to encrypt auth store: {error}"))?;
        Ok(EncryptedAuthStoreEnvelope {
            version: AUTH_STORE_ENVELOPE_VERSION_V2,
            label: Some(AUTH_STORE_ENVELOPE_LABEL.to_string()),
            nonce: BASE64_STANDARD.encode(nonce_bytes),
            ciphertext: BASE64_STANDARD.encode(ciphertext),
        })
    }

    fn decrypt_snapshot(&self, envelope: &EncryptedAuthStoreEnvelope) -> Result<AuthStoreSnapshot> {
        match envelope.version {
            AUTH_STORE_ENVELOPE_VERSION_V1 => self.decrypt_snapshot_v1(envelope),
            AUTH_STORE_ENVELOPE_VERSION_V2 => self.decrypt_snapshot_v2(envelope),
            version => bail!(
                "unsupported encrypted auth store version {} in {}",
                version,
                self.path.display()
            ),
        }
    }

    /// Decrypts a v2 envelope; its version and label are authenticated as
    /// associated data.
    fn decrypt_snapshot_v2(
        &self,
        envelope: &EncryptedAuthStoreEnvelope,
    ) -> Result<AuthStoreSnapshot> {
        let label = envelope.label.as_deref().ok_or_else(|| {
            anyhow!(
                "encrypted auth store {} is missing its envelope label",
                self.path.display()
            )
        })?;
        if label != AUTH_STORE_ENVELOPE_LABEL {
            bail!(
                "encrypted auth store {} has envelope label `{label}`, expected `{AUTH_STORE_ENVELOPE_LABEL}`",
                self.path.display()
            );
        }
        let key = self.master_key()?.ok_or_else(|| {
            anyhow!(
                "auth store {} is encrypted; set {} or {} before loading it",
                self.path.display(),
                AUTH_STORE_MASTER_KEY_ENV,
                AUTH_STORE_MASTER_KEY_FILE_ENV
            )
        })?;
        let nonce = BASE64_STANDARD
            .decode(&envelope.nonce)
            .with_context(|| format!("invalid auth store nonce in {}", self.path.display()))?;
        if nonce.len() != AUTH_STORE_NONCE_LEN {
            bail!("invalid auth store nonce length in {}", self.path.display());
        }
        let ciphertext = BASE64_STANDARD
            .decode(&envelope.ciphertext)
            .with_context(|| format!("invalid auth store ciphertext in {}", self.path.display()))?;
        let cipher = Aes256GcmSiv::new_from_slice(&key).expect("32-byte key should be valid");
        let aad = auth_store_envelope_aad(envelope.version, label);
        let plaintext = cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: ciphertext.as_ref(),
                    aad: &aad,
                },
            )
            .map_err(|error| {
                anyhow!(
                    "failed to decrypt auth store {}: {error}",
                    self.path.display()
                )
            })?;
        serde_json::from_slice(&plaintext).with_context(|| {
            format!(
                "failed to decode decrypted auth store {}",
                self.path.display()
            )
        })
    }

    /// Decrypts a v1 envelope (no associated data). Kept exactly as the v1
    /// reader was so stores written before v2 always open.
    fn decrypt_snapshot_v1(
        &self,
        envelope: &EncryptedAuthStoreEnvelope,
    ) -> Result<AuthStoreSnapshot> {
        if envelope.version != AUTH_STORE_ENVELOPE_VERSION_V1 {
            bail!(
                "unsupported encrypted auth store version {} in {}",
                envelope.version,
                self.path.display()
            );
        }
        let key = self.master_key()?.ok_or_else(|| {
            anyhow!(
                "auth store {} is encrypted; set {} or {} before loading it",
                self.path.display(),
                AUTH_STORE_MASTER_KEY_ENV,
                AUTH_STORE_MASTER_KEY_FILE_ENV
            )
        })?;
        let nonce = BASE64_STANDARD
            .decode(&envelope.nonce)
            .with_context(|| format!("invalid auth store nonce in {}", self.path.display()))?;
        if nonce.len() != AUTH_STORE_NONCE_LEN {
            bail!("invalid auth store nonce length in {}", self.path.display());
        }
        let ciphertext = BASE64_STANDARD
            .decode(&envelope.ciphertext)
            .with_context(|| format!("invalid auth store ciphertext in {}", self.path.display()))?;
        let cipher = Aes256GcmSiv::new_from_slice(&key).expect("32-byte key should be valid");
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|error| {
                anyhow!(
                    "failed to decrypt auth store {}: {error}",
                    self.path.display()
                )
            })?;
        serde_json::from_slice(&plaintext).with_context(|| {
            format!(
                "failed to decode decrypted auth store {}",
                self.path.display()
            )
        })
    }
}

fn atomic_write_private(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("auth store path {} has no file name", path.display()))?;
    let mut random = [0_u8; 8];
    let mut temp_path = None;
    for _ in 0..10 {
        rand::rngs::OsRng.fill_bytes(&mut random);
        let mut temp_name = OsString::from(file_name);
        temp_name.push(format!(
            ".{}.{}.tmp",
            std::process::id(),
            u64::from_le_bytes(random)
        ));
        let candidate = parent.join(temp_name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(mut file) => {
                let write_result = file
                    .write_all(content)
                    .with_context(|| format!("failed to write {}", candidate.display()))
                    .and_then(|_| {
                        file.sync_all()
                            .with_context(|| format!("failed to sync {}", candidate.display()))
                    });
                drop(file);
                if let Err(error) = write_result {
                    let _ = fs::remove_file(&candidate);
                    return Err(error);
                }
                temp_path = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to create {}", candidate.display()));
            }
        }
    }
    let temp_path = temp_path.ok_or_else(|| anyhow!("failed to allocate temp auth store path"))?;
    let result = fs::rename(&temp_path, path)
        .with_context(|| format!("failed to replace auth store {}", path.display()))
        .and_then(|_| sync_parent_dir(parent));
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn sync_parent_dir(parent: &Path) -> Result<()> {
    let dir = fs::File::open(parent)
        .with_context(|| format!("failed to open directory {}", parent.display()))?;
    dir.sync_all()
        .with_context(|| format!("failed to sync directory {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use tempfile::tempdir;

    use crate::{AuthMode, AuthProvider, AuthSlotId};

    fn clear_auth_store_master_key_env() {
        unsafe {
            std::env::remove_var(AUTH_STORE_MASTER_KEY_ENV);
            std::env::remove_var(AUTH_STORE_MASTER_KEY_FILE_ENV);
            std::env::remove_var(AUTH_STORE_REQUIRE_ENCRYPTED_ENV);
        }
    }

    const TEST_MASTER_KEY: &str = "0123456789abcdef0123456789abcdef";

    fn set_test_master_key() {
        unsafe {
            std::env::set_var(AUTH_STORE_MASTER_KEY_ENV, TEST_MASTER_KEY);
        }
    }

    fn sample_snapshot() -> AuthStoreSnapshot {
        AuthStoreSnapshot {
            slots: BTreeMap::from([(
                "fregate.callback".to_string(),
                AuthSlotRecord {
                    slot_id: AuthSlotId::new("fregate.callback"),
                    provider: AuthProvider::Anthropic,
                    mode: AuthMode::ApiKey,
                    state: serde_json::json!({"kind": "api_key", "api_key": "relay-secret-v2"}),
                    updated_at_ms: 7,
                },
            )]),
        }
    }

    /// Writes a store the way the v1 writer did: AES-256-GCM-SIV, no
    /// associated data, `{"version":1,"nonce":..,"ciphertext":..}`.
    fn write_v1_store(path: &Path, snapshot: &AuthStoreSnapshot) -> Result<()> {
        let key = parse_auth_store_master_key(TEST_MASTER_KEY)?;
        let cipher = Aes256GcmSiv::new_from_slice(&key).expect("32-byte key");
        let nonce_bytes = [7_u8; AUTH_STORE_NONCE_LEN];
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                serde_json::to_vec(snapshot)?.as_ref(),
            )
            .map_err(|error| anyhow!("{error}"))?;
        let envelope = serde_json::json!({
            "version": 1,
            "nonce": BASE64_STANDARD.encode(nonce_bytes),
            "ciphertext": BASE64_STANDARD.encode(ciphertext),
        });
        std::fs::write(path, serde_json::to_string_pretty(&envelope)?)?;
        Ok(())
    }

    #[test]
    fn saves_write_v2_envelopes_whose_version_and_label_are_authenticated() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        clear_auth_store_master_key_env();
        set_test_master_key();
        let temp = tempdir()?;
        let path = temp.path().join("auth-store.json");
        let store = FileAuthStore::new(&path);
        store.save(&sample_snapshot())?;
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(raw["version"], 2);
        assert_eq!(raw["label"], AUTH_STORE_ENVELOPE_LABEL);
        assert!(!raw.to_string().contains("relay-secret-v2"));
        assert_eq!(store.load()?, sample_snapshot());

        // Downgrading the version to the AAD-less v1 reader fails to decrypt.
        let mut downgraded = raw.clone();
        downgraded["version"] = serde_json::json!(1);
        std::fs::write(&path, downgraded.to_string())?;
        assert!(store.load().is_err(), "a v2 ciphertext must not open as v1");

        // A different label is refused, and a forged matching label still
        // cannot change the authenticated bytes.
        let mut relabeled = raw.clone();
        relabeled["label"] = serde_json::json!("kheish.other-store");
        std::fs::write(&path, relabeled.to_string())?;
        assert!(store.load().is_err());
        let mut unlabeled = raw.clone();
        unlabeled.as_object_mut().expect("object").remove("label");
        std::fs::write(&path, unlabeled.to_string())?;
        assert!(store.load().is_err());

        std::fs::write(&path, raw.to_string())?;
        assert_eq!(store.load()?, sample_snapshot());
        clear_auth_store_master_key_env();
        Ok(())
    }

    #[test]
    fn v1_stores_still_open_and_are_rewritten_as_v2_on_the_next_save() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        clear_auth_store_master_key_env();
        set_test_master_key();
        let temp = tempdir()?;
        let path = temp.path().join("auth-store.json");
        write_v1_store(&path, &sample_snapshot())?;
        let store = FileAuthStore::new(&path);
        assert_eq!(store.load()?, sample_snapshot());

        store.save(&store.load()?)?;
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        assert_eq!(raw["version"], 2);
        assert_eq!(store.load()?, sample_snapshot());
        clear_auth_store_master_key_env();
        Ok(())
    }

    #[test]
    fn plaintext_stores_are_encrypted_on_load_when_a_key_is_available() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        clear_auth_store_master_key_env();
        let temp = tempdir()?;
        let path = temp.path().join("auth-store.json");
        std::fs::write(&path, serde_json::to_string_pretty(&sample_snapshot())?)?;
        let store = FileAuthStore::new(&path);

        // Without a master key the plaintext store still loads, unchanged.
        assert_eq!(store.load()?, sample_snapshot());
        assert!(std::fs::read_to_string(&path)?.contains("relay-secret-v2"));

        // With one, loading rewrites it as a v2 envelope.
        set_test_master_key();
        assert_eq!(store.load()?, sample_snapshot());
        let raw = std::fs::read_to_string(&path)?;
        assert!(!raw.contains("relay-secret-v2"), "{raw}");
        let envelope: serde_json::Value = serde_json::from_str(&raw)?;
        assert_eq!(envelope["version"], 2);
        assert_eq!(store.load()?, sample_snapshot());
        clear_auth_store_master_key_env();
        Ok(())
    }

    #[test]
    fn require_encrypted_refuses_plaintext_stores() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        clear_auth_store_master_key_env();
        let temp = tempdir()?;
        let path = temp.path().join("auth-store.json");
        let plaintext = serde_json::to_string_pretty(&sample_snapshot())?;
        std::fs::write(&path, &plaintext)?;
        let store = FileAuthStore::new(&path);
        set_test_master_key();
        unsafe {
            std::env::set_var(AUTH_STORE_REQUIRE_ENCRYPTED_ENV, "true");
        }
        let error = store.load().expect_err("plaintext must be refused");
        assert!(
            error.to_string().contains(AUTH_STORE_REQUIRE_ENCRYPTED_ENV),
            "{error}"
        );
        // Nothing was rewritten.
        assert_eq!(std::fs::read_to_string(&path)?, plaintext);

        // Encrypted stores load normally under the same setting.
        store.save(&sample_snapshot())?;
        assert_eq!(store.load()?, sample_snapshot());

        unsafe {
            std::env::set_var(AUTH_STORE_REQUIRE_ENCRYPTED_ENV, "sometimes");
        }
        assert!(auth_store_requires_encryption_from_env().is_err());
        for (raw, expected) in [
            ("1", true),
            ("on", true),
            ("0", false),
            ("no", false),
            ("", false),
        ] {
            unsafe {
                std::env::set_var(AUTH_STORE_REQUIRE_ENCRYPTED_ENV, raw);
            }
            assert_eq!(
                auth_store_requires_encryption_from_env()?,
                expected,
                "{raw}"
            );
        }
        clear_auth_store_master_key_env();
        Ok(())
    }

    #[test]
    fn encrypted_store_round_trip_requires_the_master_key() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        let temp = tempdir()?;
        let path = temp.path().join("auth-store.json");
        let store = FileAuthStore::new(&path);
        let snapshot = AuthStoreSnapshot {
            slots: BTreeMap::from([(
                "openai-default".to_string(),
                AuthSlotRecord {
                    slot_id: AuthSlotId::new("openai-default"),
                    provider: AuthProvider::OpenAi,
                    mode: AuthMode::ApiKey,
                    state: serde_json::json!({
                        "kind": "api_key",
                        "api_key": "sk-test",
                        "organization": null,
                        "project": null,
                    }),
                    updated_at_ms: 1,
                },
            )]),
        };

        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
        }
        store.save(&snapshot)?;
        let raw = std::fs::read_to_string(&path)?;
        assert!(!raw.contains("sk-test"));

        let loaded = store.load()?;
        assert_eq!(loaded, snapshot);

        clear_auth_store_master_key_env();
        let error = store
            .load()
            .expect_err("encrypted load should require a master key");
        assert!(error.to_string().contains("is encrypted"));
        Ok(())
    }

    #[test]
    fn encrypted_store_rejects_invalid_master_key_shape() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        let temp = tempdir()?;
        let path = temp.path().join("auth-store.json");
        let store = FileAuthStore::new(&path);

        unsafe {
            std::env::set_var(AUTH_STORE_MASTER_KEY_ENV, "too-short");
        }
        let error = store
            .save(&AuthStoreSnapshot::default())
            .expect_err("invalid master key should be rejected");
        assert!(error.to_string().contains("must be exactly 32 raw bytes"));
        clear_auth_store_master_key_env();
        Ok(())
    }

    #[test]
    fn save_requires_a_master_key() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        let temp = tempdir()?;
        let path = temp.path().join("auth-store.json");
        let store = FileAuthStore::new(&path);
        clear_auth_store_master_key_env();
        let error = store
            .save(&AuthStoreSnapshot::default())
            .expect_err("writing without a master key should fail");
        assert!(error.to_string().contains(AUTH_STORE_MASTER_KEY_ENV));
        Ok(())
    }

    #[test]
    fn generated_master_key_decodes_to_32_bytes() -> Result<()> {
        let generated = generate_auth_store_master_key_base64();
        let parsed = parse_auth_store_master_key(&generated)?;
        assert_eq!(parsed.len(), 32);
        Ok(())
    }

    #[test]
    fn load_auth_store_master_key_from_env_rejects_invalid_values() {
        let _guard = auth_store_env_lock().lock();
        unsafe {
            std::env::set_var(AUTH_STORE_MASTER_KEY_ENV, "too-short");
        }
        let error =
            load_auth_store_master_key_from_env().expect_err("invalid key should be rejected");
        assert!(
            error
                .to_string()
                .contains("must be exactly 32 raw bytes or base64-encoded 32 bytes")
        );
        clear_auth_store_master_key_env();
    }

    #[test]
    fn generated_master_key_round_trips_through_file_store() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        let temp = tempdir()?;
        let store = FileAuthStore::new(temp.path().join("auth-store.json"));
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                generate_auth_store_master_key_base64(),
            );
        }
        store.save(&AuthStoreSnapshot::default())?;
        let loaded = store.load()?;
        assert!(loaded.slots.is_empty());
        clear_auth_store_master_key_env();
        Ok(())
    }

    #[test]
    fn load_auth_store_master_key_from_file_env() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        let temp = tempdir()?;
        let key_path = temp.path().join("auth-store.key");
        std::fs::write(&key_path, "0123456789abcdef0123456789abcdef\n")?;
        clear_auth_store_master_key_env();
        unsafe {
            std::env::set_var(AUTH_STORE_MASTER_KEY_FILE_ENV, &key_path);
        }
        let key =
            load_auth_store_master_key_from_env()?.expect("file-backed auth-store key should load");
        assert_eq!(key, *b"0123456789abcdef0123456789abcdef");
        clear_auth_store_master_key_env();
        Ok(())
    }

    #[test]
    fn load_auth_store_master_key_rejects_conflicting_env_and_file() -> Result<()> {
        let _guard = auth_store_env_lock().lock();
        let temp = tempdir()?;
        let key_path = temp.path().join("auth-store.key");
        std::fs::write(&key_path, generate_auth_store_master_key_base64())?;
        clear_auth_store_master_key_env();
        unsafe {
            std::env::set_var(
                AUTH_STORE_MASTER_KEY_ENV,
                "0123456789abcdef0123456789abcdef",
            );
            std::env::set_var(AUTH_STORE_MASTER_KEY_FILE_ENV, &key_path);
        }
        let error = load_auth_store_master_key_from_env()
            .expect_err("conflicting auth-store key sources should be rejected");
        assert!(error.to_string().contains("mutually exclusive"));
        clear_auth_store_master_key_env();
        Ok(())
    }
}
