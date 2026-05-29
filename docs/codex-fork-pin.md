# Codex fork pin

`cxporter` depends on several `codex-*` crates from `openai/codex`. Instead of
pinning them directly to an upstream revision, we pin them to a **thin fork**
(`mkusaka/codex`) whose only change is a single line in the workspace
`Cargo.toml`.

## Why a fork is required

`codex-core` decides where to read MCP OAuth credentials from in
`resolve_mcp_oauth_credentials_store_mode()` (`codex-rs/core/src/config/mod.rs`):

```rust
match (package_version, configured) {
    (LOCAL_DEV_BUILD_VERSION, Keyring | Auto) => File,
    (_, mode) => mode,
}
```

`LOCAL_DEV_BUILD_VERSION` is `"0.0.0"`, and `package_version` is
`env!("CARGO_PKG_VERSION")` — the version baked into the `codex-core` crate at
compile time. The `openai/codex` workspace keeps `[workspace.package] version`
at `"0.0.0"` and only bumps it in release CI, so **any plain git dependency
reports `0.0.0`**. That makes `codex-core` silently coerce `keyring`/`auto`
OAuth store modes to `file`, which means `cxporter auth inspect`/`export` can
never read tokens stored in the OS keychain (e.g. a Datadog MCP token on the
macOS Keychain).

Things that do **not** work as an alternative (verified):

- `.cargo/config.toml` `[env]` with `CARGO_PKG_VERSION = { force = true }` — Cargo
  sets the per-crate `CARGO_PKG_VERSION` itself and ignores the override; the
  crate still sees `0.0.0`.
- Vendoring `codex-core` alone — it uses `version.workspace = true` and
  `workspace = true` dependencies, so it cannot be built outside the full
  ~48 MB workspace.
- A local-path `[patch]` — CI does a fresh `cargo build --locked` with no local
  checkout, so a machine-local path breaks CI.

The remaining viable option is a fork that holds a one-line version bump, which
is what we pin to.

## What the fork contains

A single branch (currently `cxporter-pin-<short-rev>`) based on the upstream
revision we want, with exactly one commit changing:

```toml
# codex-rs/Cargo.toml
[workspace.package]
-version = "0.0.0"
+version = "0.137.0-dev+cxporter.<short-rev>"
```

Any non-`"0.0.0"` value works; the build metadata (`+cxporter.<short-rev>`)
documents which upstream revision the branch is based on. With a non-zero
version, `codex-core` stops coercing `keyring`/`auto` to `file`, and
`cxporter`'s own keyring/file/auto loader (`load_mcp_oauth_access_token`) works
as documented.

## How to advance the pin

When you want a newer `openai/codex`:

```bash
# In a clone of openai/codex
git fetch origin
NEW_REV=$(git rev-parse origin/main)            # or a chosen upstream rev
SHORT=$(git rev-parse --short "$NEW_REV")

# Work in a throwaway worktree so your main checkout is untouched
git worktree add -b "cxporter-pin-$SHORT" /tmp/codex-verbump "$NEW_REV"

# Bump the workspace version away from 0.0.0 (pick a current-looking value)
#   codex-rs/Cargo.toml -> [workspace.package] version = "X.Y.Z-dev+cxporter.$SHORT"

cd /tmp/codex-verbump
git commit --no-verify -am "chore: set workspace version for cxporter dependency pin"
git push -f <your-fork-remote> "cxporter-pin-$SHORT"
FORK_REV=$(git rev-parse HEAD)

# Clean up
cd -            # back to the codex clone
git worktree remove /tmp/codex-verbump --force
git branch -D "cxporter-pin-$SHORT"
```

Then in this repo:

1. Point all `codex-*` dependencies in `Cargo.toml` at `$FORK_REV` and update
   the comment above them (base rev + branch name).
2. `cargo update -p codex-core`.
3. `cargo build --locked` and fix any upstream API breakage. Advancing the pin
   is a real dependency upgrade — for example, the jump to `740d942` required
   migrating `cxporter` from `rmcp` 0.15 to 1.7 and adding the
   `prefix_mcp_tool_names` argument to `McpConnectionManager::new`.
4. Run the full gate: `cargo test --locked`, `cargo fmt --all -- --check`,
   `cargo clippy --locked --all-targets --all-features -- -D warnings`,
   `cargo shear --locked`.
5. Sanity-check the actual fix end to end:

   ```bash
   # config.toml with: mcp_oauth_credentials_store = "keyring"
   CODEX_HOME=/path/to/cfg cxporter auth inspect <server> --format json
   # storeMode must be "keyring" (not coerced to "file")
   ```

## Removing the fork

If `openai/codex` ever stops coercing store modes for `0.0.0` builds (or exposes
the configured value before coercion), this fork can be dropped and the
dependencies pointed back at `openai/codex` directly. The regression test
`auth_inspect_does_not_coerce_auto_store_mode_to_file` in `tests/serve_e2e.rs`
guards against silently losing the behavior.
