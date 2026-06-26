# cxporter

CLI for direct access to Codex-authenticated MCP servers.

`cxporter` links only to the Codex Rust crates needed for config loading, login state, and MCP transport. Codex owns all authentication (ChatGPT/Codex auth, MCP OAuth tokens, Agent Identity signing, `codex_apps` connector access) — `cxporter` does not implement auth flows or create Codex LLM threads.

## Setup

```bash
cargo build
cargo install --path . --force --locked
```

Install the bundled Codex skill to let agents use `cxporter` consistently:

```bash
npx skills add . --skill cxporter-mcp -y
```

## Commands

| Command | Description |
|---------|-------------|
| `list` | List configured MCP servers and tools |
| `call` | Call a tool through Codex's MCP connection manager |
| `batch` | Run multiple tool calls from a JSONL file |
| `schema` | Print the JSON input schema for one tool |
| `resource` | Read an MCP resource through Codex's MCP connection manager |
| `apps` | List accessible Codex apps/connectors from `codex_apps` tools |
| `auth inspect` | Inspect auth metadata for an HTTP MCP server (no secrets revealed) |
| `auth export` | Export HTTP auth headers for an MCP server (requires `--reveal`) |
| `serve` | Run a stdio MCP server that re-exports Codex-authenticated tools |

### list

```bash
cxporter list
cxporter list --server codex_apps
cxporter list --server codex_apps --connector atlassian --name-contains confluence
```

Includes synthesized `codex_apps` when Codex enables it for the current auth. Use `--connector`, `--tool`, and `--name-contains` to filter large connector surfaces. JSON output includes raw tool definitions plus `toolAliases`; pass `--format text` for a compact table.

### call

```bash
cxporter call codex_apps github_fetch_pr '{"repo_full_name":"openai/codex","pr_number":123}'
cxporter call codex_apps github_fetch_pr --args-file args.json
cxporter call codex_apps github_fetch_pr --args-file -   # stdin
```

Checks required schema properties locally before sending. Pass `--no-preflight` to bypass and send raw JSON. Use `--retry` and `--retry-delay-ms` for transient transport/startup/tool-call errors. Accepts both raw tool names and cxporter-exported aliases.

### batch

```bash
cxporter batch --server codex_apps --input calls.jsonl --concurrency 1 --retry 3 --retry-delay-ms 500
```

Reads JSONL tool calls and writes one JSONL result per input line:

```jsonl
{"tool":"codex_apps.github.fetch_pr","arguments":{"repo_full_name":"openai/codex","pr_number":123}}
{"tool":"github_fetch_pr","arguments":{"repo_full_name":"openai/codex","pr_number":124}}
```

Processing continues after per-line failures; exits non-zero if any line fails. `--concurrency > 1` is refused when any selected tool does not advertise parallel call support. Keep write-heavy batches at `--concurrency 1`, or use `--force-parallel` only when duplicate side effects are acceptable.

### schema

```bash
cxporter schema codex_apps github_fetch_pr
```

### auth inspect / auth export

```bash
cxporter auth inspect datadog --format json

cxporter auth export datadog --format json --reveal
cxporter auth export datadog --format env  --reveal
cxporter auth export datadog --format curl --reveal
```

Both commands support streamable HTTP MCP servers only; stdio servers are reported as unsupported.

`inspect` never reveals token/header values. `export` refuses to run without `--reveal` because it prints secret header values.

Supported auth sources:

| Source | `inspect` | `export` |
|--------|-----------|----------|
| `bearer_token_env_var` | Reports env var name | Resolves env var → `Authorization: Bearer <value>` |
| `http_headers` | Masks `Authorization`, `Cookie`, `X-Api-Key`, `*-Token`, `*-Secret` | Emits static headers |
| `env_http_headers` | Reports header-to-env-var mapping | Resolves current env var values |
| `mcp_oauth` | Reports OAuth metadata and expiry | Emits `Authorization` header from stored access token |
| `codex_runtime` | Reports warning | Unsupported |

OAuth note: `cxporter` mirrors only the minimal store key and access-token fields needed for inspection/export. Refresh tokens are never output. If the stored access token is expired, a warning is added (`inspect`/`export` both warn; the provider will reject it with `401`).

### serve

```bash
cxporter serve --server codex_apps
```

Runs a stdio MCP server that re-exports Codex-authenticated tools with namespaced names. Internal `codex_apps` tools are namespaced as `codex_apps.<connector>.<tool>` (e.g. `github_fetch_pr` → `codex_apps.github.fetch_pr`). Registered MCP servers are exported as `<server>.<tool>`. Tools are loaded once at startup.

Use `list --server <server> --name-contains <text>` to inspect the raw/exported name mapping.

**Register as an MCP server (generic TOML):**

```toml
[mcp_servers.cxporter]
command = "cxporter"
args = ["serve", "--server", "codex_apps"]
```

**Register in Claude Code:**

```bash
claude mcp add --transport stdio cxporter -- cxporter serve --server codex_apps
# If cxporter is not on Claude Code's PATH:
claude mcp add --transport stdio cxporter -- "$(which cxporter)" serve --server codex_apps

claude mcp get cxporter   # verify
```

## Configuration

Pass Codex config overrides with `--config`:

```bash
cxporter --config features.apps=true list
```

For local MCP environments, `cxporter` resolves the Codex runtime executable path from (in order):

1. `CXPORTER_CODEX_SELF_EXE` env var
2. `codex` on `PATH`
3. The `cxporter` binary itself

Set `CXPORTER_CODEX_SELF_EXE` only when a Codex environment needs to launch Codex hidden helper modes.

## Dependency hygiene

Use `cargo-shear` to inspect unused or misplaced Cargo dependencies:

```bash
cargo install cargo-shear --version 1.12.4 --locked
cargo shear --locked
```

Review findings before running `cargo shear --fix`. CI runs the same check with `--deny-warnings`.

The `codex-*` crates are pinned to a thin fork of `openai/codex` (a one-line workspace version bump) rather than upstream directly. See [docs/codex-fork-pin.md](docs/codex-fork-pin.md) for why this is needed and how to advance the pin.

> **Store-mode note:** `codex-core` coerces `keyring`/`auto` to `file` whenever `CARGO_PKG_VERSION` is the local-dev sentinel `0.0.0` (what a plain git dependency reports). The fork pin sets a non-zero version, keeping `keyring`/`auto` usable (e.g. tokens stored in the macOS Keychain).
