import test from "node:test";
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { Readable } from "node:stream";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve, dirname, basename } from "node:path";
import { McpUiMessageRequestSchema, McpUiUpdateModelContextRequestSchema } from "@modelcontextprotocol/ext-apps";
import { receiveAttachment, safeFilename, MAX_FILE_BYTES } from "./standard-client.mjs";
import { assembleFromUiTool, registerGeneratedFile, offerResourceContext, CHUNK_BYTES } from "./ui-candidate.mjs";

const sha = bytes => createHash("sha256").update(bytes).digest("hex");
const token = "synthetic-local-adapter-only";
const fixture = Buffer.from("项目,数量,金额\r\n一,3,120.50\r\n二,2,79.25\r\n退款,1,-5.75\r\n");
async function temporary(run) {
  const root = await mkdtemp(join(tmpdir(), "attachment-adapter-test-"));
  try { return await run(root); }
  finally {
    assert.equal(dirname(resolve(root)), resolve(tmpdir()));
    assert.ok(basename(root).startsWith("attachment-adapter-test-"));
    await rm(root, { recursive: true, force: true });
  }
}
async function fixtureServer(options, run) {
  const bytes = options.bytes || fixture;
  let base, getCalls = 0, getClosed = 0;
  const server = createServer(async (request, response) => {
    if (request.headers.authorization !== `Bearer ${token}`) { response.writeHead(401).end(); return; }
    if (request.url === "/mcp") {
      let body = ""; for await (const value of request) body += value;
      const rpc = JSON.parse(body);
      if (rpc.method === "notifications/initialized") { response.writeHead(202).end(); return; }
      let result;
      if (rpc.method === "initialize") result = { protocolVersion: "2025-06-18", capabilities: { tools: {} }, serverInfo: { name: "isolated-adapter-fixture", version: "1" } };
      else {
        assert.equal(rpc.params.name, "prepare_attachment_download");
        const reference = { account_id: "default", mailbox: "INBOX", uid: 7, uid_validity: 9, attachment_id: "att_v1_" + "1".repeat(64), attachment_index: 1 };
        const delivery = { available_modes: ["inline_base64", "authenticated_http"], authentication: "same_subject_bearer", expires_in_seconds: 600, expires_at: Math.floor(Date.now() / 1000) + (options.expired ? -1 : 600), client_delivery_required: true, next_action: "authenticated_get_then_import_file" };
        const size = options.size ?? bytes.length;
        const metadata = { ...reference, filename: "汇总 & # [验收] 100%.csv", safe_filename: "汇总 & # [验收] 100%.csv", content_type: "text/csv", declared_content_type: "text/csv", detected_content_type: null, encoded_size: size, decoded_size: options.unknownSize ? null : options.badSize ? size + 1 : size, size_accuracy: options.unknownSize ? "unknown" : "exact", disposition: "attachment", content_id: null, downloadable: true, unavailable_reason: null, checksum: null, delivery };
        result = { structuredContent: { data: { ...reference, download_url: options.crossOrigin ? "https://other.example.test/attachment" : base + "/attachment", method: "GET", required_headers: { Authorization: "Bearer <access_token>" }, expires_in_seconds: 600, metadata, delivery, content_trust: "untrusted" }, request_id: "synthetic" }, content: [], isError: false };
      }
      const wire = JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result });
      if (options.sse) {
        response.writeHead(200, { "Content-Type": "text/event-stream" }); response.write(`event: message\ndata: ${wire}\n\n`);
        if (!options.openSse) response.end();
      }
      else response.writeHead(200, { "Content-Type": "application/json" }).end(wire);
    } else if (request.url === "/attachment") {
      getCalls++;
      response.on("close", () => getClosed++);
      if (options.redirect) { response.writeHead(302, { Location: base + "/redirected" }).end(); return; }
      if (options.status) { response.writeHead(options.status).end(JSON.stringify({ error: { code: "NOT_FOUND", phase: "reference", action: "prepare_attachment_download" }, request_id: "synthetic" })); return; }
      const checksum = options.badHash ? "0".repeat(64) : options.sha256 || sha(bytes);
      response.writeHead(200, { "Content-Type": "application/octet-stream", "Cache-Control": "no-store", "X-Content-Type-Options": "nosniff", ...(options.omitDigest ? {} : { "X-Content-SHA256": checksum, "Content-Digest": `sha-256=:${Buffer.from(checksum, "hex").toString("base64")}:` }), ...(options.chunked ? {} : { "Content-Length": String(options.size ?? bytes.length) }) });
      if (options.hang) { response.write(bytes.subarray(0, 1)); return; }
      if (options.size) {
        const chunk = Buffer.alloc(CHUNK_BYTES, 120);
        Readable.from((async function* () { let left = options.size; while (left > 0) { yield chunk.subarray(0, Math.min(chunk.length, left)); left -= Math.min(chunk.length, left); } })()).pipe(response);
      } else response.end(bytes);
    } else response.writeHead(500).end();
  });
  await new Promise(done => server.listen(0, "127.0.0.1", done));
  base = `http://127.0.0.1:${server.address().port}`;
  try { return await run({ base, getCalls: () => getCalls, getClosed: () => getClosed }); }
  finally { server.closeAllConnections(); await new Promise(done => server.close(done)); }
}
const request = { uid: 7, uid_validity: 9, attachment_index: 1 };
const receive = (base, destination, more = {}) => receiveAttachment({ mcpUrl: base + "/mcp", bearerToken: token, request, destination, ...more });

test("standard MCP JSON/SSE and authenticated GET verify new R04 descriptor and preserve duplicate filenames", async () => {
  for (const sse of [false, true]) await temporary(root => fixtureServer({ sse, openSse: sse }, async ({ base }) => {
    const first = await receive(base, root), second = await receive(base, root);
    assert.equal(first.checksum_comparison, "matched_http_digest");
    assert.equal(first.analysis_status, "consumer_required");
    assert.notEqual(first.path, second.path);
    assert.match(first.path, /汇总 & # \[验收\] 100%\.csv$/);
    assert.deepEqual(await readFile(first.path), fixture);
    const rows = (await readFile(first.path, "utf8")).trim().split("\r\n").slice(1);
    assert.equal(rows.reduce((sum, row) => sum + Number(row.split(",")[2]), 0), 194);
  }));
});

test("standard client rejects checksum, size, expired lease, foreign origin and redirect with clean temporary files", async () => {
  for (const [options, reason] of [
    [{ badHash: true }, "CHECKSUM_MISMATCH"], [{ badSize: true }, "DESCRIPTOR_LENGTH_MISMATCH"],
    [{ expired: true }, "TRANSFER_EXPIRED"], [{ crossOrigin: true }, "ENDPOINT_POLICY"],
    [{ redirect: true }, "HTTP_FAILED"], [{ status: 404 }, "REFERENCE_EXPIRED_OR_MISSING"],
    [{ omitDigest: true }, "HTTP_DIGEST_REQUIRED"],
  ]) await temporary(root => fixtureServer(options, async ({ base }) => {
    await assert.rejects(receive(base, root), error => error.code === reason);
    assert.deepEqual(await readdir(root), []);
  }));
});

test("standard client handles 32MiB boundary, size preflight and midstream cancellation", async () => {
  const hash = createHash("sha256"); for (let i = 0; i < MAX_FILE_BYTES / CHUNK_BYTES; i++) hash.update(Buffer.alloc(CHUNK_BYTES, 120));
  await temporary(root => fixtureServer({ size: MAX_FILE_BYTES, sha256: hash.digest("hex") }, async ({ base }) => {
    const result = await receive(base, root); assert.equal(result.decoded_size, MAX_FILE_BYTES);
  }));
  await temporary(root => fixtureServer({ size: MAX_FILE_BYTES + 1 }, async ({ base, getCalls }) => {
    await assert.rejects(receive(base, root), error => error.code === "FILE_LIMIT"); assert.equal(getCalls(), 0);
    assert.deepEqual(await readdir(root), []);
  }));
  await temporary(root => fixtureServer({ size: MAX_FILE_BYTES + 1, unknownSize: true, chunked: true }, async ({ base }) => {
    await assert.rejects(receive(base, root), error => error.code === "FILE_LIMIT");
    assert.deepEqual(await readdir(root), []);
  }));
  await temporary(root => fixtureServer({ hang: true }, async ({ base }) => {
    const controller = new AbortController(); const timer = setTimeout(() => controller.abort(), 60);
    try { await assert.rejects(receive(base, root, { signal: controller.signal }), error => error.name === "AbortError"); }
    finally { clearTimeout(timer); }
    assert.deepEqual(await readdir(root), []);
  }));
  assert.equal(safeFilename("../../CON.txt"), "_CON.txt");
  assert.ok(Buffer.byteLength(safeFilename("中".repeat(200) + ".csv")) <= 200);
});

test("disk setup failure cancels a GET body before acquiring its reader", async () => {
  await temporary(async root => {
    const file = join(root, "occupied"); await writeFile(file, "existing");
    await fixtureServer({ hang: true }, async ({ base, getCalls, getClosed }) => {
      await assert.rejects(receive(base, file)); assert.equal(getCalls(), 1);
      for (let i = 0; i < 20 && getClosed() === 0; i++) await new Promise(done => setTimeout(done, 5));
      assert.equal(getClosed(), 1); assert.equal(await readFile(file, "utf8"), "existing");
      assert.deepEqual(await readdir(root), ["occupied"]);
    });
  });
});

function manifest(bytes = fixture) {
  return { transfer_id: "synthetic-transfer", account_id: "default", account_generation: "1", attachment_ref: "synthetic-attachment", decoded_size: bytes.length, sha256: sha(bytes), filename: "验收.csv", mime_type: "text/csv", expires_at_ms: Date.now() + 60_000 };
}
function mockedApp(bytes, item, mutate = value => value) {
  return {
    getHostCapabilities: () => ({ serverTools: {} }),
    callServerTool: async ({ name, arguments: args }) => {
      assert.equal(name, "read_attachment_delivery_chunk");
      const chunk = bytes.subarray(args.offset, args.offset + args.max_bytes);
      return { content: [], structuredContent: { status: "chunk_ready" }, _meta: { attachment_chunk: mutate({ ...item, offset: args.offset, next_offset: args.offset + chunk.length, base64: chunk.toString("base64") }) } };
    },
  };
}
test("UI mock assembles private bounded chunks, constructs File and keeps registration separate from analysis", async () => {
  const bytes = Buffer.concat([fixture, Buffer.alloc(128 * 1024, 120)]), item = manifest(bytes);
  const verified = await assembleFromUiTool(mockedApp(bytes, item), item);
  assert.equal(verified.chunk_calls, 3); assert.deepEqual(Buffer.from(verified.bytes), bytes);
  let uploads = 0;
  const registered = await registerGeneratedFile({ uploadFile: async file => { uploads++; assert.ok(file instanceof File); assert.equal(file.name, "验收.csv"); assert.equal(sha(Buffer.from(await file.arrayBuffer())), item.sha256); return { fileId: "synthetic-host-file" }; } }, item, verified);
  assert.equal(uploads, 1); assert.equal(registered.status, "host_file_registered"); assert.equal(registered.analysis_status, "target_acceptance_required");
  assert.equal((await registerGeneratedFile({}, item, verified)).status, "client_delivery_required");
  assert.equal((await registerGeneratedFile({ uploadFile: async () => { throw Error("synthetic"); } }, item, verified)).retry_allowed, false);
});

test("UI mock fails closed on binding/range/encoding/checksum/expiry/limit/cancel without registering", async () => {
  for (const [mutate, reason] of [
    [x => ({ ...x, account_id: "other" }), "CHUNK_BINDING_MISMATCH"],
    [x => ({ ...x, account_generation: "2" }), "CHUNK_BINDING_MISMATCH"],
    [x => ({ ...x, offset: 1 }), "CHUNK_RANGE_MISMATCH"],
    [x => ({ ...x, base64: "!".repeat(4) }), "CHUNK_ENCODING_INVALID"],
    [x => ({ ...x, base64: Buffer.alloc(fixture.length).toString("base64") }), "CHECKSUM_MISMATCH"],
  ]) { const item = manifest(); await assert.rejects(assembleFromUiTool(mockedApp(fixture, item, mutate), item), error => error.code === reason); }
  let item = { ...manifest(), expires_at_ms: 0 }; await assert.rejects(assembleFromUiTool(mockedApp(fixture, item), item), error => error.code === "TRANSFER_EXPIRED");
  item = { ...manifest(), decoded_size: MAX_FILE_BYTES + 1 }; await assert.rejects(assembleFromUiTool({}, item), error => error.code === "FILE_LIMIT");
  const controller = new AbortController(); controller.abort(); item = manifest();
  await assert.rejects(assembleFromUiTool(mockedApp(fixture, item), item, { signal: controller.signal }), error => error.name === "AbortError");
});

test("official ext-apps 2.0.4 schemas accept resource context/message shapes; host capability is separately detected", async () => {
  const resource = { type: "resource_link", uri: "attachment://synthetic/fixture", name: "验收.csv", mimeType: "text/csv", size: fixture.length };
  assert.ok(McpUiMessageRequestSchema.safeParse({ method: "ui/message", params: { role: "user", content: [resource] } }).success);
  assert.ok(McpUiUpdateModelContextRequestSchema.safeParse({ method: "ui/update-model-context", params: { content: [resource] } }).success);
  assert.equal((await offerResourceContext({}, resource)).status, "client_delivery_required");
  let calls = 0;
  const app = { getHostCapabilities: () => ({ updateModelContext: { resourceLink: {} } }), updateModelContext: async payload => { calls++; assert.deepEqual(payload.content, [resource]); return {}; } };
  const result = await offerResourceContext(app, resource);
  assert.equal(calls, 1); assert.equal(result.status, "context_acknowledged"); assert.equal(result.analysis_status, "target_acceptance_required");
});
