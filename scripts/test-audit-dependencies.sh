#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Offline contract tests: the cargo stub captures the exact audit view. No
# advisory fetch, Cargo resolution, credentials or main lock mutation occurs.
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
repo="$fixture/repo with spaces"
mkdir -p "$repo/scripts" "$repo/vendor/async-imap" "$repo/vendor/imap-proto" \
  "$fixture/bin" "$fixture/tmp" "$fixture/baseline"
cp scripts/audit-dependencies.sh "$repo/scripts/"
cat > "$fixture/bin/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1" == audit && "$2" == --file && -f "$3" ]]
cp "$3" "$CAPTURE"
printf '%s\n' "$@" > "$ARGUMENTS"
printf '%s\n' "$3" > "$TEMP_LOCK_PATH"
exit "${AUDIT_EXIT:-0}"
STUB
chmod +x "$fixture/bin/cargo"
cat > "$fixture/baseline/Cargo.lock" <<'LOCK'
version = 4

[[package]]
name = "async-imap"
version = "0.12.0"
dependencies = ["imap-proto"]

[[package]]
name = "imap-proto"
version = "0.17.0"
dependencies = ["nom"]

[[package]]
name = "nom"
version = "8.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "df9761775871bdef83bee530e60050f7e54b1105350d6884eb0fb4f46c2f9405"

[[package]]
name = "rust-email-mcp"
version = "0.2.1"
dependencies = ["async-imap", "imap-proto"]
LOCK
cat > "$fixture/baseline/expected.lock" <<'LOCK'
version = 4

[[package]]
name = "async-imap"
version = "0.12.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "f97b87216c9f0ccc63c516263169085fa34bba25633f6d96c6a9b9ca1fd5f70a"
dependencies = ["imap-proto"]

[[package]]
name = "imap-proto"
version = "0.17.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "5ccf963d57074747b455398a1763d174da80bcaba6f51e3671a82252b531a68b"
dependencies = ["nom"]

[[package]]
name = "nom"
version = "8.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "df9761775871bdef83bee530e60050f7e54b1105350d6884eb0fb4f46c2f9405"

[[package]]
name = "rust-email-mcp"
version = "0.2.1"
dependencies = ["async-imap", "imap-proto"]
LOCK

reset() {
  cp "$fixture/baseline/Cargo.lock" "$repo/Cargo.lock"
  printf '[package]\nname = "async-imap"\nversion = "0.12.0"\n' > "$repo/vendor/async-imap/Cargo.toml"
  printf '[package]\nname = "imap-proto"\nversion = "0.17.0"\n' > "$repo/vendor/imap-proto/Cargo.toml"
  printf '%s\n' f97b87216c9f0ccc63c516263169085fa34bba25633f6d96c6a9b9ca1fd5f70a > "$repo/vendor/async-imap/UPSTREAM.sha256"
  printf '%s\n' 5ccf963d57074747b455398a1763d174da80bcaba6f51e3671a82252b531a68b > "$repo/vendor/imap-proto/UPSTREAM.sha256"
  rm -f "$fixture/captured.lock" "$fixture/args" "$fixture/lock-path"
}
run() {
  PATH="$fixture/bin:$PATH" TMPDIR="$fixture/tmp" CAPTURE="$fixture/captured.lock" \
    ARGUMENTS="$fixture/args" TEMP_LOCK_PATH="$fixture/lock-path" AUDIT_EXIT="${AUDIT_EXIT:-0}" \
    bash "$repo/scripts/audit-dependencies.sh" --offline --json
}
cleaned() {
  [[ -z "$(find "$fixture/tmp" -type f -print -quit)" ]]
  if [[ -f "$fixture/lock-path" ]]; then
    [[ ! -e "$(cat "$fixture/lock-path")" ]]
  fi
}
passed=0
reject() {
  local label=$1 before
  before=$(sha256sum "$repo/Cargo.lock")
  if run > "$fixture/output" 2>&1; then
    printf 'Unexpected audit acceptance: %s\n' "$label" >&2
    exit 1
  fi
  [[ ! -e "$fixture/captured.lock" ]]
  [[ "$(sha256sum "$repo/Cargo.lock")" == "$before" ]]
  cleaned
  passed=$((passed + 1))
}

reset
before=$(sha256sum "$repo/Cargo.lock")
run
cmp "$fixture/baseline/expected.lock" "$fixture/captured.lock"
[[ "$(sha256sum "$repo/Cargo.lock")" == "$before" ]]
[[ "$(tail -n 2 "$fixture/args")" == $'--offline\n--json' ]]
cleaned
passed=$((passed + 1))

for crate in async-imap imap-proto; do
  reset
  printf 'wrong checksum\n' > "$repo/vendor/$crate/UPSTREAM.sha256"
  reject "$crate provenance checksum"
  reset
  sed -i 's/^version = .*/version = "9.9.9"/' "$repo/vendor/$crate/Cargo.toml"
  # A dependency with the expected version must not mask the package mismatch.
  printf '[dependencies.decoy]\nversion = "%s"\n' "$([[ "$crate" == async-imap ]] && printf 0.12.0 || printf 0.17.0)" >> "$repo/vendor/$crate/Cargo.toml"
  reject "$crate manifest package version"
  reset
  sed -i 's/^name = .*/name = "wrong-package"/' "$repo/vendor/$crate/Cargo.toml"
  reject "$crate manifest package name"
  reset
  awk -v crate="$crate" 'BEGIN { RS=""; ORS="\n\n" } index($0,"name = \"" crate "\"") == 0 { print }' "$repo/Cargo.lock" > "$fixture/changed.lock"
  cp "$fixture/changed.lock" "$repo/Cargo.lock"
  reject "$crate missing lock package"
  reset
  printf '\n[[package]]\nname = "%s"\nversion = "%s"\n' "$crate" "$([[ "$crate" == async-imap ]] && printf 0.12.0 || printf 0.17.0)" >> "$repo/Cargo.lock"
  reject "$crate duplicate lock package"
  reset
  sed -i "/^name = \"$crate\"$/,/^$/s/^version = .*/version = \"9.9.9\"/" "$repo/Cargo.lock"
  reject "$crate wrong lock version"
  reset
  sed -i "/^name = \"$crate\"$/a source = \"registry+https://github.com/rust-lang/crates.io-index\"" "$repo/Cargo.lock"
  reject "$crate unexpected source before version"
  reset
  sed -i "/^name = \"$crate\"$/a checksum = \"unexpected\"" "$repo/Cargo.lock"
  reject "$crate unexpected checksum"
done

reset
printf '\n[[package]]\nname = "unmapped-local-fork"\nversion = "1.0.0"\n' >> "$repo/Cargo.lock"
reject 'unmapped path package'
reset
printf '\n[[package]]\nname = "unmapped-git-package"\nversion = "1.0.0"\nsource = "git+https://example.test/repository"\n' >> "$repo/Cargo.lock"
reject 'unmapped git package'

reset
before=$(sha256sum "$repo/Cargo.lock")
if AUDIT_EXIT=37 run; then
  printf 'Audit failure was suppressed\n' >&2
  exit 1
else
  [[ "$?" == 37 ]]
fi
cmp "$fixture/baseline/expected.lock" "$fixture/captured.lock"
[[ "$(sha256sum "$repo/Cargo.lock")" == "$before" ]]
cleaned
passed=$((passed + 1))
printf 'Audit adapter offline regressions passed: %s.\n' "$passed"
