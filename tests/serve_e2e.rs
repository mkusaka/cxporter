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
use std::process::ExitStatus;
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

#[test]
fn list_includes_enabled_plugin_mcp_server() -> Result<()> {
    let temp = TempDir::new("cxporter-plugin-list-e2e")?;
    let fake_server = temp.path().join("plugin_fake_mcp_server.sh");
    write_named_fake_mcp_server(&fake_server, "plugin_tool")?;
    write_plugin_codex_config(temp.path(), "fixture@local")?;
    write_plugin_fixture(
        temp.path(),
        "local",
        "fixture",
        "0.1.0",
        "plugin-fake",
        &fake_server,
        None,
    )?;

    let output = run_cxporter(temp.path(), &["list", "--server", "plugin-fake"], None)?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let value: Value = serde_json::from_str(&output.stdout).context("parse list JSON")?;
    let tools = value[0]["tools"].as_object().context("tools object")?;
    assert!(tools.contains_key("plugin_tool"));

    Ok(())
}

#[test]
fn user_defined_mcp_server_shadows_plugin_mcp_server() -> Result<()> {
    let temp = TempDir::new("cxporter-plugin-shadow-e2e")?;
    let user_server = temp.path().join("user_fake_mcp_server.sh");
    let plugin_server = temp.path().join("plugin_fake_mcp_server.sh");
    write_named_fake_mcp_server(&user_server, "user_tool")?;
    write_named_fake_mcp_server(&plugin_server, "plugin_tool")?;
    write_shadow_codex_config(temp.path(), "shadow@local", "same-name", &user_server)?;
    write_plugin_fixture(
        temp.path(),
        "local",
        "shadow",
        "0.1.0",
        "same-name",
        &plugin_server,
        None,
    )?;

    let output = run_cxporter(temp.path(), &["list", "--server", "same-name"], None)?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let value: Value = serde_json::from_str(&output.stdout).context("parse list JSON")?;
    let tools = value[0]["tools"].as_object().context("tools object")?;
    assert!(tools.contains_key("user_tool"));
    assert!(!tools.contains_key("plugin_tool"));

    Ok(())
}

#[test]
fn plugin_relative_cwd_is_resolved_from_plugin_root() -> Result<()> {
    let temp = TempDir::new("cxporter-plugin-cwd-e2e")?;
    write_plugin_codex_config(temp.path(), "cwd-fixture@local")?;
    let plugin_root = write_plugin_fixture(
        temp.path(),
        "local",
        "cwd-fixture",
        "0.1.0",
        "cwd-plugin",
        Path::new("./fake_mcp_server.sh"),
        Some("."),
    )?;
    write_named_fake_mcp_server(&plugin_root.join("fake_mcp_server.sh"), "cwd_tool")?;

    let output = run_cxporter(temp.path(), &["list", "--server", "cwd-plugin"], None)?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let value: Value = serde_json::from_str(&output.stdout).context("parse list JSON")?;
    let tools = value[0]["tools"].as_object().context("tools object")?;
    assert!(tools.contains_key("cwd_tool"));

    Ok(())
}

#[test]
fn list_filters_tools_and_reports_exported_aliases() -> Result<()> {
    let temp = TempDir::new("cxporter-list-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    let state_file = temp.path().join("fake_state");
    write_batch_fake_mcp_server(&fake_server, &state_file)?;
    write_codex_config(temp.path(), &fake_server)?;

    let output = run_cxporter(
        temp.path(),
        &["list", "--server", "fake", "--name-contains", "confluence"],
        None,
    )?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let value: Value = serde_json::from_str(&output.stdout).context("parse list JSON")?;
    let tools = value[0]["tools"].as_object().context("tools object")?;
    assert!(tools.contains_key("confluence_search"));
    assert!(!tools.contains_key("echo"));
    assert_eq!(
        value[0]["toolAliases"]["confluence_search"]["exportedName"],
        "fake.confluence_search"
    );

    Ok(())
}

#[test]
fn call_reads_args_file_and_accepts_exported_tool_alias() -> Result<()> {
    let temp = TempDir::new("cxporter-call-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    let state_file = temp.path().join("fake_state");
    let args_file = temp.path().join("args.json");
    write_batch_fake_mcp_server(&fake_server, &state_file)?;
    write_codex_config(temp.path(), &fake_server)?;
    fs::write(&args_file, r#"{"message":"from file"}"#).context("write args file")?;

    let output = run_cxporter(
        temp.path(),
        &[
            "call",
            "fake",
            "fake.echo",
            "--args-file",
            args_file.to_str().context("args path utf8")?,
        ],
        None,
    )?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let value: Value = serde_json::from_str(&output.stdout).context("parse call JSON")?;
    assert_eq!(value["content"][0]["text"], "echo ok");

    Ok(())
}

#[test]
fn call_reads_args_file_from_stdin() -> Result<()> {
    let temp = TempDir::new("cxporter-call-stdin-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    let state_file = temp.path().join("fake_state");
    write_batch_fake_mcp_server(&fake_server, &state_file)?;
    write_codex_config(temp.path(), &fake_server)?;

    let output = run_cxporter(
        temp.path(),
        &["call", "fake", "echo", "--args-file", "-"],
        Some(r#"{"message":"from stdin"}"#),
    )?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let value: Value = serde_json::from_str(&output.stdout).context("parse call JSON")?;
    assert_eq!(value["content"][0]["text"], "echo ok");

    Ok(())
}

#[test]
fn batch_outputs_jsonl_and_continues_after_failures() -> Result<()> {
    let temp = TempDir::new("cxporter-batch-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    let state_file = temp.path().join("fake_state");
    let calls_file = temp.path().join("calls.jsonl");
    write_batch_fake_mcp_server(&fake_server, &state_file)?;
    write_codex_config(temp.path(), &fake_server)?;
    fs::write(
        &calls_file,
        concat!(
            "{\"tool\":\"echo\",\"arguments\":{\"message\":\"ok\"}}\n",
            "{\"tool\":\"error_tool\",\"arguments\":{}}\n",
            "{\"tool\":\"echo\",\"arguments\":{}}\n",
            "not json\n",
        ),
    )
    .context("write calls jsonl")?;

    let output = run_cxporter(
        temp.path(),
        &[
            "batch",
            "--server",
            "fake",
            "--input",
            calls_file.to_str().context("calls path utf8")?,
            "--concurrency",
            "1",
        ],
        None,
    )?;
    assert!(
        !output.status.success(),
        "batch should fail when any line fails"
    );
    let lines = parse_jsonl(&output.stdout)?;
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["line"], 1);
    assert_eq!(lines[0]["success"], true);
    assert_eq!(lines[1]["line"], 2);
    assert_eq!(lines[1]["success"], false);
    assert_eq!(lines[1]["error"], "tool returned isError=true");
    assert_eq!(lines[2]["line"], 3);
    assert_eq!(lines[2]["success"], false);
    assert!(
        lines[2]["error"]
            .as_str()
            .unwrap()
            .contains("missing required properties")
    );
    assert_eq!(lines[3]["line"], 4);
    assert_eq!(lines[3]["success"], false);
    assert!(
        lines[3]["error"]
            .as_str()
            .unwrap()
            .contains("invalid JSONL")
    );

    Ok(())
}

#[test]
fn batch_rejects_parallel_non_parallel_tools_without_force() -> Result<()> {
    let temp = TempDir::new("cxporter-batch-parallel-guard-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    let state_file = temp.path().join("fake_state");
    let calls_file = temp.path().join("calls.jsonl");
    write_batch_fake_mcp_server(&fake_server, &state_file)?;
    write_codex_config(temp.path(), &fake_server)?;
    fs::write(
        &calls_file,
        concat!(
            "{\"tool\":\"echo\",\"arguments\":{\"message\":\"one\"}}\n",
            "{\"tool\":\"echo\",\"arguments\":{\"message\":\"two\"}}\n",
        ),
    )
    .context("write calls jsonl")?;

    let output = run_cxporter(
        temp.path(),
        &[
            "batch",
            "--server",
            "fake",
            "--input",
            calls_file.to_str().context("calls path utf8")?,
            "--concurrency",
            "2",
        ],
        None,
    )?;
    assert!(
        !output.status.success(),
        "batch should fail when non-parallel tools are run concurrently"
    );
    assert!(
        output.stdout.trim().is_empty(),
        "batch should not emit partial JSONL for a concurrency policy error"
    );
    assert!(
        output
            .stderr
            .contains("do not advertise parallel call support"),
        "stderr was: {}",
        output.stderr
    );
    assert!(output.stderr.contains("raw=echo exported=fake.echo"));

    Ok(())
}

#[test]
fn batch_allows_force_parallel_for_non_parallel_tools() -> Result<()> {
    let temp = TempDir::new("cxporter-batch-force-parallel-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    let state_file = temp.path().join("fake_state");
    let calls_file = temp.path().join("calls.jsonl");
    write_batch_fake_mcp_server(&fake_server, &state_file)?;
    write_codex_config(temp.path(), &fake_server)?;
    fs::write(
        &calls_file,
        concat!(
            "{\"tool\":\"echo\",\"arguments\":{\"message\":\"one\"}}\n",
            "{\"tool\":\"echo\",\"arguments\":{\"message\":\"two\"}}\n",
        ),
    )
    .context("write calls jsonl")?;

    let output = run_cxporter(
        temp.path(),
        &[
            "batch",
            "--server",
            "fake",
            "--input",
            calls_file.to_str().context("calls path utf8")?,
            "--concurrency",
            "2",
            "--force-parallel",
        ],
        None,
    )?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let lines = parse_jsonl(&output.stdout)?;
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|line| line["success"] == true));

    Ok(())
}

#[test]
fn batch_allows_parallel_read_only_tools() -> Result<()> {
    let temp = TempDir::new("cxporter-batch-readonly-parallel-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    let state_file = temp.path().join("fake_state");
    let calls_file = temp.path().join("calls.jsonl");
    write_batch_fake_mcp_server(&fake_server, &state_file)?;
    write_codex_config(temp.path(), &fake_server)?;
    fs::write(
        &calls_file,
        concat!(
            "{\"tool\":\"confluence_search\",\"arguments\":{\"query\":\"one\"}}\n",
            "{\"tool\":\"confluence_search\",\"arguments\":{\"query\":\"two\"}}\n",
        ),
    )
    .context("write calls jsonl")?;

    let output = run_cxporter(
        temp.path(),
        &[
            "batch",
            "--server",
            "fake",
            "--input",
            calls_file.to_str().context("calls path utf8")?,
            "--concurrency",
            "2",
        ],
        None,
    )?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let lines = parse_jsonl(&output.stdout)?;
    assert_eq!(lines.len(), 2);
    assert!(lines.iter().all(|line| line["success"] == true));

    Ok(())
}

#[test]
fn call_retries_transient_tool_call_errors_when_requested() -> Result<()> {
    let temp = TempDir::new("cxporter-retry-e2e")?;
    let fake_server = temp.path().join("fake_mcp_server.sh");
    let state_file = temp.path().join("fake_state");
    write_batch_fake_mcp_server(&fake_server, &state_file)?;
    write_codex_config(temp.path(), &fake_server)?;

    let output = run_cxporter(
        temp.path(),
        &[
            "call",
            "--retry",
            "1",
            "--retry-delay-ms",
            "0",
            "fake",
            "flaky",
            "{}",
        ],
        None,
    )?;
    assert!(
        output.status.success(),
        "cxporter failed: {}",
        output.stderr
    );
    let value: Value = serde_json::from_str(&output.stdout).context("parse retry call JSON")?;
    assert_eq!(value["content"][0]["text"], "flaky ok");

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

struct CommandOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_cxporter(codex_home: &Path, args: &[&str], stdin: Option<&str>) -> Result<CommandOutput> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cxporter"));
    command
        .args(args)
        .env("CODEX_HOME", codex_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().context("spawn cxporter")?;
    if let Some(stdin_text) = stdin {
        let mut child_stdin = child.stdin.take().context("capture cxporter stdin")?;
        child_stdin
            .write_all(stdin_text.as_bytes())
            .context("write cxporter stdin")?;
    }
    let output = child.wait_with_output().context("wait for cxporter")?;
    Ok(CommandOutput {
        status: output.status,
        stdout: String::from_utf8(output.stdout).context("decode stdout")?,
        stderr: String::from_utf8(output.stderr).context("decode stderr")?,
    })
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

fn write_plugin_codex_config(codex_home: &Path, plugin_config_name: &str) -> Result<()> {
    let config = format!(
        r#"approval_policy = "never"

[plugins."{}"]
enabled = true
"#,
        escape_toml_string(plugin_config_name),
    );
    fs::write(codex_home.join("config.toml"), config).context("write Codex config.toml")
}

fn write_shadow_codex_config(
    codex_home: &Path,
    plugin_config_name: &str,
    server_name: &str,
    user_server: &Path,
) -> Result<()> {
    let config = format!(
        r#"approval_policy = "never"

[mcp_servers.{}]
command = "{}"
startup_timeout_sec = 5.0
tool_timeout_sec = 5.0

[plugins."{}"]
enabled = true
"#,
        server_name,
        escape_toml_string(&user_server.to_string_lossy()),
        escape_toml_string(plugin_config_name),
    );
    fs::write(codex_home.join("config.toml"), config).context("write Codex config.toml")
}

fn write_plugin_fixture(
    codex_home: &Path,
    marketplace: &str,
    plugin_name: &str,
    version: &str,
    server_name: &str,
    command: &Path,
    cwd: Option<&str>,
) -> Result<PathBuf> {
    let plugin_root = codex_home
        .join("plugins/cache")
        .join(marketplace)
        .join(plugin_name)
        .join(version);
    fs::create_dir_all(plugin_root.join(".codex-plugin"))
        .with_context(|| format!("create plugin root {}", plugin_root.display()))?;
    fs::write(
        plugin_root.join(".codex-plugin/plugin.json"),
        serde_json::to_string(&json!({
            "name": plugin_name,
            "version": version,
            "mcpServers": "./.mcp.json"
        }))?,
    )
    .context("write plugin manifest")?;

    let mut server = json!({
        "command": command.to_string_lossy(),
        "args": [],
        "startup_timeout_sec": 5.0,
        "tool_timeout_sec": 5.0
    });
    if let Some(cwd) = cwd {
        server["cwd"] = Value::String(cwd.to_string());
    }
    let mut servers = serde_json::Map::new();
    servers.insert(server_name.to_string(), server);
    fs::write(
        plugin_root.join(".mcp.json"),
        serde_json::to_string(&json!({
            "mcpServers": Value::Object(servers)
        }))?,
    )
    .context("write plugin MCP config")?;

    Ok(plugin_root)
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

fn write_named_fake_mcp_server(path: &Path, tool_name: &str) -> Result<()> {
    fs::write(
        path,
        format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*|*'"method": "initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake-mcp","version":"0.0.0"}}}}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*|*'"method": "tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"{tool_name}","description":"Fixture tool","inputSchema":{{"type":"object","properties":{{}}}}}}]}}}}\n' "$id"
      ;;
  esac
done
"#,
            tool_name = tool_name
        ),
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

fn write_batch_fake_mcp_server(path: &Path, state_file: &Path) -> Result<()> {
    fs::write(
        path,
        format!(
            r#"#!/bin/sh
state_file={state_file}
while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*|*'"method": "initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":"2025-06-18","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"fake-mcp","version":"0.0.0"}}}}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*|*'"method": "tools/list"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"echo","description":"Echo input","inputSchema":{{"type":"object","properties":{{"message":{{"type":"string"}}}},"required":["message"]}}}},{{"name":"confluence_search","description":"Search Confluence","inputSchema":{{"type":"object","properties":{{"query":{{"type":"string"}}}}}},"annotations":{{"readOnlyHint":true}}}},{{"name":"error_tool","description":"Return isError","inputSchema":{{"type":"object","properties":{{}}}}}},{{"name":"flaky","description":"Fails once","inputSchema":{{"type":"object","properties":{{}}}}}}]}}}}\n' "$id"
      ;;
    *'"method":"tools/call"'*|*'"method": "tools/call"'*)
      case "$line" in
        *'"name":"flaky"'*|*'"name": "flaky"'*)
          if [ ! -f "$state_file" ]; then
            printf failed > "$state_file"
            printf '{{"jsonrpc":"2.0","id":%s,"error":{{"code":-32000,"message":"temporary failure"}}}}\n' "$id"
          else
            printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"flaky ok"}}],"isError":false}}}}\n' "$id"
          fi
          ;;
        *'"name":"error_tool"'*|*'"name": "error_tool"'*)
          printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"tool error"}}],"isError":true}}}}\n' "$id"
          ;;
        *)
          printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"echo ok"}}],"isError":false}}}}\n' "$id"
          ;;
      esac
      ;;
  esac
done
"#,
            state_file = shell_quote(&state_file.to_string_lossy()),
        ),
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

fn parse_jsonl(output: &str) -> Result<Vec<Value>> {
    output
        .lines()
        .map(|line| serde_json::from_str(line).with_context(|| format!("parse JSONL: {line}")))
        .collect()
}

fn escape_toml_string(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
