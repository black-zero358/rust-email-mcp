#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# RustSec skips path packages. Audit this local fork using its upstream release
# identity too; never alter Cargo's actual lockfile or dependency sources.
# Review local completion patches separately, including SELECT/EXAMINE/STATUS;
# advisory matching cannot verify protocol completion behavior.

# APPEND continuation-state patches also require local code/protocol review;
# advisory matching cannot validate notification and literal ordering.

# Local FETCH attribute-presence accessors require root behavioral regressions
# and code review; upstream advisory matching does not validate local changes.

# Local completion-status AND parser allocation patches require code review and
# synthetic regressions separately; advisories cannot validate their behavior.
upstream_checksum=$(cat vendor/async-imap/UPSTREAM.sha256)
[[ "$upstream_checksum" == f97b87216c9f0ccc63c516263169085fa34bba25633f6d96c6a9b9ca1fd5f70a ]]
grep -Fxq 'version = "0.12.0"' vendor/async-imap/Cargo.toml
audit_lock=$(mktemp)
trap 'rm -f "$audit_lock"' EXIT
awk '
  /^\[\[package\]\]$/ { vendor = 0 }
  /^name = "async-imap"$/ { vendor = 1; found++ }
  vendor && /^(source|checksum) = / { exit 1 }
  { print }
  vendor && /^version = "0.12.0"$/ {
    print "source = \"registry+https://github.com/rust-lang/crates.io-index\""
    print "checksum = \"f97b87216c9f0ccc63c516263169085fa34bba25633f6d96c6a9b9ca1fd5f70a\""
    injected++
  }
  END { if (found != 1 || injected != 1) exit 1 }
' Cargo.lock > "$audit_lock"
cargo audit --file "$audit_lock" "$@"
