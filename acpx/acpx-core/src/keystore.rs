//! API key storage. Phase 3 step 13.
//!
//! **Encryption-at-rest is an explicit, unresolved open risk** (see
//! `05-open-risks.md`'s "Key storage mechanism is unspecified" item) --
//! this store deliberately does *not* attempt real at-rest encryption yet;
//! picking a wrong/half-baked scheme (e.g. a hardcoded key, or "encryption"
//! that's really just base64) would be worse than being honest about the
//! gap. What it *does* guarantee today, consistent with the task draft's
//! "keys are maintained by this intermediate proxy" requirement:
//! 1. Keys live only in-memory (`Keystore` is never wired into the sqlite
//!    `persistence` module, unlike sessions/transcripts) -- process
//!    restart forgets every key, by design, until a real persisted+
//!    encrypted store lands.
//! 2. Callers reference a key only via the opaque [`KeyRef`] handle
//!    (a profile stores a `KeyRef`, never raw key material) -- so adding
//!    real encryption later only changes this module internally, not any
//!    caller.
//! 3. [`Keystore`]'s `Debug` impl never prints key material (see below),
//!    so a stray `{:?}` on a `Router`/`Keystore` can't leak a secret into
//!    logs.

use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct KeyRef(pub String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeystoreError {
    #[error("no key stored for ref {0:?}")]
    NotFound(KeyRef),
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
