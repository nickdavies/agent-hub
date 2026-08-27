//! Strict sandbox-to-host protocol for attended tool approvals.
//!
//! The sandbox receives only the path of a per-run Unix socket.  Endpoint and
//! credential material are deliberately absent from these wire types.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ApprovalRequest;

pub const ATTENDED_GATEWAY_REQUEST_VERSION: &str = "attended_gateway_request_v1";
pub const ATTENDED_GATEWAY_DECISION_VERSION: &str = "attended_gateway_decision_v1";
pub const ATTENDED_GATEWAY_MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const ATTENDED_GATEWAY_SOCKET_PATH: &str = "/agent-hub/approval/approval.sock";
const TOOL_INPUT_DOMAIN: &[u8] = b"agent-hub:attended-tool-input:v1\0";
const NETWORK_DOMAIN: &[u8] = b"agent-hub:attended-network:v1\0";
const APPROVAL_BUNDLE_DOMAIN: &[u8] = b"agent-hub:attended-approval-bundle:v2\0";

/// Immutable values copied from the authorization and the foreground run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedGatewayBindingV1 {
    pub run_epoch: String,
    pub authorization_digest: String,
    pub environment_bundle_digest: String,
    pub image_digest: String,
    pub network_digest: String,
    pub approval_bundle_digest: String,
}

/// One canonical, length-prefixed JSON request per connection.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AttendedGatewayRequestV1 {
    pub version: String,
    #[serde(flatten)]
    pub binding: AttendedGatewayBindingV1,
    pub request_id: String,
    pub approval: ApprovalRequest,
    pub tool_input_digest: String,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttendedGatewayDecisionKind {
    Allow,
    Deny,
}

/// One canonical, length-prefixed JSON response. All request bindings are echoed so a
/// sandbox client can reject a response from another request or run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AttendedGatewayDecisionV1 {
    pub version: String,
    #[serde(flatten)]
    pub binding: AttendedGatewayBindingV1,
    pub request_id: String,
    pub tool_input_digest: String,
    pub deadline: DateTime<Utc>,
    pub decision: AttendedGatewayDecisionKind,
    pub reason: Option<String>,
}

impl AttendedGatewayRequestV1 {
    pub fn validate(
        &self,
        expected: &AttendedGatewayBindingV1,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        if self.version != ATTENDED_GATEWAY_REQUEST_VERSION {
            return Err("unsupported attended gateway request version".into());
        }
        if &self.binding != expected {
            return Err("attended gateway run binding mismatch".into());
        }
        uuid::Uuid::parse_str(&self.request_id)
            .map_err(|_| "attended gateway request ID is not a UUID".to_string())?;
        if self.approval.id != self.request_id {
            return Err("attended gateway approval ID does not match request ID".into());
        }
        if self.deadline <= now {
            return Err("attended gateway request deadline has expired".into());
        }
        validate_binding(expected)?;
        let digest = tool_input_digest(&self.approval.tool_input)?;
        if digest != self.tool_input_digest {
            return Err("attended gateway tool input digest mismatch".into());
        }
        Ok(())
    }

    pub fn decision(
        &self,
        decision: AttendedGatewayDecisionKind,
        reason: Option<String>,
    ) -> AttendedGatewayDecisionV1 {
        AttendedGatewayDecisionV1 {
            version: ATTENDED_GATEWAY_DECISION_VERSION.into(),
            binding: self.binding.clone(),
            request_id: self.request_id.clone(),
            tool_input_digest: self.tool_input_digest.clone(),
            deadline: self.deadline,
            decision,
            reason,
        }
    }
}

pub fn tool_input_digest(input: &serde_json::Value) -> Result<String, String> {
    let bytes = canonical_json_bytes(input.clone())?;
    Ok(crate::sha256_hex_with_domain(TOOL_INPUT_DOMAIN, &bytes))
}

pub fn attended_network_digest(value: &serde_json::Value) -> Result<String, String> {
    Ok(crate::sha256_hex_with_domain(
        NETWORK_DOMAIN,
        &canonical_json_bytes(value.clone())?,
    ))
}

pub fn attended_approval_bundle_digest(value: &serde_json::Value) -> Result<String, String> {
    Ok(crate::sha256_hex_with_domain(
        APPROVAL_BUNDLE_DOMAIN,
        &canonical_json_bytes(value.clone())?,
    ))
}

pub fn encode_attended_gateway_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let value = serde_json::to_value(value).map_err(|error| error.to_string())?;
    let payload = canonical_json_bytes(value)?;
    if payload.len() > ATTENDED_GATEWAY_MAX_FRAME_BYTES {
        return Err("attended gateway frame exceeds limit".into());
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| "attended gateway frame length overflow".to_string())?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_attended_gateway_payload<T>(payload: &[u8]) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    if payload.len() > ATTENDED_GATEWAY_MAX_FRAME_BYTES {
        return Err("attended gateway frame exceeds limit".into());
    }
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|error| error.to_string())?;
    if canonical_json_bytes(value.clone())? != payload {
        return Err("attended gateway JSON is not canonical".into());
    }
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn validate_binding(binding: &AttendedGatewayBindingV1) -> Result<(), String> {
    uuid::Uuid::parse_str(&binding.run_epoch)
        .map_err(|_| "attended gateway run epoch is not a UUID".to_string())?;
    for (name, digest) in [
        ("authorization", &binding.authorization_digest),
        ("environment bundle", &binding.environment_bundle_digest),
        ("network", &binding.network_digest),
        ("approval bundle", &binding.approval_bundle_digest),
    ] {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(format!(
                "attended gateway {name} digest is not lowercase SHA-256"
            ));
        }
    }
    if !binding.image_digest.starts_with("sha256:")
        || binding.image_digest.len() != 71
        || !binding.image_digest[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err("attended gateway image digest is not a canonical image SHA-256".into());
    }
    Ok(())
}

fn canonical_json_bytes(value: serde_json::Value) -> Result<Vec<u8>, String> {
    fn normalize(value: serde_json::Value) -> Result<serde_json::Value, String> {
        Ok(match value {
            serde_json::Value::Object(object) => {
                let sorted = object
                    .into_iter()
                    .collect::<std::collections::BTreeMap<_, _>>();
                serde_json::Value::Object(
                    sorted
                        .into_iter()
                        .map(|(key, value)| normalize(value).map(|value| (key, value)))
                        .collect::<Result<_, _>>()?,
                )
            }
            serde_json::Value::Array(values) => serde_json::Value::Array(
                values
                    .into_iter()
                    .map(normalize)
                    .collect::<Result<_, _>>()?,
            ),
            serde_json::Value::Number(number) if !number.is_i64() && !number.is_u64() => {
                return Err(
                    "attended gateway canonical JSON does not permit floating numbers".into(),
                );
            }
            other => other,
        })
    }
    serde_json::to_vec(&normalize(value)?).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> AttendedGatewayBindingV1 {
        AttendedGatewayBindingV1 {
            run_epoch: uuid::Uuid::new_v4().to_string(),
            authorization_digest: "a".repeat(64),
            environment_bundle_digest: "b".repeat(64),
            image_digest: format!("sha256:{}", "c".repeat(64)),
            network_digest: "d".repeat(64),
            approval_bundle_digest: "e".repeat(64),
        }
    }

    #[test]
    fn request_binds_exact_input_and_every_run_digest() {
        let binding = binding();
        let input = serde_json::json!({"z": 1, "a": [true]});
        let request = AttendedGatewayRequestV1 {
            version: ATTENDED_GATEWAY_REQUEST_VERSION.into(),
            binding: binding.clone(),
            request_id: uuid::Uuid::new_v4().to_string(),
            approval: ApprovalRequest {
                id: String::new(),
                session_id: crate::SessionId::new("session"),
                session_display_name: "Attended".into(),
                cwd: "/workspace/main".into(),
                tool: crate::Tool::Bash,
                tool_input: input.clone(),
                provider: "opencode".into(),
                request_type: crate::RequestType::ToolUse,
                context: crate::ApprovalContext {
                    workspace_roots: vec!["/workspace/main".into()],
                    hook_event_name: crate::HookEventName::from("permission.ask".to_string()),
                    extra: None,
                },
            },
            tool_input_digest: tool_input_digest(&input).unwrap(),
            deadline: Utc::now() + chrono::Duration::seconds(30),
        };
        let mut request = request;
        request.approval.id = request.request_id.clone();
        request.validate(&binding, Utc::now()).unwrap();
        let mut changed = request.clone();
        changed.approval.tool_input["z"] = 2.into();
        assert!(
            changed
                .validate(&binding, Utc::now())
                .unwrap_err()
                .contains("tool input")
        );
        let mut wrong = binding;
        wrong.network_digest = "f".repeat(64);
        assert!(
            request
                .validate(&wrong, Utc::now())
                .unwrap_err()
                .contains("binding")
        );
    }

    #[test]
    fn strict_wire_rejects_unknown_fields_and_expired_deadlines() {
        let binding = binding();
        let input = serde_json::json!({});
        let request = AttendedGatewayRequestV1 {
            version: ATTENDED_GATEWAY_REQUEST_VERSION.into(),
            binding: binding.clone(),
            request_id: uuid::Uuid::new_v4().to_string(),
            approval: ApprovalRequest {
                id: String::new(),
                session_id: crate::SessionId::new("session"),
                session_display_name: "Attended".into(),
                cwd: "/workspace/main".into(),
                tool: crate::Tool::Read,
                tool_input: input.clone(),
                provider: "opencode".into(),
                request_type: crate::RequestType::ToolUse,
                context: crate::ApprovalContext {
                    workspace_roots: vec!["/workspace/main".into()],
                    hook_event_name: crate::HookEventName::from("permission.ask".to_string()),
                    extra: None,
                },
            },
            tool_input_digest: tool_input_digest(&input).unwrap(),
            deadline: Utc::now() - chrono::Duration::seconds(1),
        };
        let mut request = request;
        request.approval.id = request.request_id.clone();
        assert!(
            request
                .validate(&binding, Utc::now())
                .unwrap_err()
                .contains("expired")
        );
        let mut value = serde_json::to_value(request).unwrap();
        value["server_url"] = "https://must-not-enter-sandbox".into();
        assert!(serde_json::from_value::<AttendedGatewayRequestV1>(value).is_err());
    }

    #[test]
    fn frames_are_big_endian_bounded_canonical_json() {
        let value = serde_json::json!({"z": 1, "a": 2});
        let frame = encode_attended_gateway_frame(&value).unwrap();
        assert_eq!(
            u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize,
            frame.len() - 4
        );
        assert_eq!(&frame[4..], br#"{"a":2,"z":1}"#);
        let decoded: serde_json::Value = decode_attended_gateway_payload(&frame[4..]).unwrap();
        assert_eq!(decoded, value);
        assert!(decode_attended_gateway_payload::<serde_json::Value>(br#"{"z":1,"a":2}"#).is_err());
    }
}
