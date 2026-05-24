#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${CXPORTER_BIN:-}" ]]; then
  exec "$CXPORTER_BIN" "$@"
fi

if command -v cxporter >/dev/null 2>&1; then
  exec cxporter "$@"
fi

cxporter_dir="${CXPORTER_DIR:-}"

if [[ -z "$cxporter_dir" ]]; then
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  candidate="$(cd "$script_dir/../../.." && pwd)"
  if [[ -f "$candidate/Cargo.toml" ]] && grep -q '^name = "cxporter"' "$candidate/Cargo.toml"; then
    cxporter_dir="$candidate"
  fi
fi

if [[ ! -f "$cxporter_dir/Cargo.toml" ]]; then
  echo "cxporter: set CXPORTER_BIN or CXPORTER_DIR, or install cxporter on PATH" >&2
  exit 127
fi

exec cargo run --quiet --manifest-path "$cxporter_dir/Cargo.toml" -- "$@"
