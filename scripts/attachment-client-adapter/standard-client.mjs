// Standalone client-side adapter. No product runtime, mail writes or token discovery.
import { createHash, randomUUID } from "node:crypto";
import { mkdir, mkdtemp, open, rename, unlink, rmdir, writeFile } from "node:fs/promises";
import { resolve, join, basename } from "node:path";
import { pathToFileURL } from "node:url";

export const MAX_FILE_BYTES = 32 * 1024 * 1024;
const MAX_WIRE_BYTES = 2 * 1024 * 1024;
export class AdapterError extends Error {
  constructor(code) { super(code); this.name = "AdapterError"; this.code = code; }
}
export function checkedUrl(value, origin) {
  let url;
  try { url = new URL(value); } catch { throw new AdapterError("INVALID_ENDPOINT"); }
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname);
  if ((url.protocol !== "https:" && !(url.protocol === "http:" && local)) || url.username || url.password || url.search || url.hash || (origin && url.origin !== origin)) {
    throw new AdapterError("ENDPOINT_POLICY");
  }
  return url;
}
function statusError(status) {
  return new AdapterError(status === 401 ? "AUTH_REQUIRED" : status === 404 ? "REFERENCE_EXPIRED_OR_MISSING" : status === 429 ? "BUSY" : "HTTP_FAILED");
}
async function boundedBytes(response, limit) {
  if (!response.body) throw new AdapterError("EMPTY_RESPONSE");
  const chunks = []; let total = 0;
  const reader = response.body.getReader();
  try {
    while (true) {
      const { value, done } = await reader.read(); if (done) break;
      total += value.byteLength;
      if (total > limit) throw new AdapterError("RESPONSE_LIMIT");
      chunks.push(value);
    }
  } finally { await reader.cancel().catch(() => {}); }
  return Buffer.concat(chunks, total);
}
async function mcpResponse(response, id) {
  if (!response.headers.get("Content-Type")?.startsWith("text/event-stream")) {
    let message;
    try { message = JSON.parse((await boundedBytes(response, MAX_WIRE_BYTES)).toString("utf8")); }
    catch (error) { if (error instanceof AdapterError) throw error; throw new AdapterError("INVALID_MCP_RESPONSE"); }
    return message.id === id ? message : undefined;
  }
  if (!response.body) throw new AdapterError("EMPTY_RESPONSE");
  const reader = response.body.getReader(), decoder = new TextDecoder();
  let pending = "", total = 0;
  try {
    while (true) {
      const { value, done } = await reader.read(); if (done) break;
      total += value.byteLength;
      if (total > MAX_WIRE_BYTES) throw new AdapterError("RESPONSE_LIMIT");
      pending += decoder.decode(value, { stream: true });
      let boundary;
      while ((boundary = /\r?\n\r?\n/.exec(pending))) {
        const event = pending.slice(0, boundary.index); pending = pending.slice(boundary.index + boundary[0].length);
        const data = event.split(/\r?\n/).filter(line => line.startsWith("data:")).map(line => line.slice(5).trimStart()).join("\n");
        if (!data) continue;
        let message; try { message = JSON.parse(data); } catch { throw new AdapterError("INVALID_MCP_RESPONSE"); }
        if (message.id === id) return message;
      }
    }
  } finally { await reader.cancel().catch(() => {}); }
}
export function safeFilename(value) {
  const name = basename(String(value || "attachment.bin").replaceAll("\\", "/"))
    .replace(/[<>:"/\\|?*\x00-\x1f\x7f]/g, "_").replace(/[. ]+$/g, "");
  let safe = "";
  for (const c of Array.from(name || "attachment.bin").slice(0, 120)) {
    if (Buffer.byteLength(safe + c) > 200) break;
    safe += c;
  }
  return /^(?:con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)/i.test(safe) ? "_" + safe : safe;
}
function headerFilename(value) {
  const encoded = /filename\*=UTF-8''([^;]+)/i.exec(value || "");
  if (encoded) { try { return decodeURIComponent(encoded[1]); } catch { /* metadata fallback */ } }
  return /filename="([^"]*)"/i.exec(value || "")?.[1];
}
export async function receiveAttachment({ mcpUrl, bearerToken, request, destination, expectedSha256, signal }) {
  const endpoint = checkedUrl(mcpUrl);
  if (typeof destination !== "string" || !destination || !request || typeof request !== "object" || Array.isArray(request)) throw new AdapterError("INVALID_CLIENT_INPUT");
  const root = resolve(destination);
  if (!bearerToken || typeof bearerToken !== "string") throw new AdapterError("AUTH_REQUIRED");
  if (expectedSha256 !== undefined && !/^[0-9a-f]{64}$/.test(expectedSha256)) throw new AdapterError("INVALID_EXPECTED_DIGEST");
  const combined = AbortSignal.any([signal, AbortSignal.timeout(60_000)].filter(Boolean));
  let version = "2025-06-18", session;
  async function rpc(method, params, notification = false) {
    const headers = { Authorization: `Bearer ${bearerToken}`, "Content-Type": "application/json", Accept: "application/json, text/event-stream", "MCP-Protocol-Version": version };
    if (session) headers["Mcp-Session-Id"] = session;
    const id = randomUUID();
    const response = await fetch(endpoint, { method: "POST", headers, redirect: "manual", signal: combined, body: JSON.stringify({ jsonrpc: "2.0", ...(notification ? {} : { id }), method, params }) });
    if (!response.ok) { await response.body?.cancel(); throw statusError(response.status); }
    session = response.headers.get("Mcp-Session-Id") || session;
    if (notification && response.status === 202) { await response.body?.cancel(); return; }
    const message = await mcpResponse(response, id);
    if (!message || message.error || !message.result) throw new AdapterError("MCP_FAILED");
    return message.result;
  }
  const init = await rpc("initialize", { protocolVersion: version, capabilities: {}, clientInfo: { name: "attachment-client-adapter", version: "0.1.0" } });
  if (!["2025-06-18", "2025-11-25"].includes(init.protocolVersion)) throw new AdapterError("PROTOCOL_UNSUPPORTED");
  version = init.protocolVersion;
  await rpc("notifications/initialized", {}, true);
  const result = await rpc("tools/call", { name: "prepare_attachment_download", arguments: request });
  const envelope = result.structuredContent;
  if (result.isError || envelope?.error || !envelope?.data) throw new AdapterError("PREPARE_FAILED");
  const data = envelope.data;
  if (data.method !== "GET") throw new AdapterError("DELIVERY_UNSUPPORTED");
  const metadata = data.metadata;
  if (!metadata || metadata.downloadable !== true || data.delivery?.authentication !== "same_subject_bearer" || !data.delivery.available_modes?.includes("authenticated_http")) throw new AdapterError("R04_DESCRIPTOR_REQUIRED");
  for (const field of ["uid", "uid_validity", "attachment_id", "attachment_index"]) {
    if (request[field] !== undefined && metadata[field] !== request[field]) throw new AdapterError("REFERENCE_MISMATCH");
  }
  if (metadata.account_id !== (request.account_id || "default") || (metadata.mailbox !== (request.mailbox || "INBOX") && !(metadata.mailbox?.toUpperCase() === "INBOX" && (request.mailbox || "INBOX").toUpperCase() === "INBOX"))) throw new AdapterError("REFERENCE_MISMATCH");
  if (Number.isInteger(data.delivery.expires_at) && data.delivery.expires_at <= Date.now() / 1000) throw new AdapterError("TRANSFER_EXPIRED");
  const descriptorSha = metadata.checksum?.algorithm === "sha256" && /^[0-9a-f]{64}$/.test(metadata.checksum.value) ? metadata.checksum.value : undefined;
  if (metadata.checksum !== null && !descriptorSha) throw new AdapterError("INVALID_DESCRIPTOR_DIGEST");
  if (metadata.size_accuracy === "exact" && (!Number.isSafeInteger(metadata.decoded_size) || metadata.decoded_size < 0 || metadata.decoded_size > MAX_FILE_BYTES)) throw new AdapterError("FILE_LIMIT");
  const download = checkedUrl(data.download_url, endpoint.origin);
  const response = await fetch(download, { headers: { Authorization: `Bearer ${bearerToken}` }, redirect: "manual", signal: combined });
  let file, reader, directory, temporary, promoted = false;
  try {
    if (!response.ok) throw statusError(response.status);
    const declared = response.headers.get("Content-Length");
    if (declared && (!/^\d+$/.test(declared) || Number(declared) > MAX_FILE_BYTES)) throw new AdapterError("FILE_LIMIT");
    const httpSha = response.headers.get("X-Content-SHA256");
    const httpDigest = response.headers.get("Content-Digest");
    if (!/^[0-9a-f]{64}$/.test(httpSha || "") || httpDigest !== `sha-256=:${Buffer.from(httpSha, "hex").toString("base64")}:`) throw new AdapterError("HTTP_DIGEST_REQUIRED");
    if (!response.body) throw new AdapterError("EMPTY_RESPONSE");
    await mkdir(root, { recursive: true }); combined.throwIfAborted();
    directory = await mkdtemp(join(root, "attachment-"));
    temporary = join(directory, "download.part");
    file = await open(temporary, "wx", 0o600);
    reader = response.body.getReader();
    const hash = createHash("sha256"); let total = 0;
    while (true) {
      const { value, done } = await reader.read(); if (done) break;
      total += value.byteLength;
      if (total > MAX_FILE_BYTES) throw new AdapterError("FILE_LIMIT");
      hash.update(value);
      await file.writeFile(value);
    }
    if (declared && Number(declared) !== total) throw new AdapterError("LENGTH_MISMATCH");
    if (metadata.size_accuracy === "exact" && metadata.decoded_size !== total) throw new AdapterError("DESCRIPTOR_LENGTH_MISMATCH");
    const checksum = hash.digest("hex");
    if (checksum !== httpSha) throw new AdapterError("CHECKSUM_MISMATCH");
    if (expectedSha256 && checksum !== expectedSha256) throw new AdapterError("CHECKSUM_MISMATCH");
    if (descriptorSha && checksum !== descriptorSha) throw new AdapterError("CHECKSUM_MISMATCH");
    await file.sync(); await file.close(); file = undefined;
    const filename = safeFilename(metadata.safe_filename || headerFilename(response.headers.get("Content-Disposition")) || metadata.filename);
    const target = join(directory, filename);
    await rename(temporary, target); promoted = true;
    return { status: "local_file_ready", path: target, sha256: checksum, decoded_size: total, checksum_comparison: "matched_http_digest", verified_against: ["X-Content-SHA256", "Content-Digest", ...(descriptorSha ? ["descriptor_checksum"] : []), ...(expectedSha256 ? ["expected_checksum"] : [])], declared_content_type: metadata.content_type || null, analysis_status: "consumer_required" };
  } finally {
    if (reader) await reader.cancel().catch(() => {});
    else await response.body?.cancel().catch(() => {});
    await file?.close().catch(() => {});
    if (!promoted) {
      if (temporary) await unlink(temporary).catch(() => {});
      if (directory) await rmdir(directory).catch(() => {});
    }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  if (process.argv.includes("--help")) {
    console.log("Set ADAPTER_MCP_URL, ADAPTER_BEARER_TOKEN, ADAPTER_REQUEST_JSON, ADAPTER_DESTINATION; optional ADAPTER_EXPECTED_SHA256 and ADAPTER_REPORT. No credentials in arguments.");
  } else {
    try {
      const result = await receiveAttachment({ mcpUrl: process.env.ADAPTER_MCP_URL, bearerToken: process.env.ADAPTER_BEARER_TOKEN, request: JSON.parse(process.env.ADAPTER_REQUEST_JSON || "{}"), destination: process.env.ADAPTER_DESTINATION, expectedSha256: process.env.ADAPTER_EXPECTED_SHA256 });
      if (process.env.ADAPTER_REPORT) await writeFile(process.env.ADAPTER_REPORT, JSON.stringify(result, null, 2) + "\n", { mode: 0o600 });
      console.log(JSON.stringify({ status: result.status, decoded_size: result.decoded_size, sha256: result.sha256, analysis_status: result.analysis_status }));
    } catch (error) {
      console.error(JSON.stringify({ status: "client_delivery_required", reason: error instanceof AdapterError ? error.code : error?.name === "AbortError" ? "CANCELLED" : "CLIENT_FAILED" })); process.exitCode = 1;
    }
  }
}
