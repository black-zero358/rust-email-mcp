# Bounded BODYSTRUCTURE parser patch

Source: crates.io `imap-proto` 0.17.0, upstream commit
`31c7f80e57cac52cb4514c35a567f29250d5941e`.
Archive SHA256: `5ccf963d57074747b455398a1763d174da80bcaba6f51e3671a82252b531a68b`.

The archive checksum was checked against Cargo.lock, the local Cargo archive,
and the official `static.crates.io` archive before copying the source.
The archive's `.cargo_vcs_info.json` records the commit above. Its original
manifest agrees with the [upstream commit](https://github.com/djc/imap-proto/blob/31c7f80e57cac52cb4514c35a567f29250d5941e/Cargo.toml).

MIT OR Apache-2.0; both unmodified license files are retained, including the
upstream copyright notices. The vendored tree retains source, normalized and
original package manifests, README, VCS provenance, licenses and the one example
explicitly declared in Cargo.toml. Upstream CI/editor configuration and its
independent lockfile are omitted. The package remains version 0.17.0; runtime
dependencies remain `nom = 8`, with `assert_matches` confined to upstream tests.

## Scope and integration

`Response::parse` retains the existing entry point and semantics.
`Response::parse_bounded(input, &BodyStructureLimits)` returns
`bounded::ParseResult`; `parse_bounded_with_usage` returns that result together
with `BudgetUsage`. Each response gets one explicit budget shared by all
BODY/BODYSTRUCTURE values, recursive extensions and nested ENVELOPE/address
structures. Re-parsing a fragmented response starts a fresh budget.

Default `BodyStructureLimits` fields are:

| Field | Default | Unit |
| --- | --- | --- |
| `depth` | 32 | levels; bounded parsing always caps recursive depth at 32 |
| `nodes` | 1024 | nodes |
| `string_bytes` | 131072 | bytes per string, including lossy UTF-8 expansion |
| `node_parameters` | 256 | parameter pairs per body node |
| `total_parameters` | 8192 | parameter pairs per response |
| `aggregate_bytes` | 1048576 | accounted string bytes plus container capacity |

The budget checks recursion before entering children, literal declarations before
requesting incomplete bytes, unfinished quoted strings while scanning, and lossy
UTF-8 expansion before allocating a replacement string. Vector growth and child
boxes are charged before allocation; accounting includes unused reserved slots.
Recoverable grammar alternatives restore their budget checkpoint. A limit
failure remains fatal through optional fields and unknown-attribute fallbacks.
Ordinary BODY section literals retain their existing wire-reader byte budget.

`ParseError::Limit(LimitError)` carries `kind`, `limit` and `unit`; syntax and
incomplete input remain in `ParseError::Syntax`. The six `LimitKind` variants
are `Depth`, `Nodes`, `StringBytes`, `NodeParameters`, `TotalParameters` and
`AggregateBytes`; `LimitUnit` uses `Levels`, `Nodes`, `Bytes` and `Pairs`.
`LimitError` implements Error and Display using these classifications and numeric
limits, with no raw input. Usage reports expose aggregate string/container
charges, nodes, parameter pairs, peak depth and largest string/reservation/
capacity measurements.

The companion local `async-imap` patch uses bounded parsing and transports the
typed `LimitError` inside its existing I/O error representation. Its
`body_structure_limit(&async_imap::error::Error)` helper recovers that type.
This parser delivery does not change application tool error mapping or add
BODYSTRUCTURE-based attachment discovery; those changes belong to the following
R06 manifest work package.

## Verification

`Response::parse_bounded_with_fetch_limit` adds a command-scoped BODY section
payload cap. Literal declarations are checked before `take` can request more
buffer space, for every UID and section. `ParseError::FetchLiteralLimit` carries
only the configured limit and observed logical byte count. The section grammar
retains HEADER.FIELDS/HEADER.FIELDS.NOT, numeric suffixes and legal incomplete
prefixes; its field and path allocations share the structure budget. Quoted
payloads are unescaped into checked logical octets only in this new entry point;
owned payload storage is charged before allocation. Existing ordinary parsing
keeps its wire representation and raw literal budgets.

The root `tests/part_fetch_budget.rs` covers declaration rejection without any
payload, section/quoted prefix compatibility, typed error privacy and command
completion reset through the real async client.

Recursive multipart and message parsing uses sequential field parsing and
special-type prefix dispatch to keep debug-build parser frames small. Streaming
prefix probes preserve `Incomplete` before selecting a variant, including exact
small string budgets where TEXT/MESSAGE labels use static AST strings. This
retains the recoverable basic-type fallback without constructing a recursive
four-way alternative result at every MIME level.

The root `tests/body_structure_budget.rs` suite runs under the normal locked CI
tests. Its cases cover exact/overflow boundaries, multiple structures sharing
one response budget, nested messages/envelopes, optional fields, incomplete
literals/quotes, lossy text, container accounting, grammar rollback and unchanged
ordinary FETCH/BODY behavior. Run `cargo test --locked --test body_structure_budget`.
The existing upstream parser vectors are also compared across both entry points
in the isolated parser validation harness; record the final command and count
with the candidate's validation evidence.

## Advisory auditing and maintenance

Run `bash scripts/test-audit-dependencies.sh`, then
`bash scripts/audit-dependencies.sh` from the repository root. The offline
script regressions exercise both local package mappings, strict provenance and
version checks, missing/duplicate/unexpected sources, audit failure propagation,
temporary-file cleanup and preservation of the real lockfile.

The audit script restores this package and local `async-imap` to their exact
upstream registry identities in one temporary audit view. RustSec
[matches default-registry sources](https://github.com/RustSec/rustsec/blob/main/rustsec/src/database.rs#L133-L143),
so both local forks need that mapping. The actual resolved sources and Cargo.lock
stay unchanged. Local allocation accounting, parser semantics and error
propagation are reviewed and tested separately from upstream-version advisories.

When upgrading, update the upstream archive, commit, version, checksum and
licenses together with both audit mappings and regressions. Replace the local
patch only after the bounded parser, protocol completion and application
regressions pass against the replacement.
