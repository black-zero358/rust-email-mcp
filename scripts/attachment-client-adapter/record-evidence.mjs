// Save only synthetic test summaries and hashes of public documentation snapshots.
import { execFileSync } from "node:child_process";
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { createHash } from "node:crypto";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
const root = fileURLToPath(new URL(".", import.meta.url));
const tap = execFileSync(process.execPath, ["--test", "--test-reporter=tap", "adapter.test.mjs"], { cwd: root, encoding: "utf8", maxBuffer: 1024 * 1024 });
const count = name => Number(new RegExp(`^# ${name} (\\d+)$`, "m").exec(tap)?.[1]);
if (!Number.isInteger(count("tests")) || count("fail") !== 0) throw Error("Synthetic test evidence was not successful");
const sources = [];
for (const [file, url] of [
  ["openai-reference.md", "https://developers.openai.com/plugins/reference.md"],
  ["openai-ui.md", "https://developers.openai.com/plugins/build/chatgpt-ui.md"],
  ["spec.types.ts", "https://raw.githubusercontent.com/modelcontextprotocol/ext-apps/v2.0.4/src/spec.types.ts"],
  ["app.ts", "https://raw.githubusercontent.com/modelcontextprotocol/ext-apps/v2.0.4/src/app.ts"],
  ["apps.mdx", "https://raw.githubusercontent.com/modelcontextprotocol/ext-apps/v2.0.4/specification/2026-01-26/apps.mdx"],
  ["migrate.md", "https://raw.githubusercontent.com/modelcontextprotocol/ext-apps/v2.0.4/docs/migrate_from_openai_apps.md"],
]) {
  try { const data = await readFile(join(root, "generated/research", file)); sources.push({ url, bytes: data.length, sha256: createHash("sha256").update(data).digest("hex") }); }
  catch (error) { if (error.code !== "ENOENT") throw error; }
}
const report = {
  recorded_on: "2026-11-02", node: process.version, platform: process.platform,
  command: "pnpm exec node --test --test-reporter=tap adapter.test.mjs",
  tests: count("tests"), passed: count("pass"), failed: count("fail"),
  scope: "local synthetic HTTP fixture, mocked UI host, official ext-apps 2.0.4 schemas",
  phase_exit: "offline_synthetic_download_and_content_reading_passed",
  external_connection_testing: "deferred; not part of this phase",
  stages_verified: ["local_file_ready", "mock_host_file_registered", "mock_context_acknowledged"],
  sources,
  linux_node_development_tool: { version: "24.15.0", source: "https://nodejs.org/dist/v24.15.0/node-v24.15.0-linux-x64.tar.xz", verification: "official SHASUMS256.txt: sha256sum --check passed", sha256: "472655581fb851559730c48763e0c9d3bc25975c59d518003fc0849d3e4ba0f6", location: "ignored generated directory; no system installation" },
};
await mkdir(join(root, "evidence"), { recursive: true });
await writeFile(join(root, "evidence/local-prototype-2026-11-02.json"), JSON.stringify(report, null, 2) + "\n");
console.log(JSON.stringify({ tests: report.tests, passed: report.passed, failed: report.failed, source_snapshots: sources.length }));
