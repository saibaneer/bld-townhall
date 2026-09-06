#!/usr/bin/env bash
# ============================================================================
# M12 live demo (ADR-033): drive a real booking from Telegram.
#
#   scripts/telegram-demo.sh --lucy-chat 5741534028
#
# Needs TELEGRAM_BOT_TOKEN in the environment (this script sources ./.env if
# present). It boots the mock council + the townhall-server, binds `lucy` to the
# given Telegram chat (the store write the approve-first flow needs), then runs
# the telegram-runner. Text the bot and watch the boundary drive the booking.
#
# Everything lives under a temp work dir and is torn down on exit.
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")/.."

# --- config ----------------------------------------------------------------
# The same fixture keys the authority-lane test uses (mock council + server).
COUNCIL_KEY="0707070707070707070707070707070707070707070707070707070707070707"
AUTHORITY_KEY="a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7"
COUNCIL_PORT="${COUNCIL_PORT:-8091}"
SERVER_PORT="${SERVER_PORT:-8090}"

LUCY_CHAT=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --lucy-chat) LUCY_CHAT="${2:-}"; shift ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

[ -f .env ] && { set -a; . ./.env; set +a; }
[ -z "$LUCY_CHAT" ] && { echo "usage: $0 --lucy-chat CHAT_ID" >&2; exit 2; }
[ -z "${TELEGRAM_BOT_TOKEN:-}" ] && { echo "TELEGRAM_BOT_TOKEN is not set (source .env)" >&2; exit 2; }

WORK="$(mktemp -d)"
SERVER_URL="http://127.0.0.1:${SERVER_PORT}"
COUNCIL_URL="http://127.0.0.1:${COUNCIL_PORT}"
echo "==> work dir: $WORK"

echo "==> building binaries"
cargo build -q -p mock-council -p townhall-server -p bind-channel -p telegram-runner

# Bind FIRST: bind-channel creates + migrates the DB and writes the binding, so
# the server then opens an already-prepared DB — no concurrent SQLite writer.
echo "==> binding lucy -> tg:${LUCY_CHAT}"
./target/debug/bind-channel --db "$WORK/townhall.sqlite" --address "tg:${LUCY_CHAT}" --principal lucy

echo "==> starting mock council on :${COUNCIL_PORT}"
./target/debug/mock-council --db "$WORK/council.sqlite" --key-hex "$COUNCIL_KEY" --port "$COUNCIL_PORT" \
  >"$WORK/council.log" 2>&1 &
COUNCIL_PID=$!

echo "==> starting townhall-server on :${SERVER_PORT}"
./target/debug/townhall-server \
  --db "$WORK/townhall.sqlite" --denials-db "$WORK/denials.sqlite" \
  --council-url "$COUNCIL_URL" --key-hex "$COUNCIL_KEY" --authority-key "$AUTHORITY_KEY" \
  --port "$SERVER_PORT" --reconcile-interval-ms 50 \
  >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

RUNNER_PID=""
cleanup() { kill "$COUNCIL_PID" "$SERVER_PID" ${RUNNER_PID:+"$RUNNER_PID"} 2>/dev/null || true; }
trap cleanup EXIT INT TERM

echo "==> waiting for council + server"
for _ in $(seq 1 40); do curl -s -o /dev/null "$COUNCIL_URL/" && break; sleep 0.25; done
for _ in $(seq 1 40); do curl -s -o /dev/null "$SERVER_URL/" && break; sleep 0.25; done

echo "============================================================"
echo " READY. Text @Bldtest_bot from your phone, e.g.:"
echo "   BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000"
echo " then reply:  YES <code>   (the code is in the preview it sends back)"
echo " Ctrl-C to stop. Logs: $WORK/{server,council}.log"
echo "============================================================"

TELEGRAM_BOT_TOKEN="$TELEGRAM_BOT_TOKEN" \
  ./target/debug/telegram-runner --server "$SERVER_URL" --lucy-chat "$LUCY_CHAT" \
  --stop-file "$WORK/stop.list" --continuation-file "$WORK/continuation.jsonl" &
RUNNER_PID=$!
wait "$RUNNER_PID"
