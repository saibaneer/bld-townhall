#!/usr/bin/env bash
# ============================================================================
# BLD multi-model orchestrator
#   PLANNER : deepseek-v4-flash:cloud  (architecture / plan)
#   CODER   : glm-5.3:cloud            (implementation -> emits a git diff)
#   REVIEW  : glm-5.3-flash:cloud      (fast verification pass)
#
# The coder is given the REAL contents of the files the plan names, so it
# produces a diff that actually applies.
#
# Usage:
#   ./orchestrate.sh "describe the feature/task"
#   ./orchestrate.sh --plan-only "task"   # planning only, no coding
#   ./orchestrate.sh --no-apply "task"    # produce diff but don't apply it
#   ./orchestrate.sh --no-verify "task"   # skip the review pass
#   ./orchestrate.sh --files "a.rs b.rs" "task"  # force which files to inject
#
# Safety: before applying, the current tree is committed to a WIP commit so
# the change is always reversible with `git reset --hard <wip>`.
# ============================================================================
set -euo pipefail

PLANNER="deepseek-v4-flash:cloud"
CODER="glm-5.3:cloud"
REVIEWER="glm-5.3-flash:cloud"

PLAN_ONLY=0
APPLY=1
VERIFY=1
FORCE_FILES=""
TASK=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --plan-only) PLAN_ONLY=1 ;;
    --no-apply)  APPLY=0 ;;
    --no-verify) VERIFY=0 ;;
    --files)     FORCE_FILES="$2"; shift ;;
    *) TASK="$TASK $1" ;;
  esac
  shift
done
TASK="$(echo "$TASK" | sed 's/^ *//; s/ *$//')"
[ -z "$TASK" ] && { echo "error: no task given" >&2; exit 1; }

ask() { # ask <model> <prompt>
  ollama run "$1" "$2" 2>/dev/null \
    | sed -e 's/\x1b\[[0-9;?]*[a-zA-Z]//g' \
          -e 's/\x1b\][^\x07]*\x07//g' \
          -e 's/\x1b\][^\x1b]*\x1b\\//g' \
    | tr -d '\r' | sed '/^$/d'
}

mkdir -p .orchestration

echo "==> [PLANNER] $PLANNER"
ask "$PLANNER" "You are the architect/planner for the BLD Town Hall Rust repo.
Read docs/technical-spec-v0.4.2.md and AGENTS.md conventions before planning.
Produce a concrete, milestone-aware implementation plan for this task:
\"$TASK\"
Return: (1) goal, (2) exact file paths to touch (one per line, full paths like
crates/bld-kernel/src/lib.rs), (3) ordered steps, (4) acceptance checks.
Do NOT write code." | tee .orchestration/plan.md

if [ "$PLAN_ONLY" = "1" ]; then
  echo "==> plan-only mode; skipping coding."
  exit 0
fi

# Determine which files to inject into the coder.
if [ -n "$FORCE_FILES" ]; then
  FILES="$FORCE_FILES"
else
  FILES="$(grep -oE 'crates/[A-Za-z0-9_./-]+\.rs' .orchestration/plan.md | sort -u | tr '\n' ' ')"
fi
[ -z "$FILES" ] && FILES="crates/bld-kernel/src/lib.rs"
echo "==> injecting file contents for: $FILES"

# Build the file-contents block for the coder.
CONTENTS=""
for f in $FILES; do
  if [ -f "$f" ]; then
    CONTENTS="$CONTENTS
===== FILE: $f =====
$(cat "$f")
===== END FILE: $f =====
"
  else
    echo "!! warning: $f not found; skipping." >&2
  fi
done

echo "==> [CODER] $CODER"
ask "$CODER" "You are the implementer for the BLD Town Hall Rust repo.
Follow this plan exactly. Respect AGENTS.md: work milestone-by-milestone, do
not weaken boundaries, add deterministic tests for every consequential mutation.
PLAN:
$(cat .orchestration/plan.md)

TASK: \"$TASK\"

Here are the CURRENT contents of the files you may change:
$CONTENTS

Emit your changes as a SINGLE unified git diff (the output of 'git diff').
Wrap it between these two exact markers, one per line:
<<<DIFF_START>>>
<your unified diff here>
<<<DIFF_END>>>
Rules:
- Include ONLY the diff between the markers. No prose, no explanation.
- Use proper 'diff --git a/... b/...' headers with a/ and b/ prefixes.
- Include new files as 'new file mode' hunks and deleted files as 'deleted'.
- The diff must apply cleanly with 'git apply'." | tee .orchestration/implementation.md

# Extract the diff between markers
DIFF_FILE=".orchestration/change.diff"
awk '/^<<<DIFF_START>>>$/{f=1;next} /^<<<DIFF_END>>>$/{f=0} f' \
  .orchestration/implementation.md > "$DIFF_FILE"

if [ ! -s "$DIFF_FILE" ]; then
  echo "!! no diff found between markers; nothing to apply." >&2
  exit 1
fi

if [ "$APPLY" = "0" ]; then
  echo "==> --no-apply: diff written to $DIFF_FILE (not applied)."
  exit 0
fi

# Safety net: commit current tree so the change is reversible
if [ -n "$(git status --porcelain)" ]; then
  git add -A
  git commit -q -m "wip(orchestrator): pre-apply snapshot"
  echo "==> safety snapshot committed: $(git rev-parse --short HEAD)"
fi

echo "==> applying diff..."
if ! git apply --check "$DIFF_FILE" 2> .orchestration/apply.err; then
  echo "!! diff does not apply cleanly. Trying 3-way merge..." >&2
  if ! git apply --3way "$DIFF_FILE" 2>> .orchestration/apply.err; then
    echo "!! apply failed. See .orchestration/apply.err" >&2
    echo "   Revert with: git reset --hard HEAD~1" >&2
    exit 1
  fi
else
  git apply "$DIFF_FILE"
fi
echo "==> diff applied."

if [ "$VERIFY" = "1" ]; then
  echo "==> [REVIEWER] $REVIEWER"
  ask "$REVIEWER" "Review this implementation against the plan for correctness,
boundary violations, and missing tests. List concrete fixes only.
PLAN:
$(cat .orchestration/plan.md)
IMPLEMENTATION:
$(cat .orchestration/implementation.md)" | tee .orchestration/review.md
fi

echo "==> running make check (fmt, clippy, test)..."
if make check 2>&1 | tee .orchestration/check.log; then
  echo "==> make check PASSED."
else
  echo "!! make check FAILED. See .orchestration/check.log" >&2
  echo "   Revert with: git reset --hard HEAD~1" >&2
  exit 1
fi

echo "==> done. Artifacts in .orchestration/"
