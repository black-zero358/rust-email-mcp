#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Exercise Docker's real ignore matcher with synthetic data only. COPY . makes
# any accidentally allowed sibling visible even if today's Dockerfile does not
# copy that sibling into its image.
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
context="$fixture/context"
exported="$fixture/exported"
mkdir -p "$context/src" "$context/vendor/async-imap" "$context/vendor/imap-proto" "$context/vendor/lettre" \
  "$context/vendor/unapproved" "$context/vendor/imap-proto-backup" "$context/vendor/lettre-backup" "$context/secrets" "$context/scripts"
cp .dockerignore "$context/.dockerignore"
printf 'synthetic manifest\n' > "$context/Cargo.toml"
printf 'synthetic lock\n' > "$context/Cargo.lock"
printf 'allowed\n' > "$context/src/ALLOWED_SENTINEL"
printf 'allowed\n' > "$context/vendor/async-imap/ALLOWED_SENTINEL"
printf 'allowed\n' > "$context/vendor/imap-proto/ALLOWED_SENTINEL"
printf 'allowed\n' > "$context/vendor/lettre/ALLOWED_SENTINEL"
printf 'synthetic only\n' > "$context/vendor/unapproved/PRIVATE_SENTINEL"
printf 'synthetic only\n' > "$context/vendor/imap-proto-backup/PRIVATE_SENTINEL"
printf 'synthetic only\n' > "$context/vendor/lettre-backup/PRIVATE_SENTINEL"
printf 'synthetic only\n' > "$context/secrets/PRIVATE_SENTINEL"
printf 'synthetic only\n' > "$context/scripts/PRIVATE_SENTINEL"
printf 'synthetic only\n' > "$context/.env"
printf 'FROM scratch\nCOPY . /\n' > "$context/Dockerfile.context-test"
docker buildx build --quiet --file "$context/Dockerfile.context-test" \
  --output "type=local,dest=$exported" "$context"
actual=$(find "$exported" -type f -printf '%P\n' | LC_ALL=C sort)
expected=$'Cargo.lock\nCargo.toml\nsrc/ALLOWED_SENTINEL\nvendor/async-imap/ALLOWED_SENTINEL\nvendor/imap-proto/ALLOWED_SENTINEL\nvendor/lettre/ALLOWED_SENTINEL'
if [[ "$actual" != "$expected" ]]; then
  printf 'Unexpected Docker context contents:\n%s\n' "$actual" >&2
  exit 1
fi
printf 'Docker context allowlist passed.\n'
