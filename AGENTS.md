# Development

- Rust: Cargo, pinned Cargo.lock; temporary Python scripts: uv; Node tooling: pnpm.
- Keep secrets and deployment identities out of Git and logs.
- Never send or modify real email for tests without explicit user authorization.
- Read with EXAMINE/PEEK. Bound connection count, response bytes, message size and tool output.
- Tool output budget counts the complete serialized JSON-RPC response with actual id, request_id, structuredContent and escaped text copy. HTTP rejects ids over 1024 serialized JSON bytes before execution; keep this guard wired ahead of rmcp. Limit fallback remains bounded and preserves known send status; use get_operation with the original operation_key before retrying writes.
- Persist send intent before SMTP. Preserve accepted/unknown outcomes across audit or cleanup failures; never retry unknown sends automatically.
- Authenticate each HTTP request; validate issuer, audience, expiry, scope and subject allowlist.
- Test OAuth with isolated clients/users. Keep deployment and rollback separate from source publishing.
- Completed: core, 16 regression tests, fmt/clippy, isolated OAuth + QQ read acceptance, SQLite WAL/FULL benchmark.
- Lessons: Rauthy WebAuthn origin requires explicit :443; allow early token refresh; preserve independent sessions. Bound IMAP literals before parser allocation. JWT uses AWS-LC to keep known vulnerable RustCrypto transitive dependencies out of the lockfile.
- Completed: restart recovery, public deployment templates, GitHub publication, production cutover and rollback documentation. Acceptance mailbox authorization removed after testing.
- TODO: user-side ChatGPT reconnection/natural refresh; authorized real-mail writes in a dedicated test mailbox; sustained load/attachment memory observations.
- ChatGPT callback must exactly match its setup UI. Rauthy 0.37.2 discovery omits supported public-client `none`; see deployment guide for the metadata correction and refresh it when upgrading.
- Keep Rauthy static asset/theme requests outside login rate limits. Test every login-page script/style, including burst loads; HTML 200 alone misses frontend initialization failures.
- APPEND flags passed to async-imap 0.12 are serialized verbatim: use a parenthesized list such as `(\Draft)`. The synthetic duplex regression covers literal continuation, OK and rejected NO/BAD outcomes without contacting a mailbox.
- Issue #3: parse MIME structure with shared node/depth/header budgets before allocation; reserve transfer/charset decoding budgets before decoding. Do not restore mailparse::parse_mail on untrusted input. Reply/draft paths need bounded headers only. Limits and strict delimiter matching are documented in README; regressions use synthetic data only.
