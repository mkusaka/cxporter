# cxporter

`cxporter` is a Rust CLI for direct access to Codex-authenticated MCP servers.

It links only to the Codex Rust crates needed for config loading, login state,
and MCP transport. The Codex source tree is not vendored; Cargo resolves the
pinned git dependencies. Codex still owns all authentication, including
ChatGPT/Codex auth, configured MCP OAuth tokens, Agent Identity request signing,
and internal `codex_apps` connector access. `cxporter` does not read tokens or
implement auth flows, and it does not create Codex LLM threads for tool calls.

## Setup

```bash
cargo build
cargo install --path . --force --locked
```

Install the bundled Codex skill when you want agents to use `cxporter`
consistently:

```bash
npx skills add . --skill cxporter-mcp -y
```

## Usage

```bash
cargo run -- list
cargo run -- list --server codex_apps
cargo run -- apps
cargo run -- schema codex_apps github_fetch_pr
cargo run -- call codex_apps <tool_name> '{"query":"hello"}'
```

Pass Codex config overrides through to Codex config loading with `--config`:

```bash
cargo run -- --config features.apps=true list
```

For local MCP environments, `cxporter` fills Codex's runtime executable path from
`CXPORTER_CODEX_SELF_EXE`, then `codex` on `PATH`, then the `cxporter` binary
itself. Set `CXPORTER_CODEX_SELF_EXE` only when a Codex environment needs to
launch Codex hidden helper modes.

## Commands

- `list`: lists configured MCP servers and tools, including synthesized
  `codex_apps` when Codex enables it for the current auth.
- `call`: calls a tool directly through Codex's MCP connection manager.
  `call` checks required schema properties locally before sending; pass
  `--no-preflight` to bypass that check and send the raw JSON.
- `schema`: prints the JSON input schema for one tool.
- `resource`: reads an MCP resource directly through Codex's MCP connection
  manager.
- `apps`: lists accessible Codex apps/connectors discovered from `codex_apps`
  tools.

For example, `github_fetch_pr` currently expects `repo_full_name` and
`pr_number`:

```bash
cargo run -- call codex_apps github_fetch_pr '{"repo_full_name":"openai/codex","pr_number":123}'
```
