# cxporter

`cxporter` is a Rust CLI for direct access to Codex-authenticated MCP servers.

It links only to the Codex Rust crates needed for config loading, login state,
and MCP transport. The Codex source tree is not vendored; Cargo resolves the
pinned git dependencies. Codex still owns all authentication, including
ChatGPT/Codex auth, configured MCP OAuth tokens, Agent Identity request signing,
and internal `codex_apps` connector access. `cxporter` does not implement auth
flows, and it does not create Codex LLM threads for tool calls. The only command
that reads and prints token/header values is the explicit, opt-in
`auth export --reveal` path described below.

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

## Dependency hygiene

Use `cargo-shear` to inspect unused or misplaced Cargo dependencies:

```bash
cargo install cargo-shear --version 1.12.4 --locked
cargo shear --locked
```

Review findings before running `cargo shear --fix`. CI runs the same check with
`--deny-warnings`.

## Usage

```bash
cargo run -- list
cargo run -- list --server codex_apps
cargo run -- list --server codex_apps --connector atlassian --name-contains confluence
cargo run -- apps
cargo run -- schema codex_apps github_fetch_pr
cargo run -- call codex_apps <tool_name> '{"query":"hello"}'
cargo run -- call codex_apps <tool_name> --args-file args.json
cargo run -- batch --server codex_apps --input calls.jsonl --concurrency 1 --retry 3
cargo run -- auth inspect datadog --format json
cargo run -- auth export datadog --format json --reveal
cargo run -- auth export datadog --format curl --reveal
cargo run -- serve --server codex_apps
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
  `codex_apps` when Codex enables it for the current auth. Use `--connector`,
  `--tool`, and `--name-contains` to keep large connector surfaces readable.
  JSON output includes raw tool definitions plus `toolAliases`; pass
  `--format text` for a compact table.
- `call`: calls a tool directly through Codex's MCP connection manager.
  `call` checks required schema properties locally before sending; pass
  `--no-preflight` to bypass that check and send the raw JSON. Pass JSON as
  the positional argument, `--args-file path`, or `--args-file -` for stdin.
  `--retry` and `--retry-delay-ms` opt in to retries for transient
  transport/startup/tool-call errors.
- `batch`: reads JSONL tool calls and writes one JSONL result per input line.
  Each line is `{ "tool": "...", "arguments": {...} }`. Processing continues
  after per-line failures, and the command exits non-zero if any line fails.
- `schema`: prints the JSON input schema for one tool.
- `resource`: reads an MCP resource directly through Codex's MCP connection
  manager.
- `apps`: lists accessible Codex apps/connectors discovered from `codex_apps`
  tools.
- `auth inspect`: inspects the auth source for one HTTP MCP server without
  revealing secret values. It reports metadata such as `source`, `url`, header
  names, env var names, OAuth expiry, and OAuth scopes when available.
- `auth export`: exports HTTP auth headers for one MCP server. This command
  refuses to run without `--reveal` because it prints secret header values.
  Supported formats are `json`, `env`, and `curl`.
- `serve`: runs a stdio MCP server that exports the currently available
  Codex-authenticated MCP tools as its own tools.

Direct `call` and `schema` accept both the raw downstream tool name and the
cxporter-exported alias. `serve` loads the downstream tools once at startup and
exposes each tool with a namespaced MCP tool name. Internal apps/connectors are
grouped under the `codex_apps` server name, so raw `github_fetch_pr` is exported
as `codex_apps.github.fetch_pr`. Registered MCP servers are exported as
`<server>.<tool>`. Use `list --server <server> --name-contains <text>` to see
the raw/exported mapping, and use `--server` one or more times to limit the
served surface:

```bash
cxporter serve --server codex_apps
```

Register it as an MCP server from another client with stdio transport:

```toml
[mcp_servers.cxporter]
command = "cxporter"
args = ["serve", "--server", "codex_apps"]
```

For Claude Code, add it as a local stdio MCP server:

```bash
claude mcp add --transport stdio cxporter -- cxporter serve --server codex_apps
```

If `cxporter` is not on Claude Code's `PATH`, use the absolute path:

```bash
claude mcp add --transport stdio cxporter -- "$(which cxporter)" serve --server codex_apps
```

Verify it from Claude Code with:

```bash
claude mcp get cxporter
```

For example, `github_fetch_pr` currently expects `repo_full_name` and
`pr_number`:

```bash
cargo run -- call codex_apps github_fetch_pr '{"repo_full_name":"openai/codex","pr_number":123}'
```

The same call can read arguments from a file:

```bash
cargo run -- call codex_apps github_fetch_pr --args-file args.json
```

Batch input is newline-delimited JSON:

```jsonl
{"tool":"codex_apps.github.fetch_pr","arguments":{"repo_full_name":"openai/codex","pr_number":123}}
{"tool":"github_fetch_pr","arguments":{"repo_full_name":"openai/codex","pr_number":124}}
```

Run it with bounded concurrency and opt-in retries. `batch` refuses
`--concurrency > 1` when any selected tool does not advertise parallel call
support. Keep write-heavy connector batches at `--concurrency 1`, or use
`--force-parallel` only when duplicate or overlapping side effects are
acceptable.

```bash
cargo run -- batch --server codex_apps --input calls.jsonl --concurrency 1 --retry 3 --retry-delay-ms 500
```

## Auth inspection and export

`auth inspect` and `auth export` are for debugging the exact HTTP auth headers
Codex would attach to a configured MCP server. They support streamable HTTP MCP
servers only. Stdio MCP servers are reported as unsupported.

`inspect` never reveals token/header values:

```bash
cxporter auth inspect datadog --format json
```

`export` prints secret values only with `--reveal`:

```bash
cxporter auth export datadog --format json --reveal
cxporter auth export datadog --format env --reveal
cxporter auth export datadog --format curl --reveal
```

Supported auth sources:

- `bearer_token_env_var`: `inspect` reports the env var name; `export` resolves
  it and emits `Authorization: Bearer <value>`.
- `http_headers`: static config headers. `inspect` masks secret-like header
  names such as `Authorization`, `Cookie`, `X-Api-Key`, `*-Token`, and
  `*-Secret`.
- `env_http_headers`: `inspect` reports header-to-env-var mapping; `export`
  resolves current env var values into headers.
- `mcp_oauth`: reads Codex MCP OAuth stored access tokens from the configured
  `mcp_oauth_credentials_store` mode (`auto`, `file`, or `keyring`) and emits
  an `Authorization` header. Refresh tokens are never output.
- `codex_runtime`: `codex_apps` auth is injected by the Codex runtime and is
  not a provider API token. `inspect` reports a warning; `export` is
  unsupported.

OAuth note: the upstream Codex OAuth token loader is not public at the pinned
revision, so `cxporter` mirrors only the minimal store key and access-token
fields needed for inspection/export. This intentionally avoids refresh-token
handling and does not implement OAuth login or refresh flows. Whether an MCP
OAuth access token works against a provider's normal API is provider-specific.
