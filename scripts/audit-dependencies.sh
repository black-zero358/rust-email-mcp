#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# RustSec matches default-registry packages. Restore the local forks' pinned
# upstream identities in a temporary audit view, preserving the actual lock and
# dependency sources. Review local protocol/allocation patches and their
# synthetic regressions separately; see vendor/*/PATCHES.md.
verify_upstream() {
  local crate=$1 version=$2 checksum=$3
  [[ "$(cat "vendor/$crate/UPSTREAM.sha256")" == "$checksum" ]]
  awk -v expected_name="$crate" -v expected_version="$version" '
    /^\[package\]$/ { in_package = 1; packages++; next }
    /^\[/ { in_package = 0 }
    in_package && /^name = / {
      if ($0 != "name = \"" expected_name "\"") exit 1
      names++
    }
    in_package && /^version = / {
      if ($0 != "version = \"" expected_version "\"") exit 1
      versions++
    }
    END { if (packages != 1 || names != 1 || versions != 1) exit 1 }
  ' "vendor/$crate/Cargo.toml"
}
verify_upstream async-imap 0.12.0 f97b87216c9f0ccc63c516263169085fa34bba25633f6d96c6a9b9ca1fd5f70a
verify_upstream imap-proto 0.17.0 5ccf963d57074747b455398a1763d174da80bcaba6f51e3671a82252b531a68b
verify_upstream lettre 0.11.23 f2c646bd5cc763b1087b15493e29a64be6147ba8f19342004fa52048ee596eae

audit_lock=$(mktemp)
trap 'rm -f "$audit_lock"' EXIT
awk '
  BEGIN {
    versions["async-imap"] = "0.12.0"
    checksums["async-imap"] = "f97b87216c9f0ccc63c516263169085fa34bba25633f6d96c6a9b9ca1fd5f70a"
    versions["imap-proto"] = "0.17.0"
    checksums["imap-proto"] = "5ccf963d57074747b455398a1763d174da80bcaba6f51e3671a82252b531a68b"
    versions["lettre"] = "0.11.23"
    checksums["lettre"] = "f2c646bd5cc763b1087b15493e29a64be6147ba8f19342004fa52048ee596eae"
  }
  function fail() { failed = 1; exit 1 }
  function flush(  i, name, version, name_count, version_count, sourced, source, checksum_count) {
    if (!count) return
    for (i = 1; i <= count; i++) {
      if (lines[i] ~ /^name = /) {
        name = lines[i]; sub(/^name = "/, "", name); sub(/"$/, "", name); name_count++
      }
      if (lines[i] ~ /^version = /) {
        version = lines[i]; sub(/^version = "/, "", version); sub(/"$/, "", version); version_count++
      }
      if (lines[i] ~ /^source = /) {
        source = lines[i]; sub(/^source = "/, "", source); sub(/"$/, "", source); sourced++
      }
      if (lines[i] ~ /^checksum = /) checksum_count++
    }
    if (name_count != 1 || version_count != 1) fail()
    if (name in versions) {
      if (version != versions[name] || sourced || checksum_count || ++found[name] != 1) fail()
    } else if (!sourced) {
      # Future path packages need an explicit provenance/audit mapping.
      if (name != "rust-email-mcp" || ++root_count != 1 || checksum_count) fail()
    } else if (sourced != 1 || source != "registry+https://github.com/rust-lang/crates.io-index") {
      # Other registries/git dependencies also need a reviewed audit strategy.
      fail()
    }
    for (i = 1; i <= count; i++) {
      print lines[i]
      if ((name in versions) && lines[i] ~ /^version = /) {
        print "source = \"registry+https://github.com/rust-lang/crates.io-index\""
        print "checksum = \"" checksums[name] "\""
      }
      delete lines[i]
    }
    count = 0
  }
  /^\[\[package\]\]$/ { flush(); in_package = 1 }
  !in_package { print; next }
  { lines[++count] = $0 }
  END {
    if (failed) exit 1
    flush()
    if (failed || root_count != 1) exit 1
    for (name in versions) if (found[name] != 1) exit 1
  }
' Cargo.lock > "$audit_lock"
cargo audit --file "$audit_lock" "$@"
