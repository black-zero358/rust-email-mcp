# Synthetic attachment delivery fixtures

These bytes come from `scripts/attachment-delivery-probe/probe.py::fixtures`.
`manifest.json` retains each original filename, media type, decoded length,
SHA-256 and verified parser result from the R00 self-test evidence.
Git treats CSV, PDF and XLSX fixtures as binary to retain the exact CRLF bytes,
PDF offsets and boundary sizes across Windows and Linux checkouts. Use the
manifest and generator to inspect or change their contents.

Regenerate and verify with the locked development environment:

```sh
uv run --frozen --project scripts/attachment-delivery-probe python tests/fixtures/attachment_delivery/generate.py
```

Rust tests send these bytes through synthetic IMAP, the real MCP handler and
authenticated attachment HTTP GET, then compare the complete bytes and digest.
