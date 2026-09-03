# A local stand-in for GitHub Actions' ubuntu-latest runner.
#
# # Why this exists
#
# An afternoon was lost to a bug that passed on macOS and hung on CI: a test
# asserted a binary lacked a feature while pointing at a path every
# feature-build overwrites, and only the runner's cache exposed it. The lesson
# is not the bug — it is that "green on my machine" meant one machine, and CI
# ran on a different OS as a different user with different tools installed.
#
# This image closes that gap. `ci/run.sh` runs the exact CI steps inside it, so
# "green locally" means green on Linux, non-root, with the runner's tools — the
# thing that was untrue all afternoon.
#
# Deliberately NOT the whole runner: no Actions env, no rust-cache. `run.sh`
# reproduces the one cache behaviour that mattered (a feature-enabled binary
# left at the shared path) explicitly, because a test's correctness should not
# depend on a caching accident either way.
FROM rust:1.85

# ubuntu-latest ships these; the bare rust image does not. Their absence was two
# separate false leads before this file existed.
RUN apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends sqlite3 curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN rustup component add clippy rustfmt

# A non-root user, because the runner is one — and because `file_suppression`'s
# test makes a directory unwritable and expects the write to fail, which root
# silently bypasses. Running as root turned a real test into a false pass.
RUN useradd --create-home --uid 1001 runner
USER runner
WORKDIR /work
