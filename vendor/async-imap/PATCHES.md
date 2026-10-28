# Local completion-status patch

Source: crates.io `async-imap` 0.12.0, upstream commit
`31fde98fceb5b9bde726807419992f8d84b1f193`.
Archive SHA256: `f97b87216c9f0ccc63c516263169085fa34bba25633f6d96c6a9b9ca1fd5f70a`.
MIT OR Apache-2.0; both licenses are included. Only source, package metadata,
README and licenses are retained. No dependency versions changed.

The release's `filter`/`filter_sync` silently consumed matching tagged NO/BAD.
`src/parse.rs` now uses a shared `checked_responses` stream which stops normally
only on a matching tagged OK. It emits Error::No/Error::Bad for rejection, an I/O
error for an unexpected tag/status or EOF before completion, and terminates after
its first error. Callers keep one stream instance for the entire command.
The existing LIST/FETCH/STORE/EXPUNGE/SEARCH, CAPABILITY/NOOP/METADATA and
ID/QUOTA parsers all use this boundary; untagged data parsing is unchanged.
`extensions/id.rs` and `extensions/quota.rs` use the same helper.

The completion requirement follows [RFC 3501 section 7.1](https://www.rfc-editor.org/rfc/rfc3501#section-7.1).
The [upstream parser](https://github.com/chatmail/async-imap/blob/31fde98fceb5b9bde726807419992f8d84b1f193/src/parse.rs)
is the comparison baseline. Replace this patch with an official release only
after the protocol regressions pass against that release.

Integration tests cover empty/partial results followed by OK, NO, BAD, EOF and a
wrong tag for STORE, EXPUNGE, LIST, SEARCH and FETCH. Application tests verify
structured errors and that rejected STORE stops permanent deletion before
EXPUNGE. Tests contact no real mail service.

Run `bash scripts/audit-dependencies.sh` from the repository root, as CI does.
[RustSec skips path packages](https://github.com/RustSec/rustsec/blob/main/rustsec/src/database.rs#L133-L145),
so this script audits a temporary view restoring this crate's upstream registry
identity. It checks upstream-version advisories for the patched crate plus the
current locked dependencies. The actual Cargo.lock and resolved source stay
unchanged. This does not substitute for reviewing local patch code.

The script fails on a changed upstream checksum, missing/duplicate package,
unexpected source/checksum or changed pinned version. Upgrade the provenance,
script and tests together when changing this dependency; verify audit coverage
against RustSec again. There is no fallback that suppresses an audit failure.

## FETCH attribute presence (issue #29)

`src/types/fetch.rs` adds the read-only `Fetch::has_flags()` accessor, including
explicit `FLAGS ()`. The existing `flags()` iterator returns no elements for
both an absent attribute and an empty flag set; applications merging FETCH
fragments must distinguish them. No parsing or iterator semantics change.
Upstream archive, version, checksum and licenses above remain unchanged.

Application-level synthetic regressions exercise the accessor through the real
parser: late empty FLAGS clears a previous set, while later UID/SIZE/HEADER
fragments without FLAGS retain it. They also cover raw SIZE/BODY selection,
UID filtering, unique list limits and tagged completion. These regressions run
in the root locked test suite; audit advisory matching does not validate this
local accessor. Preserve or replace this API when upgrading the vendored crate.
