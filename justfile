# Flummox. Run `just` for the list.

# Everything that must be clean before claiming something works.
check: lint test

# Build both binaries.
build:
    cargo build --release --features gui,pack-mount

# Build the command line tool only, with no window stack.
build-cli:
    cargo build --release

# Every test, all features, including doctests.
test:
    cargo test --all-features
    python3 packaging/test_release.py

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
    cargo run --features gui,pack-mount --bin flummox-gui

# Run the command line tool, for example: just cli scan
cli *ARGS:
    cargo run --bin flummox -- {{ARGS}}

# The prose rules in CLAUDE.md, as far as grep can check them.
#
# The file set is what git knows about under the listed paths, tracked or
# new, so build output is not scanned. docs/audit quotes the banned phrases
# as evidence and is excluded, as CLAUDE.md and this file are. Each grep exit
# status is read: 0 is a hit, 1 is clean, anything else fails the recipe.
prose:
    #!/usr/bin/env bash
    set -uo pipefail
    fail=0
    if ! listing=$(git ls-files --cached --others --exclude-standard -- \
        src tests docs packaging deny.toml clippy.toml Cargo.toml README.md CHANGELOG.md \
        .github .githooks tools ':(exclude)docs/audit'); then
        echo "prose: the file listing failed" >&2
        exit 2
    fi
    files=()
    while IFS= read -r file; do
        [ -f "$file" ] && files+=("$file")
    done <<< "$listing"
    if [ "${#files[@]}" -lt 50 ]; then
        echo "prose: only ${#files[@]} files to scan, expected the whole tree" >&2
        exit 2
    fi
    check() {
        local label=$1
        shift
        local hits status
        hits=$(grep -nI "$@" "${files[@]}")
        status=$?
        case $status in
            0)
                echo "banned: $label"
                echo "$hits" | sed 's/^/  /'
                fail=1
                ;;
            1) ;;
            *)
                echo "prose: grep exited with $status for '$label'" >&2
                fail=1
                ;;
        esac
    }
    for phrase in '—' 'on purpose' 'deliberately' 'by design' 'worth knowing' \
        'the point is' 'is the point'; do
        check "$phrase" -iF -e "$phrase"
    done
    for word in 'exactly the' silently quietly lies nobody; do
        check "$word" -iwF -e "$word"
    done
    check 'two hyphens used as a dash' -E -e '[[:alpha:]]-- [[:alpha:]]'
    if [ $fail -eq 0 ]; then
        echo "prose: clean (${#files[@]} files)"
    fi
    exit $fail

# Point git at the tracked hooks, so a push runs `just ci` first.
hooks:
    git config core.hooksPath .githooks

# Generate THIRD-PARTY-LICENSES.txt. Needs cargo-about.
notices:
    cargo about generate --locked --all-features --config packaging/about.toml --output-file THIRD-PARTY-LICENSES.txt packaging/about.hbs
    test -s THIRD-PARTY-LICENSES.txt

# Cross-compile for Windows. Needs cargo-xwin, which is installed here.
win:
    cargo xwin build --release --target x86_64-pc-windows-msvc --features gui --bins

# Lint the Windows code and its tests from Linux. `win` builds without
# deny-warnings and skips tests, so an import unused only on Windows passes it.
win-lint:
    cargo xwin clippy --target x86_64-pc-windows-msvc --all-features --all-targets -- -D warnings

# Everything CI runs, in the order CI runs it.
#
# `prose` is in here so a push cannot go through with the style rules broken.
ci: lint test deny prose

# Build and bundle Linux x86_64 binaries for download. The bundle carries
# RELEASE-NOTES.md and the licence notices, so both are regenerated first.
# With no tag, prepare-release.py rewrites Cargo.toml and Cargo.lock to the
# version they already hold.
release: notices
    python3 packaging/prepare-release.py
    cargo build --locked --release --features gui,pack-mount --bins
    python3 packaging/package-release.py --require-notices
