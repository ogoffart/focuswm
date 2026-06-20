#!/bin/bash
# SessionStart hook: install the system libraries focuswm needs to *link* and
# run, so `cargo build`/`cargo test`/`clippy` work in Claude Code on the web.
#
# The base image ships the runtime .so files but not the `-dev` packages, so
# linking fails on e.g. `-lxkbcommon` / `-lwayland-server` / `-lEGL` until these
# are installed (see scripts/install-build-deps.sh for the package list).
set -euo pipefail

# Only needed in the remote (web) container; local machines have their own setup.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

exec "${CLAUDE_PROJECT_DIR:-.}/scripts/install-build-deps.sh"
