use std::future::Future;
use std::path::Path;
use std::time::Duration;

use protocol::{
    ATTENDED_GATEWAY_DECISION_VERSION, ATTENDED_GATEWAY_MAX_FRAME_BYTES, AttendedGatewayDecisionV1,
    AttendedGatewayRequestV1, decode_attended_gateway_payload, encode_attended_gateway_frame,
};

#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;

pub(crate) async fn request_approval<F>(
    socket: &Path,
    request: &AttendedGatewayRequestV1,
    timeout: Duration,
    cancellation: F,
) -> Result<AttendedGatewayDecisionV1, String>
where
    F: Future<Output = ()>,
{
    #[cfg(unix)]
    {
        round_trip(socket, request, timeout, cancellation).await
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, request, timeout, cancellation);
        Err("attended approval sockets are unsupported on this platform".to_string())
    }
}

#[cfg(unix)]
async fn round_trip<F>(
    socket: &Path,
    request: &AttendedGatewayRequestV1,
    timeout: Duration,
    cancellation: F,
) -> Result<AttendedGatewayDecisionV1, String>
where
    F: Future<Output = ()>,
{
    request.validate(&request.binding, chrono::Utc::now())?;
    let socket_identity = validate_socket(socket)?;
    let request_frame = encode_attended_gateway_frame(request)?;

    let exchange = exchange(socket, socket_identity, &request_frame);
    tokio::pin!(exchange);
    tokio::pin!(cancellation);
    let response = tokio::select! {
        result = tokio::time::timeout(timeout, &mut exchange) => match result {
            Ok(result) => result?,
            Err(_) => return Err("attended approval socket timeout".to_string()),
        },
        _ = &mut cancellation => return Err("attended approval socket cancellation".to_string()),
    };

    let decision: AttendedGatewayDecisionV1 =
        decode_attended_gateway_payload(&response).map_err(|error| {
            format!("attended approval socket returned malformed decision: {error}")
        })?;
    validate_decision(request, &decision)?;
    Ok(decision)
}

fn validate_decision(
    request: &AttendedGatewayRequestV1,
    decision: &AttendedGatewayDecisionV1,
) -> Result<(), String> {
    if decision.version != ATTENDED_GATEWAY_DECISION_VERSION {
        return Err("attended approval decision version mismatch".to_string());
    }
    if decision.binding != request.binding
        || decision.request_id != request.request_id
        || decision.tool_input_digest != request.tool_input_digest
        || decision.deadline != request.deadline
    {
        return Err("attended approval decision binding mismatch or replay".to_string());
    }
    if chrono::Utc::now() >= decision.deadline {
        return Err("attended approval decision arrived after its deadline".to_string());
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
    uid: u32,
}

#[cfg(unix)]
async fn exchange(
    socket: &Path,
    expected_identity: SocketIdentity,
    request_frame: &[u8],
) -> Result<Vec<u8>, String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|error| format!("attended approval socket connect failed: {error}"))?;
    verify_socket_identity(socket, expected_identity)?;
    let peer = stream
        .peer_cred()
        .map_err(|error| format!("cannot authenticate attended approval relay peer: {error}"))?;
    let runtime_uid = unsafe { libc::geteuid() };
    if peer.uid() != runtime_uid {
        return Err("attended approval relay peer UID does not match the runtime UID".to_string());
    }

    stream.write_all(request_frame).await.map_err(|error| {
        format!("attended approval socket disconnected writing request: {error}")
    })?;

    let len = stream.read_u32().await.map_err(|error| {
        format!("attended approval socket disconnected before decision: {error}")
    })? as usize;
    if len == 0 {
        return Err("attended approval socket returned an empty frame".to_string());
    }
    if len > ATTENDED_GATEWAY_MAX_FRAME_BYTES {
        return Err(format!(
            "attended approval decision frame is too large: {len} bytes"
        ));
    }
    let mut response = vec![0; len];
    stream.read_exact(&mut response).await.map_err(|error| {
        format!("attended approval socket disconnected during decision: {error}")
    })?;
    let mut trailing = [0_u8; 1];
    match stream.read(&mut trailing).await {
        Ok(0) => Ok(response),
        Ok(_) => Err("attended approval socket returned trailing data or a replay".to_string()),
        Err(error) => Err(format!(
            "attended approval socket disconnected abnormally after decision: {error}"
        )),
    }
}

#[cfg(unix)]
fn validate_socket(socket: &Path) -> Result<SocketIdentity, String> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    if !socket.is_absolute() {
        return Err("attended approval socket path must be absolute".to_string());
    }
    let parent = socket
        .parent()
        .ok_or_else(|| "attended approval socket has no parent directory".to_string())?;
    let parent_metadata = std::fs::symlink_metadata(parent)
        .map_err(|error| format!("attended approval socket parent is inaccessible: {error}"))?;
    if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
        return Err("attended approval socket parent is not a real directory".to_string());
    }
    if std::fs::canonicalize(parent)
        .map_err(|error| format!("cannot canonicalize attended approval socket parent: {error}"))?
        != parent
    {
        return Err("attended approval socket parent path is not canonical".to_string());
    }
    let runtime_uid = unsafe { libc::geteuid() };
    if parent_metadata.uid() != runtime_uid {
        return Err(
            "attended approval socket parent owner does not match the runtime UID".to_string(),
        );
    }
    if parent_metadata.permissions().mode() & 0o777 != 0o700 {
        return Err("attended approval socket parent mode is not 0700".to_string());
    }
    verify_read_only_mount(parent)?;
    socket_identity(socket)
}

#[cfg(all(unix, not(test)))]
fn verify_read_only_mount(parent: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let parent = CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| "attended approval socket parent contains NUL".to_string())?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(parent.as_ptr(), filesystem.as_mut_ptr()) } != 0 {
        return Err(format!(
            "cannot inspect attended approval socket parent filesystem: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { filesystem.assume_init() }.f_flag & libc::ST_RDONLY == 0 {
        return Err("attended approval socket parent is not a read-only bind mount".to_string());
    }
    Ok(())
}

#[cfg(all(unix, test))]
fn verify_read_only_mount(_parent: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn socket_identity(socket: &Path) -> Result<SocketIdentity, String> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};

    let metadata = std::fs::symlink_metadata(socket)
        .map_err(|error| format!("attended approval socket is missing or inaccessible: {error}"))?;
    if !metadata.file_type().is_socket() {
        return Err("attended approval socket path is not a Unix socket".to_string());
    }
    let runtime_uid = unsafe { libc::geteuid() };
    if metadata.uid() != runtime_uid {
        return Err("attended approval socket owner does not match the runtime UID".to_string());
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err("attended approval socket mode is not 0600".to_string());
    }
    Ok(SocketIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        uid: metadata.uid(),
    })
}

#[cfg(unix)]
fn verify_socket_identity(socket: &Path, expected: SocketIdentity) -> Result<(), String> {
    if socket_identity(socket)? != expected {
        return Err("attended approval socket identity changed while connecting".to_string());
    }
    Ok(())
}

#[cfg(unix)]
#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use chrono::{TimeZone as _, Utc};
    use protocol::{
        ATTENDED_GATEWAY_REQUEST_VERSION, ApprovalContext, ApprovalRequest,
        AttendedGatewayBindingV1, AttendedGatewayDecisionKind, AttendedGatewayRequestV1,
        HookEventName, RequestType, SessionId, Tool, encode_attended_gateway_frame,
        tool_input_digest,
    };
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    use super::{
        ATTENDED_GATEWAY_MAX_FRAME_BYTES, round_trip, socket_identity, verify_socket_identity,
    };

    fn request() -> AttendedGatewayRequestV1 {
        let tool_input = json!({"command": "git status"});
        AttendedGatewayRequestV1 {
            version: ATTENDED_GATEWAY_REQUEST_VERSION.into(),
            binding: AttendedGatewayBindingV1 {
                run_epoch: "018f0f21-76c0-7b37-a446-31f82a5e7601".into(),
                authorization_digest: "a".repeat(64),
                environment_bundle_digest: "b".repeat(64),
                image_digest: format!("sha256:{}", "c".repeat(64)),
                network_digest: "d".repeat(64),
                approval_bundle_digest: "e".repeat(64),
            },
            request_id: "018f0f21-76c0-7b37-a446-31f82a5e7602".into(),
            approval: ApprovalRequest {
                id: "018f0f21-76c0-7b37-a446-31f82a5e7602".into(),
                session_id: SessionId::new("golden"),
                session_display_name: "Golden".into(),
                cwd: "/workspace".into(),
                tool: Tool::Bash,
                tool_input: tool_input.clone(),
                provider: "opencode".into(),
                request_type: RequestType::ToolUse,
                context: ApprovalContext {
                    workspace_roots: vec!["/workspace".into()],
                    hook_event_name: HookEventName::from("permission.ask".to_string()),
                    extra: None,
                },
            },
            tool_input_digest: tool_input_digest(&tool_input).unwrap(),
            deadline: Utc.with_ymd_and_hms(2099, 1, 2, 3, 4, 5).unwrap(),
        }
    }

    fn listener() -> (tempfile::TempDir, std::path::PathBuf, UnixListener) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("approval.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, socket, listener)
    }

    async fn read_request(stream: &mut tokio::net::UnixStream) -> AttendedGatewayRequestV1 {
        let len = stream.read_u32().await.unwrap() as usize;
        let mut body = vec![0; len];
        stream.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn write_decision(
        mut stream: tokio::net::UnixStream,
        request: &AttendedGatewayRequestV1,
    ) {
        let frame = encode_attended_gateway_frame(
            &request.decision(AttendedGatewayDecisionKind::Allow, None),
        )
        .unwrap();
        stream.write_all(&frame).await.unwrap();
    }

    #[tokio::test]
    async fn accepts_one_matching_decision_after_relay_close() {
        let (_dir, socket, listener) = listener();
        let relay = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            write_decision(stream, &request).await;
        });
        let result = round_trip(
            &socket,
            &request(),
            Duration::from_secs(1),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(result.decision, AttendedGatewayDecisionKind::Allow);
        relay.await.unwrap();
    }

    #[test]
    fn rust_matches_cross_language_golden_frames() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../protocol/tests/fixtures/attended_gateway_v1.json"
        ))
        .unwrap();
        serde_json::from_value::<AttendedGatewayRequestV1>(fixture["request"].clone()).unwrap();
        serde_json::from_value::<protocol::AttendedGatewayDecisionV1>(fixture["decision"].clone())
            .unwrap();
        assert_eq!(
            protocol::tool_input_digest(&fixture["tool_input"]).unwrap(),
            fixture["tool_input_digest"]
        );
        for name in ["request", "decision"] {
            let frame = encode_attended_gateway_frame(&fixture[name]).unwrap();
            let bytes = frame[4..].to_vec();
            assert_eq!(
                String::from_utf8(bytes.clone()).unwrap(),
                fixture[format!("{name}_canonical")]
            );
            assert_eq!(
                format!("{:08x}", bytes.len()),
                fixture[format!("{name}_frame_prefix_hex")]
            );
        }
    }

    #[tokio::test]
    async fn rejects_malformed_oversize_replay_and_mismatched_digest() {
        for mode in ["malformed", "oversize", "replay", "digest"] {
            let (_dir, socket, listener) = listener();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                if mode == "oversize" {
                    stream
                        .write_u32((ATTENDED_GATEWAY_MAX_FRAME_BYTES + 1) as u32)
                        .await
                        .unwrap();
                    return;
                }
                if mode == "malformed" {
                    stream.write_u32(1).await.unwrap();
                    stream.write_all(b"{").await.unwrap();
                    return;
                }
                let mut decision = serde_json::to_value(
                    request.decision(AttendedGatewayDecisionKind::Allow, None),
                )
                .unwrap();
                if mode == "digest" {
                    decision["tool_input_digest"] = Value::String("f".repeat(64));
                }
                let frame = encode_attended_gateway_frame(&decision).unwrap();
                stream.write_all(&frame).await.unwrap();
                if mode == "replay" {
                    stream.write_all(b"x").await.unwrap();
                }
            });
            assert!(
                round_trip(
                    &socket,
                    &request(),
                    Duration::from_secs(1),
                    std::future::pending()
                )
                .await
                .is_err(),
                "{mode}"
            );
        }
    }

    #[tokio::test]
    async fn rejects_timeout_cancellation_missing_socket_and_path_replacement() {
        for mode in ["timeout", "cancellation"] {
            let (_dir, socket, listener) = listener();
            tokio::spawn(async move {
                let (_stream, _) = listener.accept().await.unwrap();
                tokio::time::sleep(Duration::from_secs(1)).await;
            });
            let cancellation = async move {
                if mode == "cancellation" {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                } else {
                    std::future::pending().await
                }
            };
            let error = round_trip(&socket, &request(), Duration::from_millis(30), cancellation)
                .await
                .unwrap_err();
            assert!(error.contains(mode), "{mode}: {error}");
        }

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            round_trip(
                &missing,
                &request(),
                Duration::from_millis(30),
                std::future::pending()
            )
            .await
            .unwrap_err()
            .contains("missing")
        );

        let (dir, socket, listener) = listener();
        let original = socket_identity(&socket).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_file(&socket).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = verify_socket_identity(&socket, original).unwrap_err();
        assert!(error.contains("identity changed"), "{error}");
        drop((listener, replacement));
    }

    #[tokio::test]
    async fn in_process_relay_proves_approve_deny_cancel_and_concurrency() {
        let (_dir, socket, listener) = listener();
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let relay_active = Arc::clone(&active);
        let relay_maximum = Arc::clone(&maximum);
        let relay = tokio::spawn(async move {
            let mut handlers = Vec::new();
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let active = Arc::clone(&relay_active);
                let maximum = Arc::clone(&relay_maximum);
                handlers.push(tokio::spawn(async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    let request = read_request(&mut stream).await;
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    let kind = if request.approval.tool_input["decision"] == "deny" {
                        AttendedGatewayDecisionKind::Deny
                    } else {
                        AttendedGatewayDecisionKind::Allow
                    };
                    let frame =
                        encode_attended_gateway_frame(&request.decision(kind, None)).unwrap();
                    let _ = stream.write_all(&frame).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                }));
            }
            for handler in handlers {
                handler.await.unwrap();
            }
        });

        let request_for = |suffix: &str, decision: &str| {
            let mut request = request();
            request.request_id = format!("018f0f21-76c0-7b37-a446-31f82a5e76{suffix}");
            request.approval.id = request.request_id.clone();
            request.approval.tool_input = json!({"decision": decision});
            request.tool_input_digest = tool_input_digest(&request.approval.tool_input).unwrap();
            request
        };
        let approve = request_for("10", "allow");
        let deny = request_for("11", "deny");
        let also_approve = request_for("12", "allow");
        let cancel = request_for("13", "allow");
        let (approved, denied, second_approved, cancelled) = tokio::join!(
            round_trip(
                &socket,
                &approve,
                Duration::from_secs(1),
                std::future::pending()
            ),
            round_trip(
                &socket,
                &deny,
                Duration::from_secs(1),
                std::future::pending()
            ),
            round_trip(
                &socket,
                &also_approve,
                Duration::from_secs(1),
                std::future::pending()
            ),
            round_trip(&socket, &cancel, Duration::from_secs(1), async {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }),
        );
        assert_eq!(
            approved.unwrap().decision,
            AttendedGatewayDecisionKind::Allow
        );
        assert_eq!(denied.unwrap().decision, AttendedGatewayDecisionKind::Deny);
        assert_eq!(
            second_approved.unwrap().decision,
            AttendedGatewayDecisionKind::Allow
        );
        assert!(cancelled.unwrap_err().contains("cancellation"));
        relay.await.unwrap();
        assert!(maximum.load(Ordering::SeqCst) >= 2);
    }
}
