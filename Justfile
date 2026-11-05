default:
    @just --list

# Run all Rust tests, then documentation examples.
test:
    cargo nextest run --workspace --all-targets --locked
    cargo test --workspace --doc --locked

# Run the same checks as CI.
check:
    uv run --no-project python scripts/check_publish.py
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    just test
    just release-test

# Check installer and release behavior with local fixtures.
release-test:
    uv run --no-project python -m unittest discover -s tests -p 'release_test.py'

# Build, train, rebuild, and package a native PGO release.
release-build:
    uv run --no-project python scripts/build_release.py

# Test the PGO build helpers.
pgo-test:
    uv run --no-project python -m unittest discover -s tests -p 'release_test.py' -k PgoTest

# Build one native PGO release for an optional target.
pgo-build target="":
    uv run --no-project python scripts/build_release.py {{ if target == "" { "" } else { "--target=" + target } }}

# Build matched baseline and PGO binaries, then compare them on held-out previews.
pgo-bench target="":
    uv run --no-project python scripts/build_release.py --baseline-and-pgo {{ if target == "" { "" } else { "--target=" + target } }}
    uv run --no-project python scripts/benchmark_pgo.py --json target/pgo/results.json {{ if target == "" { "" } else { "--target=" + target } }}
