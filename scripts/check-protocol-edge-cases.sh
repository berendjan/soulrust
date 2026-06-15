#!/usr/bin/env bash
#
# check-protocol-edge-cases.sh
#
# Treats Nicotine+ as the reference implementation and de-facto spec of the
# Soulseek protocol (it interoperates with the live network; our codec is new
# and unverified). For each of our protocol modules it asks `claude` (headless)
# to read the corresponding Nicotine+ reference, compare our wire behaviour to
# it, and:
#   - FIX our code where the bytes diverge from Nicotine+ (ours is assumed
#     wrong), pinning the corrected behaviour with a test whose expected values
#     come from the reference;
#   - ADD tests where we're correct but uncovered.
# Finally it runs the whole test suite.
#
# Guardrails (so an autonomous loop can't trade an unknown bug for a
# confidently-wrong "fix"): Claude must anchor every change on what Nicotine+
# actually does, must NOT change deliberate local choices (our error types,
# atomic writes, the subset of messages we implement, the Latin-1 fallback), and
# must print "PROTOCOL CHANGE: <desc>" for each behaviour change. The script
# surfaces those lines and a git diff at the end for human review.
#
# Usage:
#   scripts/check-protocol-edge-cases.sh [KEY ...]   # run all units, or a subset
#
# Environment:
#   NICOTINE_DIR   Path to the nicotine-plus checkout (default: ../nicotine-plus)
#   MODEL          Override the Claude model (default: your configured default)
#   DRY_RUN=1      Print the prompt and the claude command for each unit, run nothing
#   YOLO=1         Use --dangerously-skip-permissions instead of scoped tools
#
# Examples:
#   scripts/check-protocol-edge-cases.sh                # every module
#   scripts/check-protocol-edge-cases.sh wire frame     # just those two
#   DRY_RUN=1 scripts/check-protocol-edge-cases.sh      # preview prompts only

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NICOTINE_DIR="${NICOTINE_DIR:-$(cd "$REPO_ROOT/.." && pwd)/nicotine-plus}"
LOG_DIR="$REPO_ROOT/target/protocol-edge-cases"
DRY_RUN="${DRY_RUN:-0}"
YOLO="${YOLO:-0}"

# Each unit: KEY | our protocol module (relative to repo root) | nicotine
# reference (relative to NICOTINE_DIR) | what to focus the edge-case hunt on.
UNITS=(
  "wire|crates/soulseek-proto/src/wire.rs|pynicotine/slskmessages.py|primitive field pack/unpack: integer widths and little-endian order, the bool/uint8/uint32/uint64 helpers, length-prefixed strings, and the UTF-8 vs Latin-1 charset fallback for non-UTF-8 bytes"
  "frame|crates/soulseek-proto/src/frame.rs|pynicotine/slskproto.py|message framing over a byte stream: the u32 length prefix, accumulating a partial frame across reads, splitting multiple frames from one buffer, zero-length frames, and rejecting absurd/oversized declared lengths before allocating"
  "server|crates/soulseek-proto/src/server.rs|pynicotine/slskmessages.py|client<->server messages we implement (Login request/response, SetWaitPort, GetPeerAddress request/response, FileSearch): exact field layouts, the login success vs failure branches, optional/absent trailing fields, obfuscated-port widths, and unknown server codes"
  "peer|crates/soulseek-proto/src/peer.rs|pynicotine/slskmessages.py|peer-init handshake messages (PeerInit, PierceFirewall): the u8 message code, the P/F/D connection types, the legacy token field, and rejection of unknown codes/connection types"
  "peer_message|crates/soulseek-proto/src/peer_message.rs|pynicotine/slskmessages.py|the browse exchange (GetSharedFileList, SharedFileList): zlib (de)compression of the body, the directory/file/attribute tree layout, the optional trailing private-directories section, empty directories, and guards against decompression bombs and truncated/oversized trees"
)

# ---------------------------------------------------------------------------

die() { echo "error: $*" >&2; exit 1; }

command -v claude >/dev/null 2>&1 || die "claude CLI not found on PATH"
[ -d "$NICOTINE_DIR" ] || die "nicotine-plus checkout not found at $NICOTINE_DIR (set NICOTINE_DIR)"
mkdir -p "$LOG_DIR"

# Permission strategy. By default, auto-accept edits and allow only the
# read/edit tools plus `cargo test`; YOLO=1 bypasses all checks (useful in a
# throwaway sandbox, but it lets Claude run anything).
if [ "$YOLO" = "1" ]; then
  PERM_ARGS=(--dangerously-skip-permissions)
else
  PERM_ARGS=(
    --permission-mode acceptEdits
    --allowedTools Read Grep Glob Edit Write MultiEdit "Bash(cargo test:*)" "Bash(cargo fmt:*)"
  )
fi

MODEL_ARGS=()
[ -n "${MODEL:-}" ] && MODEL_ARGS=(--model "$MODEL")

# Build the prompt for one unit.
build_prompt() {
  local our_file="$1" nicotine_file="$2" focus="$3"
  cat <<EOF
You are auditing the soulrust Soulseek protocol implementation against
Nicotine+, which is the reference implementation and the de-facto spec of this
protocol. Nicotine+ interoperates with the live Soulseek network; our codec is
new and unverified. Treat Nicotine+ as the SOURCE OF TRUTH for on-the-wire
behaviour: where our code and Nicotine+ disagree on how a message is encoded or
decoded, assume OURS is wrong and fix it to match Nicotine+.

Source of truth (read it):  $NICOTINE_DIR/$nicotine_file
Our implementation + tests:  $REPO_ROOT/$our_file

Focus for this module: $focus

Do this:
1. Read our file (impl + tests) and the relevant parts of the Nicotine+
   reference.
2. Enumerate the exact wire details and edge cases Nicotine+ handles for this
   concern — field order, integer widths, endianness, length prefixes, charset,
   empty/optional/absent and legacy fields, compression, and size/bounds limits.
3. For each, compare our behaviour and act:
   - WIRE DIVERGENCE (we produce/accept different bytes, wrong field order or
     width, or mishandle a boundary Nicotine+ handles): FIX our code to match
     Nicotine+, and add a test that pins the now-correct behaviour with expected
     values taken directly from the reference. Print, on its own line,
     "PROTOCOL CHANGE: <what changed and the Nicotine+ behaviour it now matches>".
   - CORRECT BUT UNCOVERED: add a test in the existing style, reusing helpers.
     Do not duplicate existing tests.

Judgment — these are NOT bugs, do not "fix" them:
- Scope: we intentionally implement only a subset of messages. Do NOT add
  message types we don't already implement.
- Local design choices are deliberate: our DecodeError error types, atomic
  config writes, the API shape, and the UTF-8->Latin-1 string fallback. Only
  change behaviour when the actual BYTES on the wire are wrong.

Constraints:
- Anchor every change on what Nicotine+ actually does; cite the reference
  behaviour in the test comment. Do not invent expected values.
- Keep changes minimal and within the soulseek-proto crate. If a fix must touch
  another file in that crate, do it and note it in your summary.
- Before finishing, run \`cargo test -p soulseek-proto\` until it passes.

When done, print a short summary: every PROTOCOL CHANGE line, and the edge-case
tests you added.
EOF
}

run_unit() {
  local key="$1" our_file="$2" nicotine_file="$3" focus="$4"
  local log="$LOG_DIR/$key.log"

  echo "=================================================================="
  echo ">> $key   ($our_file  <=  $nicotine_file)"
  echo "=================================================================="

  [ -f "$REPO_ROOT/$our_file" ] || die "missing our file: $our_file"
  [ -f "$NICOTINE_DIR/$nicotine_file" ] || die "missing reference: $nicotine_file"

  local prompt
  prompt="$(build_prompt "$our_file" "$nicotine_file" "$focus")"

  if [ "$DRY_RUN" = "1" ]; then
    echo "--- prompt ---"
    echo "$prompt"
    echo "--- command ---"
    echo "claude -p <prompt> --add-dir \"$NICOTINE_DIR\" ${PERM_ARGS[*]} ${MODEL_ARGS[*]}"
    return 0
  fi

  # Run Claude headless from the repo root so its cwd is the project.
  ( cd "$REPO_ROOT" && claude -p "$prompt" \
      --add-dir "$NICOTINE_DIR" \
      "${PERM_ARGS[@]}" \
      "${MODEL_ARGS[@]}" ) 2>&1 | tee "$log"
  echo
}

# ---------------------------------------------------------------------------

# Select units: all, or only those whose KEY was passed as an argument.
selected=("$@")
matched=0

for unit in "${UNITS[@]}"; do
  IFS='|' read -r key our_file nicotine_file focus <<<"$unit"
  if [ "${#selected[@]}" -gt 0 ]; then
    skip=1
    for want in "${selected[@]}"; do [ "$want" = "$key" ] && skip=0; done
    [ "$skip" = "1" ] && continue
  fi
  matched=1
  run_unit "$key" "$our_file" "$nicotine_file" "$focus"
done

[ "$matched" = "0" ] && die "no units matched: ${selected[*]} (valid keys: wire frame server peer peer_message)"

if [ "$DRY_RUN" = "1" ]; then
  echo "DRY_RUN: nothing executed."
  exit 0
fi

echo "=================================================================="
echo ">> Running the full test suite"
echo "=================================================================="
cd "$REPO_ROOT"
if cargo test --workspace; then
  echo
  echo "ALL TESTS PASS."
else
  echo
  echo "TESTS FAILED — see output above." >&2
  exit 1
fi

# Surface every wire-behaviour change for human review — green tests alone don't
# prove a change is right when the same pass wrote the tests.
echo
echo "=================================================================="
echo ">> Protocol behaviour changes (review these against Nicotine+)"
echo "=================================================================="
if grep -rhn "PROTOCOL CHANGE:" "$LOG_DIR" 2>/dev/null; then
  echo
  echo "Review the diff below before trusting these changes:"
  git -C "$REPO_ROOT" --no-pager diff --stat -- crates/soulseek-proto/ || true
else
  echo "None reported — only coverage tests were added (or nothing changed)."
fi
