# lettre SMTP reply budget patch

- Upstream: [lettre 0.11.23](https://crates.io/crates/lettre/0.11.23), registry archive commit [eb6136a](https://github.com/lettre/lettre/commit/eb6136a06f1aba9432f73ef8f02a06aa0afc8f9c).
- Archive SHA-256: f2c646bd5cc763b1087b15493e29a64be6147ba8f19342004fa52048ee596eae.
- License: [MIT](LICENSE).
- Import: registry library sources, normalized manifest, README and license. The local manifest omits upstream examples, integration tests, benches and development dependencies. Upstream inline test modules are omitted from the copy, and the archive remains the source for those tests. Production features and dependency declarations retain their upstream values.

[Upstream PR #1143](https://github.com/lettre/lettre/pull/1143) set the line and total caps. The 0.11.23 async reader checked them after a complete line had accumulated. The local patch caps the bytes exposed to the async line reader before it can append to the buffer. The existing 1,000-byte line and 100,000-byte reply budgets include line endings. Each reply uses a reusable 1,000-byte line buffer, a capped cumulative buffer and the existing fixed-size I/O prefetch. The response parser and TLS/AUTH behavior remain the upstream implementation.

A reply that exhausts its allowance before completion poisons the connection, clears prefetched bytes and drops the underlying stream. Internal cleanup skips QUIT on that stream. The synchronous connection keeps its upstream implementation. The service only uses the async connection for SMTP.

The root SMTP regressions exercise fragmented greeting and EHLO replies, a complete reply at the exact limits, limit exhaustion with the peer still open, reply boundaries across prefetch, DATA and QUIT outcomes, and authenticated implicit TLS and STARTTLS. Remove the patch when an upstream release enforces equivalent bounds before accumulation and passes these regressions. The audit adapter restores the exact upstream registry identity in its temporary lockfile.
