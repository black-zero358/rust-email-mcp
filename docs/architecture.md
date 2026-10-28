# Architecture decisions

## MCP and mail

The service uses the official Rust MCP SDK, rmcp 3.8.0, over Streamable HTTP. IMAP uses async-imap over certificate-verified TLS; SMTP uses lettre with implicit TLS or required STARTTLS. Python and Node are absent from the runtime.

Each operation acquires one of two mailbox slots. Connections belong to one operation and are dropped on completion, timeout or parser failure. Reads use EXAMINE and BODY.PEEK. The socket wrapper enforces a byte budget before the IMAP parser reads literals. Queries scan at most 500 UIDs and return at most 50 summaries. Clients advance next_before_uid through sparse windows. Message references include UIDVALIDITY.

Sending commits an operation key and argument fingerprint before SMTP. Concurrent callers share the stored result. A lost SMTP response becomes `unknown`; another request with the same key returns the recorded state. Process restart changes unfinished sends to `unknown`. SMTP acceptance remains `accepted` if a later journal update fails, with a separate warning. Draft sending retains the source draft and preserves its MIME body and BCC envelope while removing BCC headers from transmitted content.

## Authentication

Rauthy 0.37.2 is the selected identity service, using embedded SQLite storage. It owns authorization code/PKCE, user login, refresh rotation and session persistence. The mail service validates RS256 signatures, issuer, resource audience, expiry, not-before, required scope and a subject allowlist on every HTTP request. Static OAuth clients keep callback allowlists explicit; production and acceptance tests use separate clients and users.

This removes the previous FastMCP-to-Dex token proxy. That proxy shared a single upstream Dex client; new test logins invalidated existing refresh records. Session isolation and restart recovery are deployment acceptance criteria.

Keycloak supports PostgreSQL as a production database. Its supported database list does not include SQLite. It was deprioritized because the deployment's primary storage objective is reducing resident memory. The existing PostgreSQL data remains available for rollback.

Rauthy's refresh-token not-before restriction is disabled to accommodate clients that refresh before access-token expiration. Rotation and a five-second refresh grace interval remain enabled. The single-node Hiqlite configuration uses immediate durable Raft log synchronization, one read connection, one HTTP worker and one concurrent Argon2id hash with 32 MiB memory, three iterations and two lanes. Multi-session refresh, restart recovery and memory measurements are release acceptance checks.

## Local operation store

SQLite stores send keys, argument hashes, message IDs, timestamps and outcomes. It contains no email bodies or recipient lists. WAL permits readers alongside a writer; FULL synchronization commits intent before network submission. Transactions stay short and never span SMTP or IMAP calls. The database lives on a local filesystem, with one service process owning it. Backups use SQLite's backup API or a stopped service.

A four-worker benchmark of 1,000 claim/finish/read cycles completed in 4.068 seconds on the deployment host: median 3.719 ms, p95 7.236 ms and maximum 2.107 seconds. Authentication uses Rauthy's embedded Hiqlite/SQLite backend. See [validation](validation.md) for the workload and measurements.

## Logs and failure handling

JSON logs contain server-generated request IDs, allowlisted tool names, elapsed milliseconds, HTTP status and stable error codes. Credentials, tokens, authorization codes, mailbox subjects, addresses, bodies, attachment bytes and raw library errors are excluded. Library protocol logs are disabled by a fixed filter. Container logs rotate at 5 MiB with two files. A panic hook emits a fixed event without formatting panic payloads.

Tool errors use structured content. Multi-message reads expose per-item results. Sending has durable outcomes and a status tool. Health checks report process/configuration; separately scheduled read-only probes can measure mailbox connectivity using an isolated identity.

## Sources checked 2026-10-28

- [Official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk/tree/rmcp-v3.8.0)
- [OpenAI MCP authentication](https://developers.openai.com/plugins/build/auth)
- [Keycloak OIDC endpoints](https://www.keycloak.org/securing-apps/oidc-layers)
- [Keycloak supported databases](https://www.keycloak.org/server/db)
- [Keycloak container sizing and hardening](https://www.keycloak.org/server/containers)
- [SQLite WAL and concurrency](https://www.sqlite.org/wal.html)
- [Rauthy refresh configuration](https://sebadob.github.io/rauthy/config/config.html)
- [lettre BCC handling](https://docs.rs/lettre/latest/lettre/message/struct.MessageBuilder.html#method.keep_bcc)
- [async-imap session operations](https://docs.rs/async-imap/latest/async_imap/struct.Session.html)
