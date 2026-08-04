//! Typed helpers for gateway-owned session permission profiles.

use crate::raw::{ClientError, GatewayClient};
pub use acpx_proto::gateway::{
    PermissionProfile, PermissionProfilesListResult, SessionPermissionProfileParams,
    SessionPermissionProfileResult,
};

pub use acpx_proto::gateway::{
    PermissionPolicySchema, PermissionProfileOverrides, PermissionProfileType,
};

pub async fn list(client: &GatewayClient) -> Result<Vec<PermissionProfile>, ClientError> {
    let result = client
        .call("permission_profiles/list", serde_json::json!({}), None)
        .await?;
    let envelope: PermissionProfilesListResult = serde_json::from_value(result).map_err(|e| {
        ClientError::InvalidParams(format!("invalid permission profile response: {e}"))
    })?;
    Ok(envelope.profiles)
}

pub async fn get(
    client: &GatewayClient,
    session_id: impl Into<String>,
) -> Result<SessionPermissionProfileResult, ClientError> {
    let result = client
        .call(
            "session/permission_profile/get",
            serde_json::json!({ "sessionId": session_id.into() }),
            None,
        )
        .await?;
    serde_json::from_value(result).map_err(|e| {
        ClientError::InvalidParams(format!("invalid session permission profile response: {e}"))
    })
}

pub async fn set(
    client: &GatewayClient,
    params: SessionPermissionProfileParams,
) -> Result<SessionPermissionProfileResult, ClientError> {
    let result = client
        .call(
            "session/permission_profile/set",
            serde_json::to_value(params).map_err(|e| ClientError::InvalidParams(e.to_string()))?,
            None,
        )
        .await?;
    serde_json::from_value(result).map_err(|e| {
        ClientError::InvalidParams(format!("invalid session permission profile response: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_use_gateway_wire_names() {
        let params = SessionPermissionProfileParams {
            session_id: "s1".into(),
            profile_type: Some(PermissionProfileType::Readonly),
            overrides: None,
        };
        assert_eq!(serde_json::to_value(params).unwrap()["sessionId"], "s1");
    }
}
