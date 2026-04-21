use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};

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
}

impl McpBridgeInstance {
    /// Start a bridge that listens on a Unix socket and proxies to the host endpoint.
    pub async fn start(config: &McpBridgeConfig) -> anyhow::Result<Self> {
        let container_path = config
            .container_socket_path
            .clone()
            .unwrap_or_else(|| format!("/tmp/mcp-{}.sock", config.name));

        let socket_dir = std::env::temp_dir().join("agent-hub-mcp");
        tokio::fs::create_dir_all(&socket_dir).await?;
        let socket_path = socket_dir.join(format!("{}.sock", config.name));

        let _ = tokio::fs::remove_file(&socket_path).await;

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind bridge socket {}", socket_path.display()))?;
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let endpoint = config.host_endpoint.clone();
        let path = socket_path.clone();

        tokio::spawn(async move {
            if let Err(error) = run_bridge_loop(listener, &endpoint, shutdown_rx).await {
                eprintln!("MCP bridge '{}' error: {error:#}", path.display());
            }
        });

        Ok(Self {
            name: config.name.clone(),
            socket_path,
            container_path,
            shutdown_tx,
        })
    }

    /// Get the Docker mount argument for this bridge socket.
    pub fn docker_mount_arg(&self) -> String {
        format!(
            "{}:{}:rw",
            self.socket_path.to_string_lossy(),
            self.container_path
        )
    }

    /// Stop the bridge listener loop.
    pub fn stop(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

impl Drop for McpBridgeInstance {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

async fn run_bridge_loop(
    listener: UnixListener,
    endpoint: &McpEndpoint,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (client_stream, _) = accept_result?;
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_mcp_connection(client_stream, &endpoint).await {
                        eprintln!("MCP bridge connection error: {error:#}");
                    }
                });
            }
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    Ok(())
}

async fn handle_mcp_connection(client: UnixStream, endpoint: &McpEndpoint) -> anyhow::Result<()> {
    match endpoint {
        McpEndpoint::Unix { path } => {
            let upstream = UnixStream::connect(path).await?;
            bidirectional_copy(client, upstream).await
        }
        McpEndpoint::Tcp { host, port } => {
            let upstream = tokio::net::TcpStream::connect(format!("{host}:{port}")).await?;
            bidirectional_copy_tcp(client, upstream).await
        }
        McpEndpoint::Stdio { command, args, env } => {
            let mut child = tokio::process::Command::new(command)
                .args(args)
                .envs(env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .spawn()?;

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
