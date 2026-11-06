# Local protocol and allocation patches

`Session::run_command_with_limits` accepts typed `CommandReadLimits`. Its BODY
section payload cap applies to every received UID/section until the matching
tagged completion. Another command cannot override an unfinished limited read;
the stream closes after a parser error. Default command methods retain existing
raw-response behavior. `error::body_section_literal_limit` recovers the static
limit/count error without response content. The application still checks UID,
section, origin, duplicates, byte counts and tagged status before delivery.

The existing `types::ResponseData` reexport is public so the bounded MIME
manifest reader can retain one response owner without cloning its AST and inspect
the exact UID, section, origin and duplicate attributes. The type layout and
existing response parsing behavior are unchanged.

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

## Bounded mailbox synchronization (R12)

The bounded SEARCH stream rejects a second SEARCH response before parsing its
UID count, including split response prefixes. Only an oversized first result
can report a UID-count overflow. The ordinary SEARCH path retains its behavior.

`CommandReadLimits::mailbox_metadata` uses the existing bounded mailbox
notification machinery for UID FETCH metadata. It has independent limits on
total command response bytes and each header literal, and never enters
SEARCH or BODYSTRUCTURE parsing. `Fetch::parse_internal_date` exposes the
same date conversion used by ordinary FETCH so the strict collector can
validate UID and attribute presence before persisting observations.
Root protocol regressions run in `tests/thread_search_budget.rs`.

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

## BODYSTRUCTURE allocation budgets (R06 parser foundation)

`src/imap_stream.rs::decode` uses the patched imap-proto 0.17.0
`Response::parse_bounded` for every client, including `Client::new`,
`Client::with_buffer_limit`, greeting/login and diagnostic connections. The
application parser-buffer limit and the transport byte budget remain separate.
Default structure limits are 32 recursive levels, 1024 nodes, 128 KiB per string,
256 parameter pairs per MIME node, 8192 parameter pairs per response, and 1 MiB
for structure strings and container capacities. See the imap-proto PATCHES file
for recursive allocation and backtracking details.

Budget failure returns an `io::Error` containing
`imap_proto::bounded::LimitError { kind, limit, unit }`. Its Display/Debug values
contain the resource kind, numeric limit and unit; response fragments are never
formatted into this error. The raw-input decode trace was removed. Other syntax
errors retain their previous behavior. `async_imap::error::body_structure_limit`
detects this typed source in `async_imap::error::Error::Io`. The failed stream
ends after its first error and rejects subsequent encode attempts, so the failed
connection cannot be reused for another command.

The default root test `cargo test --locked --test body_structure_budget` runs the
public API budget matrix and actual Client regressions. It covers both Client
constructors, rejection of huge literal declarations before buffer growth,
private typed errors, terminal streams, blocked subsequent commands, ordinary
BODY literals, fragmented metadata and every NIL split after `N` and `NI`.
The fixed proper-prefix oracle fixtures were checked with the unmodified
registry imap-proto 0.17.0. Existing tagged-completion regressions remain required.

This parser foundation provides typed detection. Application mapping to
`LIMIT_EXCEEDED/mime_structure` and the controlled fresh-connection raw fallback
are integrated with the subsequent PartManifest reader work package.

## Bounded UID SEARCH (R09 foundation)

`CommandReadLimits::search` adds an opt-in 128 KiB command response ceiling
and at most 200 UID entries. The byte count includes complete untagged
notifications and the matching tagged completion. Each read and parser buffer
growth is checked against the remaining budget before receiving more bytes.
Bytes buffered beyond the matching completion remain available to the next
read. Buffered unsolicited notifications can precede the new command's reply.
Pre-command SEARCH data and tagged completions cannot satisfy the new command.

`Session::uid_search_bounded` requires exactly one untagged SEARCH result and
matching tagged OK. An empty SEARCH result is valid. Unexpected continuation,
duplicate results, wrong tag, BYE and EOF fail the command. The high-level
method closes the parser/command stream on failure. The existing UID SEARCH,
FETCH limits and UTF-8 literal command methods retain their behavior.

The local parser budgets nested FLAGS/FETCH allocation before reserving
capacity. It consumes status text and response codes without constructing
their AST and rejects unrelated response types. The convenience collector
forwards scalar mailbox notifications and releases other response owners
when consumed, including legitimate FETCH flag updates.

`error::search_limit` recovers typed UID-count, parser allocation or command
response-byte failures. The errors contain numeric limits only. Terminal
parser failures stop later reads and command writes.

Run `cargo test --locked --test thread_search_budget` for the public parser and
isolated async session checks. Upstream versions, checksums, licenses and
dependency resolution remain unchanged.

### Opt-in CONDSTORE commands

`CommandReadLimits::condstore()` reuses the 128 KiB cumulative command wire
budget and adds a separate grammar for CAPABILITY, EXAMINE (CONDSTORE) and
UID FETCH (CHANGEDSINCE). Pre-command capability, selection and FETCH data
cannot satisfy the command. The transport rejects unexpected tags, BYE and
continuations, preserves bytes after matching completion, and closes on parser
errors. Consumers call `Session::abort_bounded_command` when the completed
exchange fails their evidence checks. Existing command APIs retain their
original grammar. Run `cargo test --locked --test sync_delta_budget` for
fragmentation, cumulative budgets and terminal failure checks.

### Opt-in QRESYNC commands

`CommandReadLimits::qresync()` retains the 128 KiB cumulative command wire
budget, adding distinct ENABLED and compressed VANISHED responses. An
ENABLED or VANISHED response already buffered before the command cannot
satisfy new evidence. Matching tags, terminal failure and post-completion
buffering use the same machinery as other bounded commands. Consumers bind
ENABLE and subsequent commands to one connection. The application limits
generated extension command lines to 8192 bytes including framing, retaining
the exact UID set or taking its bounded fallback. Run
`cargo test --locked --test sync_qresync_budget`.

## Test suite reduction (2026-11-06)

The vendored crate-local test modules were removed as part of the repository-wide test maintenance reduction. Runtime source and upstream identity remain the same. Current parser/client verification runs through the root package integration tests in `tests/`; historical test commands above describe the earlier patch acceptance.

## Case-insensitive system flags (issue #84)

Both borrowed and owned flag conversions recognize the system flag names in
ASCII case-insensitive form. Other flags retain their original text and the
existing Custom representation. This follows [RFC 3501 section 9](https://www.rfc-editor.org/rfc/rfc3501#section-9).
The root package's summary FETCH and sync facts regressions cover mixed-case
wire flags. The upstream identity and dependency versions remain unchanged.
