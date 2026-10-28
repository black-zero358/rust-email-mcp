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

## SELECT/EXAMINE and STATUS completion (issue #28)

`src/parse.rs` also routes `parse_mailbox` and `parse_status` through one
`checked_responses` instance per command. SELECT, SELECT (CONDSTORE), EXAMINE
and STATUS now require matching tagged OK; empty or partial response EOF,
wrong tags and matching NO/BAD all fail before returning a Mailbox. Mailbox
field extraction, unsolicited handling and typed NO/BAD errors are preserved.
The shared helper is unchanged; this closes two omitted callers rather than
introducing a second completion state machine. Upstream version, checksum,
licenses and locked dependencies remain unchanged.

`mailbox_commands_require_matching_tagged_completion` tests all four command
entry points with both empty and partial data followed by OK/NO/BAD/EOF/wrong
tag. `empty_mailbox_pages_require_completed_examine` exercises application
list/search empty-page behavior, structured backend errors and absence of any
follow-up command. No real service is contacted.

This requirement follows [RFC 3501 section 2.2.2](https://www.rfc-editor.org/rfc/rfc3501#section-2.2.2)
and [EXAMINE](https://www.rfc-editor.org/rfc/rfc3501#section-6.3.2).
The advisory audit must still be paired with local completion-code review and
these regressions; changing the official upstream release requires both.