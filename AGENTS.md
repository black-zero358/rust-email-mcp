# Agent working guide

[CONTRIBUTING.md](CONTRIBUTING.md) is the engineering entry point for design, development and maintenance. This file is an execution summary; it does not replace or relax that policy. Read the relevant [architecture constraints](docs/architecture.md) before changing behavior and the [maintenance policy](docs/maintenance.md) before changing dependencies, delivery or operations.

## Working rules

- Rust: Cargo with the pinned toolchain and Cargo.lock; temporary Python scripts: uv; Node tooling: pnpm. Python and Node are not runtime dependencies.
- Keep secrets and deployment identities out of Git and logs. Use synthetic data for regressions. Never send or modify real email for tests without explicit user authorization.
- Test OAuth with isolated clients and users. Source publishing does not authorize production deployment, cutover or rollback; follow the separate deployment procedure.
- Preserve the module boundaries and public contracts in [architecture](docs/architecture.md#module-boundaries). Record material design changes there and follow the compatibility rules in CONTRIBUTING.md.

## Constraints to preserve

- Read with EXAMINE/PEEK and retain UIDVALIDITY checks. Keep target-UID FETCH handling, IMAP4rev1 text encoding, APPEND flag syntax and tagged completion checks as specified in [MCP and mail](docs/architecture.md#mcp-and-mail).
- APPEND waits through legitimate untagged notifications for literal continuation; tagged results, BYE and EOF before continuation must fail without sending literal bytes.
- Bound connections, response bytes and parser allocation. MIME structure, header and decoding budgets apply before allocation; do not restore `mailparse::parse_mail` on untrusted input. Preserve shared read/forward body selection and bounded tokenizer-only HTML handling, including foreign-content rejection and adjacent-attribute regressions. See [MIME and HTML](docs/architecture.md#mime-and-html).
- Plain-only alternative probes must cover the complete inline representation: non-empty HTML makes the candidate incomplete, and all collected fragments must be rolled back before trying another candidate.
- Persist intent before SMTP. Preserve accepted/unknown outcomes across journal or cleanup failures; never automatically retry unknown sends. Keep the documented principal-scoped operation keys, bounded QUIT cleanup, TLS/AUTH and SMTPUTF8/8BITMIME policy. Preserve the canonical draft-header privacy boundary and body bytes. See [send lifecycle](docs/architecture.md#send-lifecycle).
- Authenticate every HTTP request entering MCP. Preserve issuer, audience, expiry, nbf, scope and subject validation, the hard JWKS TTL and shared refresh cooldown. Keep deployment-specific OAuth and login-asset checks in [authentication](docs/architecture.md#authentication).
- Count the complete serialized JSON-RPC tool response, including the actual id, request_id, structuredContent and escaped text copy. Keep the 1024-byte serialized-id guard ahead of rmcp; bounded fallback must retain known send status and direct clients to `get_operation` with the original key. See [stable contracts](docs/architecture.md#stable-contracts).
- Fit `get_email` display-body prefixes against that complete wire budget at UTF-8 boundaries; preserve existing truncation flags and metadata, and retain LIMIT_EXCEEDED when metadata alone cannot fit.
- Keep the vendored dependency provenance, completion-status regressions and `scripts/audit-dependencies.sh` aligned. Plain `cargo audit` skips path packages. Keep the Docker context allowlist narrow and include only the required vendored crate in the builder. See [dependency maintenance](docs/maintenance.md) and [PATCHES.md](vendor/async-imap/PATCHES.md).

- Mail workflow extensions live in `mail/attachments.rs` and `mail/insights.rs`; reuse bounded MIME and rev1 text handling. Keep attachment indices consistent and preserve inline Base64 downloads; authenticated binary handles use a separate tool.
- Statistics require all requested STATUS fields and matching tagged completion. Batches preserve per-item unknown/unprocessed outcomes and never retry automatically; document bounded search coverage and omissions.

## Before handing off

- Follow the applicable validation and PR checklist in CONTRIBUTING.md; preserve the [invariant-to-verification mapping](docs/architecture.md#invariants-and-verification) when behavior changes.
- Keep current contracts in README/architecture, maintenance procedures in their designated guides, and dated results in [validation](docs/validation.md) with a commit or release reference. Track remaining work through [open follow-ups](CONTRIBUTING.md#待闭环事项) and issues, not Completed/TODO entries in this file.
