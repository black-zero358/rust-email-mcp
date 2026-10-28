# Local protocol and allocation patches

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

## APPEND continuation notifications (issue #30)

`src/client.rs::Session::append` loops while waiting for synchronous literal
continuation and routes legitimate untagged notifications through the existing
bounded unsolicited-response queue. Only Continue authorizes literal bytes.
Any tagged response, BYE or EOF before continuation fails with the existing
Error::Append, so neither a rejection nor a queued later continuation sends
message content. Read errors retain the existing propagation. Final completion
still uses the existing matching-tag check. Flags and MIME bytes are unchanged.

`save_draft_waits_for_continuation_after_notifications` uses the application
save_draft path with zero, one and multiple notifications, successful literal
and tagged OK, and pre-literal NO/BAD/BYE/EOF/wrong-tag/premature-OK rejection;
it asserts no bytes are sent after rejection and checks notification delivery.
The previous APPEND flags and post-literal NO/BAD regressions remain in place.
No real mail service is used. Upstream version, archive checksum, licenses and
Cargo.lock remain unchanged; review this local state-machine change separately
from the advisory mapping in scripts/audit-dependencies.sh.

Protocol basis: [RFC 3501 section 2.2.1](https://www.rfc-editor.org/rfc/rfc3501#section-2.2.1).

## Application parser allocation limit (issue #24)

The upstream archive, version, licenses and checksum above remain unchanged.
Local changes in `src/client.rs` and `src/imap_stream.rs` add
`Client::with_buffer_limit(stream, bytes)`; `Client::new` retains the upstream
512 MiB ceiling. The application supplies its transport budget to this new
constructor before greeting/login. Limits clamp to 1..=512 MiB.

`nom::Needed::Size` counts additional bytes: checked addition of buffered and
needed bytes must fit the application limit before any growth. Unknown needs,
block rounding, initial allocations and copied response tails obey the same
limit. Fresh exact-size BytesMut blocks avoid geometric resize exceeding it;
new and old blocks can coexist during copying, so this is not an RSS promise.
Parser errors close the stream. Transport accounting remains independent.

Small synthetic regressions in application tests exercise fragmented legal text,
normal literals, known and unknown incomplete sizes, and an exact boundary.
Vendored unit regressions additionally assert unchanged allocation capacity after
rejected growth and cover arithmetic overflow, non-block limits and tail reset.
Run those tests from a temporary standalone copy of this crate with the root
Cargo.lock as the resolution seed, using `cargo test --no-default-features
--features runtime-tokio --lib imap_stream::tests`; Cargo may add upstream-only
dev dependencies to that temporary lockfile. Root tests run under `--locked`.

Review these local parser changes separately from advisory matching in
`scripts/audit-dependencies.sh`. An upstream replacement must pass both the
completion-status and allocation regressions before removing either patch.
Protocol semantics reference: [nom streaming take](https://docs.rs/nom/8.0.0/nom/bytes/streaming/fn.take.html).
