use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};

use super::ExecutionCancellationContext;

/// MCP bridge configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpBridgeConfig {
    /// Name of this MCP bridge (e.g., "glean", "jira")
    pub name: String,
    /// Host-side MCP server connection info
    pub host_endpoint: McpEndpoint,
    /// Path inside container where the socket will appear
    pub container_socket_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpEndpoint {
    /// Connect to a Unix socket on the host
    #[serde(rename = "unix")]
    Unix { path: String },
    /// Connect to a TCP endpoint on the host
    #[serde(rename = "tcp")]
    Tcp { host: String, port: u16 },
    /// Spawn a local command as MCP server (stdio transport)
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
}

/// A running MCP bridge instance
pub struct McpBridgeInstance {
    pub name: String,
    pub socket_path: PathBuf,
    pub container_path: String,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    bridge_task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    cancellation: ExecutionCancellationContext,
}

impl McpBridgeInstance {
    /// Start a bridge that listens on a Unix socket and proxies to the host endpoint.
    pub async fn start(
        config: &McpBridgeConfig,
        runtime_root: &std::path::Path,
        cancellation: &ExecutionCancellationContext,
    ) -> anyhow::Result<Self> {
        cancellation.check()?;
        let container_path = config
            .container_socket_path
            .clone()
            .unwrap_or_else(|| format!("/tmp/mcp-{}.sock", config.name));

        let socket_dir = runtime_root.join("mcp");
        let directory = cancellation.begin_external_operation(
            "mcp_socket_directory_create",
            &socket_dir.to_string_lossy(),
        )?;
        tokio::fs::create_dir_all(&socket_dir).await?;
        directory.completed()?;
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        config.name.hash(&mut hasher);
        let socket_path = socket_dir.join(format!("{:016x}.sock", hasher.finish()));

        if socket_path.exists() {
            let remove = cancellation.begin_external_operation(
                "mcp_stale_socket_remove",
                &socket_path.to_string_lossy(),
            )?;
            tokio::fs::remove_file(&socket_path).await?;
            remove.completed()?;
        }

        let bind = cancellation
            .begin_external_operation("mcp_socket_bind", &socket_path.to_string_lossy())?;
        let listener = bind_runtime_socket(&socket_dir, &socket_path)
            .with_context(|| format!("bind bridge socket {}", socket_path.display()))?;
        bind.completed()?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let endpoint = config.host_endpoint.clone();
        let path = socket_path.clone();

        let bridge_cancellation = cancellation.clone();
        let bridge_task = tokio::spawn(async move {
            run_bridge_loop(listener, &endpoint, shutdown_rx, &bridge_cancellation)
                .await
                .with_context(|| format!("MCP bridge '{}' loop", path.display()))
        });

        Ok(Self {
            name: config.name.clone(),
            socket_path,
            container_path,
            shutdown_tx,
            bridge_task: Some(bridge_task),
            cancellation: cancellation.clone(),
        })
    }

    /// Get the Docker mount argument for this bridge socket.
    pub fn docker_mount_arg(&self) -> anyhow::Result<String> {
        Ok(
            agent_hub_executor::DockerMount::new(&self.socket_path, &self.container_path, false)?
                .argument(),
        )
    }

    /// Stop the bridge listener loop.
    pub async fn stop(
        &mut self,
        cancellation: &ExecutionCancellationContext,
    ) -> anyhow::Result<()> {
        let _ = self.shutdown_tx.send(true);
        if let Some(task) = self.bridge_task.take() {
            task.await.context("MCP bridge task failed")??;
        }
        if self.socket_path.exists() {
            let remove = cancellation.begin_external_operation(
                "mcp_socket_remove",
                &self.socket_path.to_string_lossy(),
            )?;
            tokio::fs::remove_file(&self.socket_path).await?;
            remove.completed()?;
            cancellation
                .operations
                .reconcile_owned_resource(&self.socket_path.to_string_lossy())?;
        }
        Ok(())
    }
}

fn bind_runtime_socket(
    socket_dir: &std::path::Path,
    socket_path: &std::path::Path,
) -> std::io::Result<UnixListener> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        if socket_path.as_os_str().as_bytes().len() < 108 {
            return UnixListener::bind(socket_path);
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;
        let directory = std::fs::File::open(socket_dir)?;
        let file_name = socket_path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path has no file name",
            )
        })?;
        let bounded_path = std::path::Path::new("/proc/self/fd")
            .join(directory.as_raw_fd().to_string())
            .join(file_name);
        UnixListener::bind(bounded_path)
    }
    #[cfg(not(target_os = "linux"))]
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "runtime MCP socket path exceeds the platform Unix socket path limit",
    ))
}

async fn connect_unix_socket(path: &std::path::Path) -> std::io::Result<UnixStream> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        if path.as_os_str().as_bytes().len() < 108 {
            return UnixStream::connect(path).await;
        }
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd as _;
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path has no parent",
            )
        })?;
        let directory = std::fs::File::open(parent)?;
        let file_name = path.file_name().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "socket path has no file name",
            )
        })?;
        let bounded_path = std::path::Path::new("/proc/self/fd")
            .join(directory.as_raw_fd().to_string())
            .join(file_name);
        UnixStream::connect(bounded_path).await
    }
    #[cfg(not(target_os = "linux"))]
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "Unix socket path exceeds the platform path limit",
    ))
}

impl Drop for McpBridgeInstance {
    fn drop(&mut self) {
        if let Some(task) = self.bridge_task.take() {
            task.abort();
            let error =
                anyhow::anyhow!("MCP bridge '{}' dropped without awaited cleanup", self.name);
            let _ = self.cancellation.phase.containment_failed(&error);
            if std::thread::panicking() {
                std::process::abort();
            }
            panic!("{error:#}");
        }
    }
}

async fn run_bridge_loop(
    listener: UnixListener,
    endpoint: &McpEndpoint,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (client_stream, _) = accept_result?;
                let endpoint = endpoint.clone();
                let cancellation = cancellation.clone();
                connections.spawn(async move {
                    if let Err(error) =
                        handle_mcp_connection(client_stream, &endpoint, &cancellation).await
                    {
                        eprintln!("MCP bridge connection error: {error:#}");
                    }
                    cancellation.check()
                });
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }
    connections.abort_all();
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            return Err(error).context("MCP bridge connection task failed");
        }
    }
    Ok(())
}

async fn handle_mcp_connection(
    client: UnixStream,
    endpoint: &McpEndpoint,
    cancellation: &ExecutionCancellationContext,
) -> anyhow::Result<()> {
    match endpoint {
        McpEndpoint::Unix { path } => {
            let operation =
                cancellation.begin_external_operation("mcp_upstream_connect", "unix_endpoint")?;
            let upstream = connect_unix_socket(std::path::Path::new(path)).await?;
            operation.completed()?;
            bidirectional_copy(client, upstream).await
        }
        McpEndpoint::Tcp { host, port } => {
            let operation = cancellation
                .begin_external_operation("mcp_upstream_connect", &format!("tcp:{host}:{port}"))?;
            let upstream = tokio::net::TcpStream::connect(format!("{host}:{port}")).await?;
            operation.completed()?;
            bidirectional_copy_tcp(client, upstream).await
        }
        McpEndpoint::Stdio { command, args, env } => {
            let operation =
                cancellation.begin_external_operation("mcp_helper_spawn", "stdio_endpoint")?;
            let spawn_operation_id = operation.operation_id();
            let mut child_command = tokio::process::Command::new(command);
            child_command
                .args(args)
                .envs(env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                child_command.as_std_mut().process_group(0);
            }
            let mut child = child_command.spawn()?;
            let pid = child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .context("MCP helper has no valid PID")?;
            let identity = super::read_process_identity(pid)?;
            let helper = cancellation.begin_external_operation(
                "mcp_helper_process",
                &super::linked_helper_resource(
                    spawn_operation_id,
                    identity.pid,
                    identity.pgid,
                    &identity.linux_boot_id,
                    identity.proc_start_time_ticks,
                )?,
            )?;
            operation.completed()?;

            let mut child_stdin = child
                .stdin
                .take()
                .context("stdio MCP child missing stdin")?;
            let mut child_stdout = child
                .stdout
                .take()
                .context("stdio MCP child missing stdout")?;
            let (mut client_read, mut client_write) = client.into_split();

            let client_to_child = tokio::io::copy(&mut client_read, &mut child_stdin);
            let child_to_client = tokio::io::copy(&mut child_stdout, &mut client_write);
            tokio::try_join!(client_to_child, child_to_client)?;

            let _ = child.wait().await;
            super::verify_process_group_absent(&identity).await?;
            helper.reconciled()?;
            Ok(())
        }
    }
}

async fn bidirectional_copy(mut a: UnixStream, mut b: UnixStream) -> anyhow::Result<()> {
    tokio::io::copy_bidirectional(&mut a, &mut b).await?;
    Ok(())
}

async fn bidirectional_copy_tcp(
    mut unix: UnixStream,
    mut tcp: tokio::net::TcpStream,
) -> anyhow::Result<()> {
    tokio::io::copy_bidirectional(&mut unix, &mut tcp).await?;
    Ok(())
}

pub fn mcp_socket_env_var(name: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut previous_was_underscore = false;

    for ch in name.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_uppercase()
        } else {
            '_'
        };
        if mapped == '_' {
            if !previous_was_underscore {
                normalized.push('_');
            }
            previous_was_underscore = true;
        } else {
            normalized.push(mapped);
            previous_was_underscore = false;
        }
    }

    let normalized = normalized.trim_matches('_');
    if normalized.is_empty() {
        None
    } else {
        Some(format!("MCP_{normalized}_SOCKET"))
    }
}

pub fn default_container_socket_path(name: &str) -> String {
    format!("/tmp/mcp-{}.sock", name)
}

pub fn container_socket_path(config: &McpBridgeConfig) -> String {
    config
        .container_socket_path
        .clone()
        .unwrap_or_else(|| default_container_socket_path(&config.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn socket_bridge_setup_proxy_and_teardown_leave_no_pending_cleanup() {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir().join(format!("m-{}", &id[..8]));
        let upstream_path = root.join("upstream.sock");
        std::fs::create_dir_all(&root).unwrap();
        let upstream = UnixListener::bind(&upstream_path).unwrap();
        let upstream_task = tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut bytes = [0_u8; 4];
            stream.read_exact(&mut bytes).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
        });
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let cancellation = ExecutionCancellationContext::for_test(&root.join("attempt"), stop_rx)
            .expect("cancellation context");
        let name = format!("test-{}", uuid::Uuid::new_v4().simple());
        let mut bridge = McpBridgeInstance::start(
            &McpBridgeConfig {
                name,
                host_endpoint: McpEndpoint::Unix {
                    path: upstream_path.to_string_lossy().into_owned(),
                },
                container_socket_path: None,
            },
            &root.join("runtime"),
            &cancellation,
        )
        .await
        .expect("start bridge");
        assert!(
            std::fs::symlink_metadata(&bridge.socket_path)
                .unwrap()
                .file_type()
                .is_socket()
        );
        let mut client = connect_unix_socket(&bridge.socket_path).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut reply = [0_u8; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ping");
        drop(client);
        upstream_task.await.unwrap();
        bridge.stop(&cancellation).await.expect("stop bridge");
        assert!(!bridge.socket_path.exists());
        assert!(cancellation.operations.unresolved().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn socket_bridge_binds_long_explicit_runtime_path_without_temp_fallback() {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir()
            .join(format!("m-{}", &id[..8]))
            .join("explicit-runtime-root-component-that-exceeds-the-unix-socket-address-limit")
            .join("another-explicit-runtime-root-component");
        std::fs::create_dir_all(&root).unwrap();
        let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let cancellation = ExecutionCancellationContext::for_test(&root.join("attempt"), stop_rx)
            .expect("cancellation context");
        let mut bridge = McpBridgeInstance::start(
            &McpBridgeConfig {
                name: "long-path".to_string(),
                host_endpoint: McpEndpoint::Tcp {
                    host: "127.0.0.1".to_string(),
                    port: 9,
                },
                container_socket_path: None,
            },
            &root.join("runtime"),
            &cancellation,
        )
        .await
        .expect("start bridge through bounded descriptor path");
        assert!(bridge.socket_path.starts_with(&root));
        assert!(bridge.socket_path.exists());
        bridge.stop(&cancellation).await.unwrap();
        std::fs::remove_dir_all(
            root.ancestors()
                .nth(2)
                .expect("test root has expected components"),
        )
        .unwrap();
    }
}
