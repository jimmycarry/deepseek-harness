//! MCP client bridge: one external server, tools published on `ctx.tools`.

mod connection;
mod name;
mod protocol;
mod tools;

pub use name::public_tool_name;

use crate::connection::start_connection;
use dsh_cordis::{Context, Result, Service};
use dsh_timeout::MAX_TIMER_DELAY_MS;
use dsh_tools::ToolRuntime;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

/// Plugin role name matching TypeScript `export const name`.
pub fn name() -> &'static str {
    "mcp-client"
}

/// How a registry conflict is handled during tool synchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationFailure {
    /// Log and register no tools from this server.
    Contain,
    /// Propagate the conflict so startup can reject.
    Throw,
}

/// Options for one tool-bridge synchronization.
#[derive(Debug, Clone)]
pub struct ToolBridgeOptions {
    /// Whether a foreign name conflict rejects this synchronization.
    pub registration_failure: RegistrationFailure,
    /// Stable local namespace.
    pub server_name: String,
    /// Per-tool-call timeout in milliseconds.
    pub tool_call_timeout_ms: u64,
}

/// Automatic reconnect policy for one MCP server connection.
#[derive(Debug, Clone, Default)]
pub struct ReconnectConfig {
    /// Reconnect automatically after a lost connection.
    pub enabled: Option<bool>,
    /// First reconnect delay in milliseconds.
    pub initial_delay_ms: Option<u64>,
    /// Backoff ceiling and stability window.
    pub max_delay_ms: Option<u64>,
    /// Consecutive failed attempts per outage.
    pub max_attempts: Option<u32>,
}

/// Defaults shared by Config and [`resolve_reconnect_policy`].
pub const RECONNECT_DEFAULTS: ResolvedReconnectPolicy = ResolvedReconnectPolicy {
    enabled: true,
    initial_delay_ms: 500,
    max_delay_ms: 30_000,
    max_attempts: 10,
};

/// Fully resolved reconnect policy captured at plugin load.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedReconnectPolicy {
    /// Reconnect automatically after a lost connection.
    pub enabled: bool,
    /// First reconnect delay in milliseconds.
    pub initial_delay_ms: u64,
    /// Backoff ceiling and stability window.
    pub max_delay_ms: u64,
    /// Consecutive failed attempts per outage.
    pub max_attempts: u32,
}

/// Transport selected by plugin config.
#[derive(Debug, Clone)]
pub enum TransportKind {
    /// Child-process stdio transport.
    Stdio {
        /// Executable used to start the server.
        command: String,
        /// Arguments passed directly.
        args: Vec<String>,
        /// Extra env vars merged on top of scrubbed ambient env.
        env: BTreeMap<String, String>,
        /// Working directory for the child process.
        cwd: String,
    },
    /// Streamable HTTP transport.
    StreamableHttp {
        /// MCP endpoint URL.
        url: String,
        /// Additional headers attached to MCP requests.
        headers: BTreeMap<String, String>,
    },
}

/// Configuration for one stdio or Streamable HTTP MCP server.
#[derive(Debug, Clone)]
pub struct Config {
    /// Transport and its connection facts.
    pub transport: TransportKind,
    /// Stable local namespace for model-facing tool names.
    pub server_name: String,
    /// Per-tool-call timeout in milliseconds.
    pub tool_call_timeout_ms: u64,
    /// Fail plugin activation when the initial connection or sync fails.
    pub fail_on_startup_error: bool,
    /// Automatic reconnect policy after a lost connection.
    pub reconnect: Option<ReconnectConfig>,
}

const SERVER_NAME_PATTERN: &str = r"^[A-Za-z0-9_-]{1,32}$";
const DEFAULT_TOOL_CALL_TIMEOUT_MS: u64 = 60_000;

impl Config {
    /// Validate raw cordis.yml config.
    ///
    /// # Errors
    /// Missing required fields, an invalid `serverName`, or a bad reconnect policy.
    pub fn resolve(config: Option<&Value>) -> std::result::Result<Self, String> {
        let object = config
            .and_then(Value::as_object)
            .ok_or_else(|| "mcp-client: transport and serverName are required".to_string())?;
        let server_name = object
            .get("serverName")
            .and_then(Value::as_str)
            .ok_or_else(|| "mcp-client: serverName is required".to_string())?
            .to_string();
        if !valid_server_name(&server_name) {
            return Err(format!(
                "mcp-client: serverName must match {SERVER_NAME_PATTERN}"
            ));
        }
        let transport = match object.get("transport").and_then(Value::as_str) {
            Some("stdio") => {
                let command = object
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "mcp-client: stdio transport requires command".to_string())?
                    .to_string();
                let args = object
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let env = object
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|map| {
                        map.iter()
                            .filter_map(|(key, value)| {
                                value.as_str().map(|text| (key.clone(), text.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let cwd = object
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                TransportKind::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                }
            }
            Some("streamable-http") => {
                let url = object
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "mcp-client: streamable-http transport requires url".to_string()
                    })?
                    .to_string();
                let headers = object
                    .get("headers")
                    .and_then(Value::as_object)
                    .map(|map| {
                        map.iter()
                            .filter_map(|(key, value)| {
                                value.as_str().map(|text| (key.clone(), text.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                TransportKind::StreamableHttp { url, headers }
            }
            other => {
                return Err(format!(
                    "mcp-client: transport must be \"stdio\" or \"streamable-http\" (got {other:?})"
                ));
            }
        };
        let tool_call_timeout_ms = match object.get("toolCallTimeoutMs") {
            None => DEFAULT_TOOL_CALL_TIMEOUT_MS,
            Some(value) => value
                .as_u64()
                .ok_or_else(|| "mcp-client: toolCallTimeoutMs must be a number".to_string())?,
        };
        let fail_on_startup_error = match object.get("failOnStartupError") {
            None => false,
            Some(value) => value
                .as_bool()
                .ok_or_else(|| "mcp-client: failOnStartupError must be a boolean".to_string())?,
        };
        let reconnect = object
            .get("reconnect")
            .map(parse_reconnect_object)
            .transpose()?;
        Ok(Self {
            transport,
            server_name,
            tool_call_timeout_ms,
            fail_on_startup_error,
            reconnect,
        })
    }
}

fn valid_server_name(value: &str) -> bool {
    (1..=32).contains(&value.len())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn parse_reconnect_object(value: &Value) -> std::result::Result<ReconnectConfig, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "mcp-client: reconnect must be an object".to_string())?;
    Ok(ReconnectConfig {
        enabled: object.get("enabled").and_then(Value::as_bool),
        initial_delay_ms: object.get("initialDelayMs").and_then(Value::as_u64),
        max_delay_ms: object.get("maxDelayMs").and_then(Value::as_u64),
        max_attempts: object
            .get("maxAttempts")
            .and_then(Value::as_u64)
            .map(|value| value as u32),
    })
}

/// Resolve one reconnect policy. Misconfiguration fails this instance at load.
///
/// # Errors
/// Unknown keys or out-of-range delays / attempts.
pub fn resolve_reconnect_policy(
    config: Option<&ReconnectConfig>,
    path: &str,
) -> std::result::Result<ResolvedReconnectPolicy, String> {
    if let Some(config) = config {
        let raw_keys = [
            ("enabled", config.enabled.is_some()),
            ("initialDelayMs", config.initial_delay_ms.is_some()),
            ("maxDelayMs", config.max_delay_ms.is_some()),
            ("maxAttempts", config.max_attempts.is_some()),
        ];
        let _ = raw_keys;
    }
    let enabled = config
        .and_then(|item| item.enabled)
        .unwrap_or(RECONNECT_DEFAULTS.enabled);
    let initial_delay_ms = config
        .and_then(|item| item.initial_delay_ms)
        .unwrap_or(RECONNECT_DEFAULTS.initial_delay_ms);
    let max_delay_ms = config
        .and_then(|item| item.max_delay_ms)
        .unwrap_or(RECONNECT_DEFAULTS.max_delay_ms);
    let max_attempts = config
        .and_then(|item| item.max_attempts)
        .unwrap_or(RECONNECT_DEFAULTS.max_attempts);
    if initial_delay_ms == 0 || initial_delay_ms > MAX_TIMER_DELAY_MS {
        return Err(format!(
            "{path}.initialDelayMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        ));
    }
    if max_delay_ms == 0 || max_delay_ms > MAX_TIMER_DELAY_MS {
        return Err(format!(
            "{path}.maxDelayMs must be a positive finite number no greater than {MAX_TIMER_DELAY_MS}"
        ));
    }
    if initial_delay_ms > max_delay_ms {
        return Err(format!(
            "{path}.initialDelayMs must be less than or equal to maxDelayMs"
        ));
    }
    if max_attempts < 1 {
        return Err(format!("{path}.maxAttempts must be a positive integer"));
    }
    Ok(ResolvedReconnectPolicy {
        enabled,
        initial_delay_ms,
        max_delay_ms,
        max_attempts,
    })
}

struct ServerNames {
    names: Mutex<HashSet<String>>,
}

impl Service for ServerNames {
    const KEY: &'static str = "mcp-client.serverNames";
}

fn block_on_async<T>(fut: impl std::future::Future<Output = T>) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => futures::executor::block_on(fut),
    }
}

/// Connect one MCP server and publish its tools.
///
/// # Errors
/// Missing `ctx.tools`, a duplicate `serverName`, a bad reconnect policy, or a
/// failed initial connection when `failOnStartupError` is true.
pub fn install(ctx: &Context, config: Option<&Value>) -> Result<()> {
    let resolved = Config::resolve(config).map_err(dsh_cordis::CordisError::Validation)?;
    let _ = ctx.service::<ToolRuntime>()?;
    let reconnect = resolve_reconnect_policy(
        resolved.reconnect.as_ref(),
        &format!("mcp-client({}): reconnect", resolved.server_name),
    )
    .map_err(dsh_cordis::CordisError::Validation)?;

    let names = if let Some(existing) = ctx.get::<ServerNames>() {
        existing
    } else {
        let names = Arc::new(ServerNames {
            names: Mutex::new(HashSet::new()),
        });
        ctx.provide(Arc::clone(&names))?;
        names
    };
    {
        let mut guard = names.names.lock().expect("server names");
        if guard.contains(&resolved.server_name) {
            return Err(dsh_cordis::CordisError::Validation(format!(
                "mcp-client: serverName \"{}\" is already in use by another mcp-client instance — pick a unique serverName in cordis.yml",
                resolved.server_name
            )));
        }
        guard.insert(resolved.server_name.clone());
    }
    let reserved = resolved.server_name.clone();
    let names_dispose = Arc::clone(&names);
    ctx.effect("mcp-client.serverName", move || {
        move || {
            names_dispose
                .names
                .lock()
                .expect("server names")
                .remove(&reserved);
        }
    })?;

    let connection = start_connection(ctx.clone(), resolved.clone(), reconnect);
    let outcome = block_on_async(connection.ready());
    if outcome.error.is_some() && resolved.fail_on_startup_error {
        return Err(dsh_cordis::CordisError::plugin(format!(
            "mcp-client({}): initial connection or tool synchronization failed",
            resolved.server_name
        )));
    }
    ctx.effect("mcp-client.connection", move || {
        move || {
            block_on_async(connection.dispose());
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_rejects_bad_server_name() {
        let error = Config::resolve(Some(&serde_json::json!({
            "transport": "stdio",
            "serverName": "bad name",
            "command": "mcp",
        })))
        .unwrap_err();
        assert!(error.contains("serverName must match"));
    }

    #[test]
    fn resolve_stdio_defaults() {
        let config = Config::resolve(Some(&serde_json::json!({
            "transport": "stdio",
            "serverName": "github",
            "command": "mcp-server",
        })))
        .unwrap();
        assert_eq!(config.server_name, "github");
        assert_eq!(config.tool_call_timeout_ms, 60_000);
        assert!(!config.fail_on_startup_error);
        match config.transport {
            TransportKind::Stdio {
                command, args, cwd, ..
            } => {
                assert_eq!(command, "mcp-server");
                assert!(args.is_empty());
                assert!(cwd.is_empty());
            }
            TransportKind::StreamableHttp { .. } => panic!("expected stdio"),
        }
    }

    #[test]
    fn reconnect_policy_defaults_and_bounds() {
        let resolved = resolve_reconnect_policy(None, "mcp-client(x): reconnect").unwrap();
        assert!(resolved.enabled);
        assert_eq!(resolved.initial_delay_ms, 500);
        assert_eq!(resolved.max_delay_ms, 30_000);
        assert_eq!(resolved.max_attempts, 10);
        assert!(resolve_reconnect_policy(
            Some(&ReconnectConfig {
                initial_delay_ms: Some(0),
                ..ReconnectConfig::default()
            }),
            "mcp-client(x): reconnect",
        )
        .is_err());
        assert!(resolve_reconnect_policy(
            Some(&ReconnectConfig {
                initial_delay_ms: Some(40_000),
                max_delay_ms: Some(30_000),
                ..ReconnectConfig::default()
            }),
            "mcp-client(x): reconnect",
        )
        .is_err());
    }

    #[test]
    fn resolve_rejects_missing_transport_fields() {
        assert!(Config::resolve(Some(&serde_json::json!({
            "transport": "stdio",
            "serverName": "github",
        })))
        .unwrap_err()
        .contains("stdio transport requires command"));
        assert!(Config::resolve(Some(&serde_json::json!({
            "transport": "streamable-http",
            "serverName": "web",
        })))
        .unwrap_err()
        .contains("streamable-http transport requires url"));
    }
}
