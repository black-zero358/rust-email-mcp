// Browser-only candidate: official bridge methods plus proposed app-only chunk tool.
// Tool names below are design proposals, not existing Rust server capabilities.
export const MAX_FILE_BYTES = 32 * 1024 * 1024;
export const CHUNK_BYTES = 64 * 1024;
export class CandidateError extends Error {
  constructor(code) { super(code); this.name = "CandidateError"; this.code = code; }
}
const requireState = (condition, code) => { if (!condition) throw new CandidateError(code); };
const hex = data => Array.from(new Uint8Array(data), byte => byte.toString(16).padStart(2, "0")).join("");
const absent = reason => ({ status: "client_delivery_required", reason, analysis_status: "target_acceptance_required" });
function validManifest(manifest) {
  requireState(manifest && Number.isSafeInteger(manifest.decoded_size) && manifest.decoded_size >= 0 && manifest.decoded_size <= MAX_FILE_BYTES, "FILE_LIMIT");
  for (const name of ["transfer_id", "account_id", "account_generation", "attachment_ref"]) {
    requireState(typeof manifest[name] === "string" && manifest[name].length > 0 && manifest[name].length <= 256, "INVALID_BINDING");
  }
  requireState(/^[0-9a-f]{64}$/.test(manifest.sha256), "INVALID_DIGEST");
  requireState(Number.isSafeInteger(manifest.expires_at_ms), "INVALID_EXPIRY");
  requireState(typeof manifest.filename === "string" && manifest.filename.length <= 255 && !/[\x00-\x1f\x7f/\\]/.test(manifest.filename), "INVALID_FILENAME");
  requireState(typeof manifest.mime_type === "string" && manifest.mime_type.length <= 200 && /^[a-z0-9.+-]+\/[a-z0-9.+-]+$/i.test(manifest.mime_type), "INVALID_MIME");
}
export async function assembleFromUiTool(app, manifest, { signal, now = Date.now } = {}) {
  validManifest(manifest);
  if (!app?.getHostCapabilities?.()?.serverTools || typeof app.callServerTool !== "function") return absent("HOST_TOOL_PROXY_REQUIRED");
  const bytes = new Uint8Array(manifest.decoded_size);
  let offset = 0, calls = 0;
  try {
    while (offset < bytes.byteLength) {
      signal?.throwIfAborted();
      requireState(now() < manifest.expires_at_ms, "TRANSFER_EXPIRED");
      let result;
      for (let attempt = 0; attempt < 2; attempt++) {
        try {
          calls++;
          result = await app.callServerTool({ name: "read_attachment_delivery_chunk", arguments: { transfer_id: manifest.transfer_id, offset, max_bytes: CHUNK_BYTES } }, { signal });
          break;
        } catch (error) {
          signal?.throwIfAborted();
          if (attempt === 1) throw new CandidateError("CHUNK_TRANSPORT_FAILED");
        }
      }
      signal?.throwIfAborted();
      requireState(!result?.isError, "CHUNK_TOOL_FAILED");
      const chunk = result?._meta?.attachment_chunk;
      requireState(chunk && typeof chunk === "object", "PRIVATE_CHUNK_MISSING");
      for (const name of ["transfer_id", "account_id", "account_generation", "attachment_ref", "decoded_size", "sha256"]) {
        requireState(chunk[name] === manifest[name], "CHUNK_BINDING_MISMATCH");
      }
      requireState(chunk.offset === offset && Number.isSafeInteger(chunk.next_offset) && chunk.next_offset > offset && chunk.next_offset <= bytes.byteLength && chunk.next_offset - offset <= CHUNK_BYTES, "CHUNK_RANGE_MISMATCH");
      requireState(typeof chunk.base64 === "string" && chunk.base64.length <= 4 * Math.ceil(CHUNK_BYTES / 3) && chunk.base64.length % 4 === 0 && /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(chunk.base64), "CHUNK_ENCODING_INVALID");
      const decoded = atob(chunk.base64);
      requireState(decoded.length === chunk.next_offset - offset, "CHUNK_LENGTH_MISMATCH");
      for (let index = 0; index < decoded.length; index++) bytes[offset + index] = decoded.charCodeAt(index);
      offset = chunk.next_offset;
    }
    signal?.throwIfAborted();
    requireState(now() < manifest.expires_at_ms, "TRANSFER_EXPIRED");
    requireState(hex(await crypto.subtle.digest("SHA-256", bytes)) === manifest.sha256, "CHECKSUM_MISMATCH");
    return { status: "bytes_verified", bytes, chunk_calls: calls, sha256: manifest.sha256, decoded_size: bytes.byteLength };
  } catch (error) {
    bytes.fill(0);
    throw error;
  }
}

// Call from an explicit experiment/user action. Returning a fileId is one stage;
// the target host must separately attach that file to its analysis environment.
export async function registerGeneratedFile(host, manifest, verified, { signal } = {}) {
  validManifest(manifest);
  if (typeof host?.uploadFile !== "function" || typeof File !== "function") return absent("HOST_UPLOAD_REQUIRED");
  requireState(verified?.status === "bytes_verified" && verified.bytes instanceof Uint8Array && verified.bytes.byteLength === manifest.decoded_size && verified.sha256 === manifest.sha256, "VERIFIED_BYTES_REQUIRED");
  signal?.throwIfAborted();
  requireState(hex(await crypto.subtle.digest("SHA-256", verified.bytes)) === manifest.sha256, "CHECKSUM_MISMATCH");
  const file = new File([verified.bytes], manifest.filename, { type: manifest.mime_type });
  let result;
  try { result = await host.uploadFile(file); }
  catch { return { ...absent("HOST_UPLOAD_OUTCOME_UNKNOWN"), registration_status: "unknown", retry_allowed: false }; }
  if (typeof result?.fileId !== "string" || !result.fileId || result.fileId.length > 256) return { ...absent("HOST_UPLOAD_RESULT_INVALID"), registration_status: "unknown", retry_allowed: false };
  return { status: "host_file_registered", fileId: result.fileId, sha256: manifest.sha256, decoded_size: manifest.decoded_size, analysis_status: "target_acceptance_required", client_delivery_required: true, next_action: "attach_registered_file_using_verified_host_interface" };
}

// Opt-in standard resource-context experiment; no chat message is sent here.
export async function offerResourceContext(app, resource, { signal } = {}) {
  if (!app?.getHostCapabilities?.()?.updateModelContext?.resourceLink || typeof app.updateModelContext !== "function") return absent("HOST_RESOURCE_CONTEXT_REQUIRED");
  requireState(resource?.type === "resource_link" && typeof resource.uri === "string" && resource.uri.length <= 2048 && !/^https?:/i.test(resource.uri), "SERVER_RESOURCE_URI_REQUIRED");
  const result = await app.updateModelContext({ content: [resource] }, { signal });
  if (result?.isError) return absent("HOST_RESOURCE_CONTEXT_REJECTED");
  return { status: "context_acknowledged", analysis_status: "target_acceptance_required", client_delivery_required: true };
}
