//! Cross-store integration coverage for Phase 3 steps 12-14 --
//! `ProviderStore`, `Keystore`, and `ProfileStore` working together the
//! way `router.rs`'s `resolve_profile` actually uses them, exercised via
//! `acpx_core`'s public API from outside the crate. Each store's own
//! single-operation CRUD cases already live in its own
//! `#[cfg(test)] mod tests` (`src/provider.rs`/`src/keystore.rs`/
//! `src/profile.rs`); this file is deliberately about multi-entity
//! combinations instead. End-to-end coverage of the *router* actually
//! resolving a profile against these stores (provider env injection, MCP
//! server merge) lives in `profile_resolution_test.rs`, not here.

use acpx_core::keystore::{Keystore, KeystoreError};
use acpx_core::profile::{Profile, ProfileStore, ProfileStoreError};
use acpx_core::provider::{ProviderConfig, ProviderKind, ProviderStore};
use std::collections::HashMap;

fn litellm_profile(name: &str, key_ref: acpx_core::keystore::KeyRef) -> Profile {
    let mut launch_overrides = HashMap::new();
    launch_overrides.insert("CODEX_TIMEOUT_MS".to_string(), "60000".to_string());
    Profile {
        name: name.to_string(),
        agent_id: "codex-acp".to_string(),
        provider: Some("litellm-proxy".to_string()),
        key_ref: Some(key_ref),
        launch_overrides,
        mcp_servers: vec!["fs".to_string(), "git".to_string()],
        permission_policy: Default::default(),
        allow_fs_access: false,
        allow_terminal_access: false,
    }
}

#[test]
fn provider_keystore_and_profile_stores_compose_end_to_end() {
    let mut providers = ProviderStore::new();
    providers
        .create(ProviderConfig {
            name: "litellm-proxy".to_string(),
            kind: ProviderKind::LiteLlm,
            base_url: Some("https://litellm.example.com/v1".to_string()),
        })
        .expect("create provider");

    let mut keystore = Keystore::new();
    let key_ref = keystore.store("sk-integration-test");

    let mut profiles = ProfileStore::new();
    profiles
        .create(litellm_profile("work", key_ref.clone()))
        .expect("create profile");

    // Resolve exactly like `router.rs`'s `resolve_profile` does: profile
    // -> provider name -> ProviderStore, profile -> key_ref -> Keystore.
    let profile = profiles.get("work").expect("profile exists");
    let provider = providers
        .get(profile.provider.as_deref().expect("provider set"))
        .expect("provider exists");
    assert_eq!(provider.kind, ProviderKind::LiteLlm);
    let secret = keystore
        .resolve(profile.key_ref.as_ref().expect("key_ref set"))
        .expect("key resolves");
    assert_eq!(secret, "sk-integration-test");

    // launch_overrides and mcp_servers both round-trip through the store,
    // not just the provider/key half.
    assert_eq!(
        profile.launch_overrides.get("CODEX_TIMEOUT_MS").unwrap(),
        "60000"
    );
    assert_eq!(
        profile.mcp_servers,
        vec!["fs".to_string(), "git".to_string()]
    );
}

#[test]
fn deleting_a_key_makes_a_profile_that_references_it_fail_to_resolve() {
    // Keystore and ProfileStore are independent -- deleting a key doesn't
    // cascade into the profile that references it (no foreign-key-style
    // enforcement between these two in-memory stores, unlike sqlite's
    // sessions/transcripts). A profile referencing a deleted key should
    // fail to *resolve* (what `router.rs` does at `session/new`), not
    // silently succeed with an empty/wrong secret.
    let mut keystore = Keystore::new();
    let key_ref = keystore.store("sk-will-be-deleted");
    let mut profiles = ProfileStore::new();
    profiles
        .create(litellm_profile("work", key_ref.clone()))
        .expect("create profile");

    keystore.delete(&key_ref).expect("delete key");

    let profile = profiles.get("work").expect("profile still exists");
    let result = keystore.resolve(profile.key_ref.as_ref().unwrap());
    assert_eq!(result, Err(KeystoreError::NotFound(key_ref)));
}

#[test]
fn multiple_profiles_can_share_one_provider_with_different_keys() {
    let mut providers = ProviderStore::new();
    providers
        .create(ProviderConfig {
            name: "shared-anthropic".to_string(),
            kind: ProviderKind::Anthropic,
            base_url: None,
        })
        .unwrap();

    let mut keystore = Keystore::new();
    let alice_key = keystore.store("sk-alice");
    let bob_key = keystore.store("sk-bob");

    let mut profiles = ProfileStore::new();
    profiles
        .create(Profile {
            name: "alice".to_string(),
            agent_id: "claude-agent-acp".to_string(),
            provider: Some("shared-anthropic".to_string()),
            key_ref: Some(alice_key),
            launch_overrides: HashMap::new(),
            mcp_servers: vec![],
            permission_policy: Default::default(),
            allow_fs_access: false,
            allow_terminal_access: false,
        })
        .unwrap();
    profiles
        .create(Profile {
            name: "bob".to_string(),
            agent_id: "claude-agent-acp".to_string(),
            provider: Some("shared-anthropic".to_string()),
            key_ref: Some(bob_key),
            launch_overrides: HashMap::new(),
            mcp_servers: vec![],
            permission_policy: Default::default(),
            allow_fs_access: false,
            allow_terminal_access: false,
        })
        .unwrap();

    assert_eq!(profiles.list().count(), 2);
    let alice_secret = keystore
        .resolve(profiles.get("alice").unwrap().key_ref.as_ref().unwrap())
        .unwrap();
    let bob_secret = keystore
        .resolve(profiles.get("bob").unwrap().key_ref.as_ref().unwrap())
        .unwrap();
    assert_ne!(alice_secret, bob_secret);
    assert_eq!(alice_secret, "sk-alice");
    assert_eq!(bob_secret, "sk-bob");
}

#[test]
fn create_profile_referencing_provider_that_does_not_exist_yet_still_succeeds() {
    // ProfileStore doesn't validate `provider` against ProviderStore at
    // create time -- that's `router.rs`'s job at resolution time (see
    // `RouterError::UnknownProviderRef` and
    // `profile_resolution_test.rs`'s coverage of that error path). A
    // profile can legitimately be created before its provider is
    // registered (e.g. config applied out of order), it just won't
    // resolve until the provider shows up.
    let mut profiles = ProfileStore::new();
    let profile = Profile {
        name: "future-provider".to_string(),
        agent_id: "codex-acp".to_string(),
        provider: Some("not-registered-yet".to_string()),
        key_ref: None,
        launch_overrides: HashMap::new(),
        mcp_servers: vec![],
        permission_policy: Default::default(),
        allow_fs_access: false,
        allow_terminal_access: false,
    };
    assert!(profiles.create(profile).is_ok());

    let providers = ProviderStore::new();
    assert!(providers
        .get(
            profiles
                .get("future-provider")
                .unwrap()
                .provider
                .as_deref()
                .unwrap()
        )
        .is_none());
}

#[test]
fn profile_store_error_variants_are_distinguishable() {
    let mut profiles = ProfileStore::new();
    let profile = Profile {
        name: "x".to_string(),
        agent_id: "codex-acp".to_string(),
        provider: None,
        key_ref: None,
        launch_overrides: HashMap::new(),
        mcp_servers: vec![],
        permission_policy: Default::default(),
        allow_fs_access: false,
        allow_terminal_access: false,
    };
    profiles.create(profile.clone()).unwrap();
    assert_eq!(
        profiles.create(profile.clone()),
        Err(ProfileStoreError::AlreadyExists("x".to_string()))
    );
    profiles.delete("x").unwrap();
    assert_eq!(
        profiles.update(profile),
        Err(ProfileStoreError::NotFound("x".to_string()))
    );
}
