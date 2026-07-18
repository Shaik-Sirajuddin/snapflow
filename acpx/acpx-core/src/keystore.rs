//! API key storage. Phase 3 step 13.
//!
//! **Encryption-at-rest** (`durable_secret_and_configuration_store`,
//! `acp-gateway-daemon`): [`MasterKeyring`] wraps every secret in real
//! AES-256-GCM (not a hand-rolled or base64 stand-in) before it ever
//! reaches `crate::persistence::PersistenceStore`. This is deliberately a
//! *local-file* key-management tier, not a real OS-keychain/KMS
//! integration -- honest about that distinction rather than overclaiming,
//! but structured (a versioned keyring + rotation, callers only ever see
//! opaque [`KeyRef`]s) so a real KMS-backed [`MasterKeyring::save`]/
//! `load` pair can replace this file-based one later without touching any
//! caller. Consistent with the task draft's "keys are maintained by this
//! intermediate proxy" requirement:
//! 1. A plain, non-durable [`Keystore::new`] stays in-memory only --
//!    unchanged default for any `Router` that never opts into
//!    `Router::enable_durable_config` (most of this crate's own tests, or
//!    a deployment that never sets `ACPX_DB_PATH`).
//! 2. Callers reference a key only via the opaque [`KeyRef`] handle
//!    (a profile stores a `KeyRef`, never raw key material) -- so this
//!    encryption-at-rest addition changes nothing at any call site beyond
//!    `crate::router::Router`'s own persistence wiring.
//! 3. [`Keystore`]'s `Debug` impl never prints key material (see below),
//!    so a stray `{:?}` on a `Router`/`Keystore` can't leak a secret into
//!    logs.
//! 4. Rotation ([`MasterKeyring::rotate`]) mints a new key version without
//!    invalidating ciphertext still encrypted under an older version --
//!    every row records its own `key_version`, so decrypt always works
//!    mid-rotation, and `Router::rotate_master_key` re-encrypts every
//!    live secret under the new version rather than leaving old rows on
//!    an old key forever.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use std::collections::HashMap;
use std::io;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct KeyRef(pub String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeystoreError {
    #[error("no key stored for ref {0:?}")]
    NotFound(KeyRef),
    #[error("secret decryption failed for key_version {0} (wrong key or corrupted ciphertext)")]
    DecryptFailed(u32),
    #[error("no keyring entry for key_version {0} -- cannot decrypt a row encrypted under it")]
    UnknownKeyVersion(u32),
}

/// In-memory secret store, keyed by [`KeyRef`]. See module docs for the
/// at-rest-encryption caveat.
#[derive(Default)]
pub struct Keystore {
    keys: HashMap<KeyRef, String>,
}

impl std::fmt::Debug for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keystore")
            .field("key_refs", &self.keys.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Keystore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store `secret` under a freshly-minted `KeyRef`, returning it so the
    /// caller (typically a `profiles/create` handler) can attach it to a
    /// `Profile`.
    pub fn store(&mut self, secret: impl Into<String>) -> KeyRef {
        let key_ref = KeyRef(uuid::Uuid::new_v4().to_string());
        self.keys.insert(key_ref.clone(), secret.into());
        key_ref
    }

    /// Insert under an already-known `KeyRef` -- used when reloading
    /// persisted (decrypted) secrets at startup, where the `KeyRef` is
    /// fixed by whatever a profile already references, not freshly
    /// minted. Overwrites silently on a repeat call (startup load is
    /// idempotent by construction: each `key_ref` is only ever loaded
    /// once per process lifetime).
    pub(crate) fn insert_known(&mut self, key_ref: KeyRef, secret: impl Into<String>) {
        self.keys.insert(key_ref, secret.into());
    }

    /// Every stored `(KeyRef, secret)` pair -- used by
    /// `Router::rotate_master_key` to re-encrypt every live secret under
    /// a freshly-minted keyring version. Crate-visible only: a raw
    /// secret must never cross this module's boundary except through
    /// `resolve` (backend spawn time) or this rotation seam.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&KeyRef, &str)> {
        self.keys.iter().map(|(k, v)| (k, v.as_str()))
    }

    /// Resolve a `KeyRef` back to its raw secret -- only called at backend
    /// spawn time (see `crate::launch`), never surfaced back to a client.
    pub fn resolve(&self, key_ref: &KeyRef) -> Result<&str, KeystoreError> {
        self.keys
            .get(key_ref)
            .map(String::as_str)
            .ok_or_else(|| KeystoreError::NotFound(key_ref.clone()))
    }

    pub fn delete(&mut self, key_ref: &KeyRef) -> Result<(), KeystoreError> {
        self.keys
            .remove(key_ref)
            .map(|_| ())
            .ok_or_else(|| KeystoreError::NotFound(key_ref.clone()))
    }
}

/// Versioned AES-256-GCM keyring. See module doc comment for the
/// local-file-tier caveat. `#[derive(Serialize, Deserialize)]` on the
/// on-disk shape only -- key bytes are hex-encoded (see [`hex_encode`]/
/// [`hex_decode`]) rather than pulled in via a new `base64`/`hex` crate
/// dependency for one small internal use.
#[derive(Default)]
pub struct MasterKeyring {
    keys: HashMap<u32, [u8; 32]>,
    current_version: u32,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct KeyringFile {
    current_version: u32,
    keys: HashMap<String, String>,
}

impl MasterKeyring {
    /// A fresh single-version keyring with a random key. Never touches
    /// disk -- pair with [`Self::save`] to persist it, or use
    /// [`Self::load_or_create`] to do both atomically.
    pub fn generate() -> Self {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        let mut keys = HashMap::new();
        keys.insert(1, key);
        Self {
            keys,
            current_version: 1,
        }
    }

    /// Read an existing keyring file at `path`, or generate + write a
    /// fresh one (with `0600` permissions on unix, the same "only this
    /// user can read it" bar a private key file gets) if `path` does not
    /// exist yet. This is the one function a caller needs for normal
    /// startup -- [`Self::generate`]/[`Self::save`]/[`Self::load`] exist
    /// separately mainly for testing.
    pub fn load_or_create(path: &std::path::Path) -> io::Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let keyring = Self::generate();
        keyring.save(path)?;
        Ok(keyring)
    }

    pub fn load(path: &std::path::Path) -> io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let file: KeyringFile = serde_json::from_str(&raw)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut keys = HashMap::with_capacity(file.keys.len());
        for (version, hex_key) in file.keys {
            let version: u32 = version
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid key version"))?;
            let bytes = hex_decode(&hex_key)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let key: [u8; 32] = bytes
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "key is not 32 bytes"))?;
            keys.insert(version, key);
        }
        Ok(Self {
            keys,
            current_version: file.current_version,
        })
    }

    pub fn save(&self, path: &std::path::Path) -> io::Result<()> {
        let file = KeyringFile {
            current_version: self.current_version,
            keys: self
                .keys
                .iter()
                .map(|(version, key)| (version.to_string(), hex_encode(key)))
                .collect(),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, json)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Mint a new key version and make it current; existing versions stay
    /// in the keyring (so already-persisted ciphertext under an old
    /// version keeps decrypting) until a caller explicitly re-encrypts
    /// and prunes them -- this method alone never deletes a key. Callers
    /// must [`Self::save`] afterward to persist the new version.
    pub fn rotate(&mut self) -> u32 {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        let new_version = self.current_version + 1;
        self.keys.insert(new_version, key);
        self.current_version = new_version;
        new_version
    }

    pub fn current_version(&self) -> u32 {
        self.current_version
    }

    /// Encrypt `plaintext` under the current key version. Returns
    /// `(key_version, nonce, ciphertext)` -- all three are needed to
    /// later decrypt via [`Self::decrypt`].
    pub fn encrypt(&self, plaintext: &[u8]) -> (u32, Vec<u8>, Vec<u8>) {
        let version = self.current_version;
        let key_bytes = self
            .keys
            .get(&version)
            .expect("current_version always has a key");
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key_bytes));
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher
            .encrypt(nonce, plaintext)
            .expect("AES-256-GCM encryption of a bounded in-memory secret never fails");
        (version, nonce_bytes.to_vec(), ciphertext)
    }

    /// Decrypt a row previously produced by [`Self::encrypt`]. Fails with
    /// [`KeystoreError::UnknownKeyVersion`] if this keyring never held (or
    /// lost, e.g. a keyring file replaced out of band) the version the row
    /// was encrypted under, or [`KeystoreError::DecryptFailed`] if the
    /// ciphertext/nonce/tag don't authenticate (wrong key or corruption).
    pub fn decrypt(
        &self,
        key_version: u32,
        nonce: &[u8],
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, KeystoreError> {
        let key_bytes = self
            .keys
            .get(&key_version)
            .ok_or(KeystoreError::UnknownKeyVersion(key_version))?;
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key_bytes));
        let nonce = Nonce::from_slice(nonce);
        cipher
            .decrypt(nonce, ciphertext)
            .map_err(|_| KeystoreError::DecryptFailed(key_version))
    }
}

impl std::fmt::Debug for MasterKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterKeyring")
            .field("versions", &{
                let mut versions: Vec<_> = self.keys.keys().copied().collect();
                versions.sort_unstable();
                versions
            })
            .field("current_version", &self.current_version)
            .finish_non_exhaustive()
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex string".to_string());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_resolve_round_trips() {
        let mut ks = Keystore::new();
        let key_ref = ks.store("sk-super-secret");
        assert_eq!(ks.resolve(&key_ref).unwrap(), "sk-super-secret");
    }

    #[test]
    fn resolve_unknown_ref_errors() {
        let ks = Keystore::new();
        let bogus = KeyRef("does-not-exist".to_string());
        assert_eq!(
            ks.resolve(&bogus),
            Err(KeystoreError::NotFound(bogus.clone()))
        );
    }

    #[test]
    fn delete_forgets_the_key() {
        let mut ks = Keystore::new();
        let key_ref = ks.store("sk-super-secret");
        ks.delete(&key_ref).unwrap();
        assert!(ks.resolve(&key_ref).is_err());
    }

    #[test]
    fn debug_impl_never_prints_secret_material() {
        let mut ks = Keystore::new();
        ks.store("sk-super-secret-marker");
        let rendered = format!("{ks:?}");
        assert!(!rendered.contains("sk-super-secret-marker"));
    }
}

#[cfg(test)]
mod keyring_tests {
    use super::*;

    #[test]
    fn encrypt_then_decrypt_round_trips() {
        let keyring = MasterKeyring::generate();
        let (version, nonce, ciphertext) = keyring.encrypt(b"sk-super-secret");
        assert_eq!(version, 1);
        assert_ne!(ciphertext, b"sk-super-secret");
        let plaintext = keyring.decrypt(version, &nonce, &ciphertext).unwrap();
        assert_eq!(plaintext, b"sk-super-secret");
    }

    #[test]
    fn rotate_keeps_old_versions_decryptable() {
        let mut keyring = MasterKeyring::generate();
        let (v1, nonce1, ciphertext1) = keyring.encrypt(b"secret-under-v1");
        let v2 = keyring.rotate();
        assert_eq!(v2, 2);
        assert_ne!(v1, v2);
        // Old ciphertext still decrypts after rotation -- rotation must
        // never orphan already-persisted rows.
        assert_eq!(
            keyring.decrypt(v1, &nonce1, &ciphertext1).unwrap(),
            b"secret-under-v1"
        );
        let (v2_check, nonce2, ciphertext2) = keyring.encrypt(b"secret-under-v2");
        assert_eq!(v2_check, v2);
        assert_eq!(
            keyring.decrypt(v2, &nonce2, &ciphertext2).unwrap(),
            b"secret-under-v2"
        );
    }

    #[test]
    fn decrypt_with_unknown_version_errors() {
        let keyring = MasterKeyring::generate();
        let result = keyring.decrypt(99, &[0u8; 12], b"whatever");
        assert_eq!(result, Err(KeystoreError::UnknownKeyVersion(99)));
    }

    #[test]
    fn load_or_create_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("master.keyring");
        let created = MasterKeyring::load_or_create(&path).expect("create");
        let (version, nonce, ciphertext) = created.encrypt(b"sk-disk-round-trip");

        let reloaded = MasterKeyring::load_or_create(&path).expect("reload existing file");
        assert_eq!(reloaded.current_version(), created.current_version());
        assert_eq!(
            reloaded.decrypt(version, &nonce, &ciphertext).unwrap(),
            b"sk-disk-round-trip"
        );
    }

    #[cfg(unix)]
    #[test]
    fn keyring_file_is_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("master.keyring");
        MasterKeyring::load_or_create(&path).expect("create");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 255, 16, 128];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes.to_vec());
    }
}
