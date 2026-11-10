#!/usr/bin/env bash
# ai-verify.sh — the repo's own verification script (AIOS convention)
#
# Why this exists: the agent's headless Bash allowlist is a prefix
# match, so a flagged form like `curl -s -X POST http://...` won't hit
# `Bash(curl http://localhost:*)` — rather than enumerating every curl
# variant, verification capability is folded into this one script: the
# allowlist only permits `bash scripts/ai-verify.sh`, and inside the
# script you're free to curl, run playwright, hit a local service,
# whatever you need.
#
# Contract integration: CONTRACT §5 (Definition of Done) requires this
# script, when present, to be run and to pass; the agent excerpts its
# output into the receipt's evidence section.
#
# Conventions:
# - every failing check should exit non-zero and print why it failed
#   (the agent uses this to fix it)
# - keep it idempotent and safely rerunnable; no interactive prompts
# - a check that needs a dev server: start it yourself, clean it up
#   yourself (or check whether it's already running)
set -u

# Rust project: headless sessions may only run `bash scripts/ai-verify.sh`
# (see .claude/aios-headless-settings.json), so build + test live here.
#
# The supervisor's headless shell may not have cargo on PATH (T-001 paused
# four times on this). If cargo is missing, try rustup's default location
# $HOME/.cargo/bin; if it is still missing, fail loudly — no other guesses.
if ! command -v cargo >/dev/null 2>&1; then
  echo "ai-verify: cargo not on PATH, trying \$HOME/.cargo/bin ($HOME/.cargo/bin)"
  PATH="$HOME/.cargo/bin:$PATH"
  export PATH
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "ai-verify: cargo not found. Tried PATH=$PATH (which includes $HOME/.cargo/bin)." >&2
  echo "ai-verify: install Rust via rustup or put cargo on the supervisor's PATH." >&2
  exit 1
fi
echo "ai-verify: using cargo at $(command -v cargo)"
echo "ai-verify: cargo build"
cargo build --quiet || { echo "ai-verify: cargo build failed"; exit 1; }
echo "ai-verify: cargo test"
cargo test --quiet || { echo "ai-verify: cargo test failed"; exit 1; }
echo "ai-verify: ok"
exit 0
