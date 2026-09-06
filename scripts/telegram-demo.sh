#!/usr/bin/env bash
# ============================================================================
# M12 live demo (ADR-033): drive a real booking from Telegram.
#
#   scripts/telegram-demo.sh --lucy-chat 5741534028          # book (no payment)
#   scripts/telegram-demo.sh --lucy-chat 5741534028 --pay    # + Stripe payment
#
# Needs TELEGRAM_BOT_TOKEN in the environment (this script sources ./.env).
# With --pay it also needs a valid STRIPE_SECRET_KEY in ./.env and the Stripe
# CLI logged in (`stripe login`) — `stripe listen` forwards the real webhook so a
# paid checkout advances the booking to Booked.
#
# It boots the mock council + the townhall-server, binds `lucy` to the given
# Telegram chat, then runs the telegram-runner. Everything lives under a temp
# work dir and is torn down on exit.
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")/.."

# The same fixture keys the authority-lane test uses (mock council + server).
COUNCIL_KEY="0707070707070707070707070707070707070707070707070707070707070707"
AUTHORITY_KEY="a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7"
COUNCIL_PORT="${COUNCIL_PORT:-8091}"
SERVER_PORT="${SERVER_PORT:-8090}"

LUCY_CHAT=""
PAY=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --lucy-chat) LUCY_CHAT="${2:-}"; shift ;;
    --pay) PAY=1 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

[ -f .env ] && { set -a; . ./.env; set +a; }
[ -z "$LUCY_CHAT" ] && { echo "usage: $0 --lucy-chat CHAT_ID [--pay]" >&2; exit 2; }
[ -z "${TELEGRAM_BOT_TOKEN:-}" ] && { echo "TELEGRAM_BOT_TOKEN is not set (source .env)" >&2; exit 2; }

WORK="$(mktemp -d)"
SERVER_URL="http://127.0.0.1:${SERVER_PORT}"
COUNCIL_URL="http://127.0.0.1:${COUNCIL_PORT}"
echo "==> work dir: $WORK"

echo "==> building binaries"
cargo build -q -p mock-council -p townhall-server -p bind-channel -p telegram-runner

# --- payment wiring (optional) ---------------------------------------------
PAY_ARGS=()
STRIPE_PID=""
if [ "$PAY" = 1 ]; then
  [ -z "${STRIPE_SECRET_KEY:-}" ] && { echo "--pay needs STRIPE_SECRET_KEY in .env" >&2; exit 2; }
  if ! stripe listen --print-secret >/dev/null 2>&1; then
    echo "!! Stripe CLI is not logged in (or its key expired)." >&2
    echo "   Run:  stripe login   then re-run this script with --pay." >&2
    exit 2
  fi
  echo "==> starting 'stripe listen' -> ${SERVER_URL}/webhooks/stripe"
  stripe listen --forward-to "${SERVER_URL}/webhooks/stripe" >"$WORK/stripe.log" 2>&1 &
  STRIPE_PID=$!
  # Parse the session's webhook signing secret from the listener's banner.
  WHSEC=""
  for _ in $(seq 1 40); do
    WHSEC="$(grep -oE 'whsec_[A-Za-z0-9]+' "$WORK/stripe.log" | head -1 || true)"
    [ -n "$WHSEC" ] && break
    sleep 0.25
  done
  [ -z "$WHSEC" ] && { echo "could not read whsec from stripe listen; see $WORK/stripe.log" >&2; cat "$WORK/stripe.log" >&2; exit 2; }
  echo "==> stripe webhook secret captured (whsec_…)"
  PAY_ARGS=(--enable-payments --payment-threshold-pence 3000)
  export STRIPE_WEBHOOK_SECRET="$WHSEC"
  export STRIPE_BASE_URL="${STRIPE_BASE_URL:-https://api.stripe.com}"
  export STRIPE_SUCCESS_URL="${STRIPE_SUCCESS_URL:-https://townhall.example/paid}"
  export STRIPE_CANCEL_URL="${STRIPE_CANCEL_URL:-https://townhall.example/cancelled}"
fi

# Bind FIRST: bind-channel creates + migrates the DB, so the server then opens an
# already-prepared DB — no concurrent SQLite writer.
echo "==> binding lucy -> tg:${LUCY_CHAT}"
./target/debug/bind-channel --db "$WORK/townhall.sqlite" --address "tg:${LUCY_CHAT}" --principal lucy

echo "==> starting mock council on :${COUNCIL_PORT}"
./target/debug/mock-council --db "$WORK/council.sqlite" --key-hex "$COUNCIL_KEY" --port "$COUNCIL_PORT" \
  >"$WORK/council.log" 2>&1 &
COUNCIL_PID=$!

echo "==> starting townhall-server on :${SERVER_PORT} (request logging on${PAY:+, payments on})"
TOWNHALL_LOG_REQUESTS=1 ./target/debug/townhall-server \
  --db "$WORK/townhall.sqlite" --denials-db "$WORK/denials.sqlite" \
  --council-url "$COUNCIL_URL" --key-hex "$COUNCIL_KEY" --authority-key "$AUTHORITY_KEY" \
  --port "$SERVER_PORT" --reconcile-interval-ms 50 "${PAY_ARGS[@]}" \
  >"$WORK/server.log" 2>&1 &
SERVER_PID=$!

RUNNER_PID=""
cleanup() { kill "$COUNCIL_PID" "$SERVER_PID" ${RUNNER_PID:+"$RUNNER_PID"} ${STRIPE_PID:+"$STRIPE_PID"} 2>/dev/null || true; }
trap cleanup EXIT INT TERM

echo "==> waiting for council + server"
for _ in $(seq 1 40); do curl -s -o /dev/null "$COUNCIL_URL/" && break; sleep 0.25; done
for _ in $(seq 1 40); do curl -s -o /dev/null "$SERVER_URL/" && break; sleep 0.25; done

echo "============================================================"
echo " READY. Text @Bldtest_bot from your phone, e.g.:"
echo "   BOOK date=2026-09-10 from=14:00 to=17:00 people=20 accessible=yes max=5000"
if [ "$PAY" = 1 ]; then
  echo " (fee ≥ £30 threshold, so it routes to PAYMENT)"
  echo " then reply:  YES <code>   -> the bot sends a REAL Stripe checkout link"
  echo " pay it, then reply:  STATUS   -> the bot confirms 'Booked. Council ref …'"
else
  echo " then reply:  YES <code>   (the code is in the preview it sends back)"
fi
echo " Ctrl-C to stop. Logs: $WORK/{server,council${PAY:+,stripe}}.log"
echo "============================================================"

TELEGRAM_BOT_TOKEN="$TELEGRAM_BOT_TOKEN" \
  ./target/debug/telegram-runner --server "$SERVER_URL" --lucy-chat "$LUCY_CHAT" \
  --stop-file "$WORK/stop.list" --continuation-file "$WORK/continuation.jsonl" &
RUNNER_PID=$!
wait "$RUNNER_PID"
