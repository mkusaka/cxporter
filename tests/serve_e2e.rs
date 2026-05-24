#![cfg(unix)]

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdin;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;

#[test]
fn serve_exports_and_calls_configured_mcp_server_tools() -> Result<()> {
    let temp = TempDir::new("cxporter-serve-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    write_fake_mcp_server(&fake_server)?;
    write_codex_config(temp.path(), &fake_server)?;

    let mut child = Command::new(env!("CARGO_BIN_EXE_cxporter"))
        .arg("serve")
        .arg("--server")
        .arg("fake")
        .env("CODEX_HOME", temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn cxporter serve")?;
    let mut stdin = child.stdin.take().context("capture cxporter stdin")?;
    let stdout = child.stdout.take().context("capture cxporter stdout")?;
    let stderr = child.stderr.take().context("capture cxporter stderr")?;
    let stdout_rx = spawn_line_reader(stdout);
    let stderr_rx = spawn_line_reader(stderr);
    let _child = ChildGuard(child);

    send_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {
                    "name": "cxporter-serve-e2e",
                    "version": "0.0.0"
                }
            }
        }),
    )?;
    let initialize = recv_response(&stdout_rx, &stderr_rx, 1)?;
    assert_eq!(initialize["result"]["serverInfo"]["name"], "cxporter");

    send_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        }),
    )?;
    send_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }),
    )?;
    let tools = recv_response(&stdout_rx, &stderr_rx, 2)?;
    let tool_names = tools["result"]["tools"]
        .as_array()
        .context("tools/list result should contain tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(tool_names, vec!["fake.echo"]);

    send_request(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "fake.echo",
                "arguments": {
                    "message": "hello from e2e"
                }
            }
        }),
    )?;
    let call = recv_response(&stdout_rx, &stderr_rx, 3)?;
    assert_eq!(
        call["result"]["content"][0]["text"],
        "fake echo response from configured MCP server"
    );
    assert_eq!(call["result"]["isError"], false);

    Ok(())
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before UNIX_EPOCH")?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{unique}", std::process::id()));
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write_codex_config(codex_home: &Path, fake_server: &Path) -> Result<()> {
    let config = format!(
        r#"approval_policy = "never"

[mcp_servers.fake]
command = "{}"
startup_timeout_sec = 5.0
tool_timeout_sec = 5.0
"#,
        escape_toml_string(&fake_server.to_string_lossy()),
    );
    fs::write(codex_home.join("config.toml"), config).context("write Codex config.toml")
}

fn write_fake_mcp_server(path: &Path) -> Result<()> {
    fs::write(
        path,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*|*'"method": "initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fake-mcp","version":"0.0.0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*|*'"method": "tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo input","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*|*'"method": "tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"fake echo response from configured MCP server"}],"isError":false}}\n' "$id"
      ;;
  esac
done
"#,
    )
    .with_context(|| format!("write fake MCP server {}", path.display()))?;

    let mut permissions = fs::metadata(path)
        .with_context(|| format!("stat fake MCP server {}", path.display()))?
        .permissions();
    use std::os::unix::fs::PermissionsExt as _;
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("chmod fake MCP server {}", path.display()))
}

fn spawn_line_reader<R>(reader: R) -> mpsc::Receiver<String>
where
    R: std::io::Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else {
                break;
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

fn send_request(stdin: &mut ChildStdin, request: Value) -> Result<()> {
    serde_json::to_writer(&mut *stdin, &request).context("serialize JSON-RPC request")?;
    stdin.write_all(b"\n").context("write JSON-RPC newline")?;
    stdin.flush().context("flush JSON-RPC request")
}

fn recv_response(
    stdout_rx: &mpsc::Receiver<String>,
    stderr_rx: &mpsc::Receiver<String>,
    id: u64,
) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            bail!(
                "timed out waiting for JSON-RPC response id {id}; stderr: {}",
                drain(stderr_rx)
            );
        };
        match stdout_rx.recv_timeout(remaining.min(Duration::from_millis(250))) {
            Ok(line) => {
                let value: Value = serde_json::from_str(&line)
                    .with_context(|| format!("parse cxporter stdout as JSON: {line}"))?;
                if value["id"].as_u64() == Some(id) {
                    return Ok(value);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!(
                    "cxporter stdout closed before JSON-RPC response id {id}; stderr: {}",
                    drain(stderr_rx)
                );
            }
        }
    }
}

fn drain(rx: &mpsc::Receiver<String>) -> String {
    let lines = rx.try_iter().collect::<Vec<_>>();
    if lines.is_empty() {
        "<empty>".to_string()
    } else {
        lines.join("\n")
    }
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
