---
name: cxporter-mcp
description: Use when Codex needs to list, inspect, or directly call MCP servers through the local cxporter CLI using the user's existing Codex sign-in/auth. Covers registered MCP servers, internal codex_apps connectors/apps, tool schemas, resources, and direct tool calls without implementing OAuth, token handling, or Codex LLM threads.
---

# cxporter MCP

## Overview

Use `cxporter` to operate MCP servers through Codex-owned config and auth. Treat
`cxporter` as a direct MCP client: it loads Codex config/auth, lists servers and
tools, shows schemas, and calls tools without creating a Codex LLM thread.
It can also run as a stdio MCP server that re-exports those downstream tools
with stable namespace-style names.

## Invocation

Prefer the bundled wrapper so the skill works from arbitrary working
directories:

```bash
skills/cxporter-mcp/scripts/cxporter.sh <cxporter-args>
```

If the skill is installed elsewhere, use the installed skill's
`scripts/cxporter.sh`. The wrapper resolves cxporter in this order:

1. `CXPORTER_BIN`
2. `cxporter` on `PATH`
3. `cargo run --manifest-path "$CXPORTER_DIR/Cargo.toml"`

Set `CXPORTER_DIR` when the repository lives outside the installed skill's
source checkout, or run `cargo install --path . --force --locked` so `cxporter`
is available on `PATH`.

## Core Workflow

1. List available MCP servers or apps before assuming a target exists.
2. Inspect the tool schema before calling a tool unless the exact schema is
   already known from the current session.
3. Call the tool with a JSON object matching that schema.
4. Treat local preflight errors as argument-shape errors to fix before calling
   the connector.
5. Interpret connector errors as remote/auth/schema errors, not as proof that
   cxporter failed to connect.

## Commands

List registered MCP servers and synthesized `codex_apps`:

```bash
skills/cxporter-mcp/scripts/cxporter.sh list
```

List only the internal apps/connectors exposed by `codex_apps`:

```bash
skills/cxporter-mcp/scripts/cxporter.sh apps
```

Inspect one server's tools:

```bash
skills/cxporter-mcp/scripts/cxporter.sh list --server codex_apps
```

Show one tool's input schema:

```bash
skills/cxporter-mcp/scripts/cxporter.sh schema codex_apps github_fetch_pr
```

Call a tool:

```bash
skills/cxporter-mcp/scripts/cxporter.sh call codex_apps github_fetch_pr '{"repo_full_name":"openai/codex","pr_number":123}'
```

Bypass local required-property preflight only when the remote connector must see
the raw JSON:

```bash
skills/cxporter-mcp/scripts/cxporter.sh call --no-preflight codex_apps github_fetch_pr '{"repo_full_name":"openai/codex","pr_number":123}'
```

Quote tool names that contain spaces:

```bash
skills/cxporter-mcp/scripts/cxporter.sh schema codex_apps 'google drive_search'
```

Read a resource:

```bash
skills/cxporter-mcp/scripts/cxporter.sh resource <server> <uri>
```

Run as an MCP server:

```bash
skills/cxporter-mcp/scripts/cxporter.sh serve --server codex_apps
```

When serving, cxporter loads downstream tools at startup and exposes them as MCP
tools named `<server>.<connector>.<tool>` for apps, or `<server>.<tool>` for
ordinary registered MCP servers. For example, raw `codex_apps` tool
`github_fetch_pr` is exported as `codex_apps.github.fetch_pr`.

Register an installed cxporter binary in another MCP client with stdio
transport:

```toml
[mcp_servers.cxporter]
command = "cxporter"
args = ["serve", "--server", "codex_apps"]
```

## Error Handling

- `INVALID_ARGUMENT`: the JSON arguments do not match the connector schema.
  Run `schema <server> <tool>` and retry with the required property names.
- `missing required properties`: cxporter rejected the arguments before calling
  the connector. Use the listed known properties or run `schema`.
- `notLoggedIn`: the user is not signed in through Codex for that MCP path.
  Ask them to sign in with Codex; do not implement auth or ask for tokens.
- `NOT_FOUND`, permission, or workspace policy messages: the connector was
  reached, but the remote service or workspace policy rejected the request.
- `unknown MCP server`: list servers and verify the server is registered or
  that `codex_apps` is enabled for the current Codex auth/config.

## Guardrails

- Do not read, print, transform, or store Codex tokens.
- Do not implement OAuth, bearer-token plumbing, or custom connector auth.
- Confirm before using mutating tools such as send, create, update, delete,
  merge, transition, archive, or label operations.
- Prefer `schema` over guessing argument names. Connector schemas can differ
  from public API names; for example `github_fetch_pr` expects
  `repo_full_name` and `pr_number`.
