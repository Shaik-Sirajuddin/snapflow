//! Integration tests for the `npx`/`uvx` install path (04-phased-plan.md
//! step 19): these exercise the real `node`/`npm`/`uv` binaries already on
//! `PATH` in this environment, no network access required -- npx/uvx's own
//! on-demand package resolution never runs here, only `<runtime> --version`.

use acpx_registry::index::{Distribution, NpxDist};
use acpx_registry::{install, Agent, InstallError, InstallOutcome};

fn agent_with(id: &str, distribution: Distribution) -> Agent {
    Agent {
        id: id.to_string(),
        name: id.to_string(),
        version: "0.0.0".to_string(),
        description: None,
        repository: None,
        website: None,
        authors: vec![],
        license: None,
        icon: None,
        distribution,
    }
}

#[tokio::test]
async fn npx_runtime_check_succeeds_against_real_node_npm() {
    let agent = agent_with(
        "test-npx-agent",
        Distribution {
            npx: Some(NpxDist {
                package: "@agentclientprotocol/does-not-matter@0.0.0".to_string(),
                args: vec![],
            }),
            uvx: None,
            binary: None,
        },
    );

    let outcome = install(&agent)
        .await
        .expect("node/npm should be on PATH in this environment");
    assert_eq!(
        outcome,
        InstallOutcome::RuntimeConfirmed {
            runtime: "node+npm"
        }
    );
}

#[tokio::test]
async fn uvx_runtime_check_succeeds_against_real_uv() {
    let agent = agent_with(
        "test-uvx-agent",
        Distribution {
            npx: None,
            uvx: Some(NpxDist {
                package: "does-not-matter".to_string(),
                args: vec![],
            }),
            binary: None,
        },
    );

    let outcome = install(&agent)
        .await
        .expect("uv should be on PATH in this environment");
    assert_eq!(outcome, InstallOutcome::RuntimeConfirmed { runtime: "uv" });
}

#[tokio::test]
async fn agent_with_no_distribution_method_errors_without_touching_runtimes() {
    let agent = agent_with("test-empty-agent", Distribution::default());
    let err = install(&agent).await.unwrap_err();
    assert!(matches!(err, InstallError::NoDistribution(id) if id == "test-empty-agent"));
}
