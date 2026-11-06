#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C
umask 077

usage() {
  cat <<'EOF'
Usage:
  bash scripts/package-release.sh --source-sha COMMIT --output NEW_DIR --schema-version N
  bash scripts/package-release.sh --snapshot-label LABEL --binary ABS_PATH --output NEW_DIR --schema-version N

Commit mode builds musl from a Git archive using the repository's Rust toolchain
and an isolated Cargo target directory. Snapshot mode checks a prebuilt static
amd64 binary and includes the snapshot label in the archive filename. Both run
the isolated cache recovery smoke test and write SHA256SUMS to NEW_DIR.
EOF
}

fail() { printf '%s\n' "$*" >&2; exit 1; }

source_sha= snapshot_label= binary= output= schema=
while (( $# )); do
  case "$1" in
    --help|-h) usage; exit 0 ;;
    --source-sha|--snapshot-label|--binary|--output|--schema-version)
      (( $# >= 2 )) && [[ -n "$2" && "$2" != --* ]] || fail "Missing value for $1"
      case "$1" in
        --source-sha) source_sha=$2 ;;
        --snapshot-label) snapshot_label=$2 ;;
        --binary) binary=$2 ;;
        --output) output=$2 ;;
        --schema-version) schema=$2 ;;
      esac
      shift 2 ;;
    *) usage >&2; fail "Unexpected argument: $1" ;;
  esac
done

[[ -n "$output" && "$schema" =~ ^[1-9][0-9]*$ ]] || fail 'Provide --output and a positive --schema-version'
if [[ -n "$source_sha" ]]; then
  [[ -z "$snapshot_label" && -z "$binary" ]] || fail 'Commit mode builds its own binary'
  [[ "$source_sha" =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]] || fail '--source-sha must be a full Git commit SHA'
else
  [[ "$snapshot_label" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]{0,79}$ && "$binary" == /* ]] || \
    fail 'Snapshot mode requires --snapshot-label and an absolute --binary'
fi

repo=$(cd -- "$(dirname -- "$0")/.." && pwd -P)
target=x86_64-unknown-linux-musl
[[ ! -e "$output" && ! -L "$output" ]] || fail '--output must name a new directory'
output=$(realpath -m -- "$output")
[[ ! -e "$output" && ! -L "$output" && -d "$(dirname -- "$output")" ]] || \
  fail '--output must be a new directory with an existing parent'
work=$(mktemp -d)
publish=
cleanup() {
  rm -rf -- "$work"
  if [[ -n "$publish" ]]; then rm -rf -- "$publish"; fi
}
trap cleanup EXIT

if [[ -n "$source_sha" ]]; then
  commit=$(git -C "$repo" rev-parse --verify "$source_sha^{commit}")
  [[ "$commit" == "$source_sha" ]] || fail '--source-sha must name the commit itself'
  source_root="$work/source"
  mkdir -- "$source_root"
  git -C "$repo" archive "$commit" | tar -xf - -C "$source_root"
  source_kind=commit
  source_identity=$commit
  epoch=$(git -C "$repo" show -s --format=%ct "$commit")
else
  source_root=$repo
  source_kind=workspace-snapshot
  source_identity=$snapshot_label
  epoch=0
  binary=$(realpath -e -- "$binary")
fi

version=$(awk -F '"' '/^version = "/ { print $2; exit }' "$source_root/Cargo.toml")
toolchain=$(awk -F '"' '/^channel = "/ { print $2; exit }' "$source_root/rust-toolchain.toml")
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ && "$toolchain" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || \
  fail 'Expected a release version and a pinned Rust toolchain'

if [[ "$source_kind" == commit ]]; then
  (
    cd -- "$source_root"
    export CARGO_TARGET_DIR="$work/target"
    export CC_x86_64_unknown_linux_musl="${CC_x86_64_unknown_linux_musl:-musl-gcc}"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="${CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER:-musl-gcc}"
    rustc "+$toolchain" -vV
    printf 'C compiler: %s\n' "$CC_x86_64_unknown_linux_musl"
    "$CC_x86_64_unknown_linux_musl" --version
    printf 'Rust target linker: %s\n' "$CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER"
    "$CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER" --version
    if linker_version=$("$CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER" -Wl,--version 2>/dev/null); then
      printf '%s\n' "$linker_version"
    fi
    cargo "+$toolchain" build --release --locked --target "$target"
  )
  binary="$work/target/$target/release/rust-email-mcp"
fi

[[ -f "$binary" && -x "$binary" ]] || fail 'Release binary is not an executable file'
name="rust-email-mcp-$version-$target"
mkdir -- "$work/$name"
chmod 755 -- "$work/$name"
install -m 755 -- "$binary" "$work/$name/rust-email-mcp"
artifact="$work/$name/rust-email-mcp"
header=$(readelf -hW -- "$artifact")
program_headers=$(readelf -lW -- "$artifact")
dynamic=$(readelf -dW -- "$artifact")
[[ "$header" == *ELF64* && "$header" == *'little endian'* && \
   "$header" == *'Advanced Micro Devices X86-64'* && "$program_headers" != *INTERP* && \
   "$dynamic" != *'(NEEDED)'* ]] || fail 'Expected a static Linux amd64 ELF'
file -- "$artifact"
uv run --no-project python "$source_root/scripts/check-cache-recovery.py" \
  --binary "$artifact" --schema-version "$schema"

install -m 644 -- "$source_root/README.md" "$source_root/LICENSE" "$work/$name/"
binary_hash=$(sha256sum -- "$artifact")
printf 'version=%s\ntarget=%s\nrust_toolchain=%s\nsource_kind=%s\nsource_identity=%s\nbinary_sha256=%s\n' \
  "$version" "$target" "$toolchain" "$source_kind" "$source_identity" "${binary_hash%% *}" \
  > "$work/$name/SOURCE.txt"
chmod 644 -- "$work/$name/SOURCE.txt"

publish=$(mktemp -d -- "$(dirname -- "$output")/.release-package.XXXXXXXX")
archive="$name.tar.gz"
if [[ "$source_kind" == workspace-snapshot ]]; then
  archive="$name-snapshot-$snapshot_label.tar.gz"
fi
tar --format=ustar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner \
  -C "$work" -cf - -- "$name" | gzip -n > "$publish/$archive"
(cd -- "$publish" && sha256sum -- "$archive" > SHA256SUMS && sha256sum -c SHA256SUMS)
mv -nT -- "$publish" "$output"
[[ ! -e "$publish" ]] || fail 'Release output already exists'
publish=
printf 'Release package: %s/%s\n' "$output" "$archive"
