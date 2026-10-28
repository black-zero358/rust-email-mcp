# Development

- Rust: Cargo, pinned Cargo.lock; temporary Python scripts: uv; Node tooling: pnpm.
- Keep secrets and deployment identities out of Git and logs.
- Never send or modify real email for tests without explicit user authorization.
- Read with EXAMINE/PEEK. Bound connection count, response bytes, message size and tool output.
- Persist send intent before SMTP. Preserve accepted/unknown outcomes across audit or cleanup failures; never retry unknown sends automatically.
- Authenticate each HTTP request; validate issuer, audience, expiry, scope and subject allowlist.
- Test OAuth with isolated clients/users. Keep deployment and rollback separate from source publishing.
- Completed: core, 16 regression tests, fmt/clippy, isolated OAuth + QQ read acceptance, SQLite WAL/FULL benchmark.
- Lessons: Rauthy WebAuthn origin requires explicit :443; allow early token refresh; preserve independent sessions. Bound IMAP literals before parser allocation.
- TODO: restart acceptance, publish deployment templates and GitHub release, switch production, record rollback and client reconnection.
