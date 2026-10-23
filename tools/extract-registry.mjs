// Registry extraction: dynamic-import the 9router JS registry files and dump
// their default exports as one JSON document (the same trick ProxyGo used).
//
// Usage: node tools/extract-registry.mjs <path-to-9router-repo> [outFile]
// Defaults: <repo>/open-sse/providers/registry → registry.json at repo root.
import { readdirSync, writeFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";

const repoArg = process.argv[2];
if (!repoArg) {
  console.error("usage: node tools/extract-registry.mjs <path-to-9router-repo> [outFile]");
  process.exit(1);
}
const registryDir = resolve(repoArg, "open-sse", "providers", "registry");
const outFile = process.argv[3]
  ? resolve(process.argv[3])
  : resolve("crates", "gf-providers", "registry.json");

const files = readdirSync(registryDir)
  .filter((f) => f.endsWith(".js") && f !== "index.js")
  .sort();

// GitHub push protection flags Google OAuth client IDs even though a web-app
// client_id is public by design. Redact the suffix everywhere — nothing in the
// Rust registry needs the exact value.
const GOOGLE_CLIENT_ID = /[A-Za-z0-9_-]+\.apps\.googleusercontent\.com/g;

function redact(value) {
  if (typeof value === "string") return value.replace(GOOGLE_CLIENT_ID, "[redacted-google-client-id]");
  if (Array.isArray(value)) return value.map(redact);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, redact(v)]));
  }
  return value;
}

const providers = [];
const failures = [];
for (const file of files) {
  const url = `file:///${join(registryDir, file).replace(/\\/g, "/")}`;
  try {
    const mod = await import(url);
    const entry = redact(mod.default);
    if (!entry || typeof entry !== "object" || !entry.id) {
      failures.push({ file, reason: "no default export with id" });
      continue;
    }
    providers.push(entry);
  } catch (error) {
    failures.push({ file, reason: String(error?.message || error).slice(0, 200) });
  }
}

providers.sort((a, b) => (a.priority ?? 999) - (b.priority ?? 999) || a.id.localeCompare(b.id));
writeFileSync(outFile, JSON.stringify({ providers }, null, 2) + "\n", "utf8");

console.log(`extracted ${providers.length} providers -> ${outFile}`);
for (const failure of failures) {
  console.warn(`  FAILED: ${failure.file}: ${failure.reason}`);
}
if (failures.length > 0) process.exitCode = 2;
