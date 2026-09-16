# Flummox. Run `just` for the list.

# Everything that must be clean before claiming something works.
check: lint test

# Build both binaries.
build:
    cargo build --release --features gui

# Build the command line tool only, with no window stack.
build-cli:
    cargo build --release

# Every test, all features, including doctests.
test:
    cargo test --all-features

# Clippy at deny-warnings, over every target and feature.
#
# --all-targets does not enable features, so the GUI would go unchecked
# without --all-features.
lint:
    cargo clippy --all-features --all-targets -- -D warnings

# Dependency policy: advisories, licences, duplicates, sources.
deny:
    cargo deny check

# Run the window from the working tree.
gui:
    cargo run --features gui --bin flummox-gui

# Run the command line tool, for example: just cli scan
cli *ARGS:
    cargo run --bin flummox -- {{ARGS}}

# The prose rules in CLAUDE.md, as far as grep can check them.
prose:
    #!/usr/bin/env bash
    set -uo pipefail
    fail=0
    for pattern in '—' 'on purpose' 'deliberately' 'by design' 'worth knowing' 'is the point'; do
        hits=$(grep -rn "$pattern" src tests README.md deny.toml CLAUDE.md 2>/dev/null | grep -v 'CLAUDE.md' || true)
        if [ -n "$hits" ]; then
            echo "banned: $pattern"
            echo "$hits" | sed 's/^/  /'
            fail=1
        fi
    done
    [ $fail -eq 0 ] && echo "prose: clean"
    exit $fail

# Cross-compile for Windows. Needs cargo-xwin, which is installed here.
win:
    cargo xwin build --release --target x86_64-pc-windows-msvc

# Everything CI runs, in the order CI runs it.
ci: lint test deny
