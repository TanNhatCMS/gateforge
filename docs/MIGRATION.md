# GateForge — migration plan (9router JS → Rust)

Source of truth: https://github.com/decolua/9router (JS/Next.js, v0.5.109 on `dev`).
Reference port: https://github.com/TheOwlOps/proxygo (Go port of v0.5.104 — survey,
contracts and phase discipline are reused here; 222 Go files, full backend).

## Measured size of the original (from the proxygo survey @ v0.5.104)

| Part | Files | LOC | Notes |
|---|---|---|---|
| `open-sse/` | 418 | 60,200 | routing engine: 145 provider registries, executors, translator, RTK, usage |
| `src/app/api/` | 179 | 18,617 | dashboard management APIs |
| `src/lib/` | 133 | 17,580 | sqlite layer, oauth services, tunnel, updater, MITM |
| dashboard UI | ~200 | ~40,000 | React 19 + Next — **kept as prebuilt static, never rewritten** |
| tests | 394 | 55,681 | Vitest |

## The one big decision (same as ProxyGo)

The React dashboard is **kept as a prebuilt static bundle** and served by the Rust
binary. Only the backend moves to Rust. Registry/model catalogs are **data**
(extracted from the JS registry by `tools/extract-registry.mjs`), never hand-typed.

## Stack

- **Ship as a Tauri v2 desktop app**: the Rust gateway core is the Tauri
  backend; the dashboard runs in a webview window (the local /v1 port stays
  open for CLI tools — same contract as 9router).
- tokio + axum (server), reqwest (upstream calls, streaming)
- serde_json + `map[string]any` semantics: preserve unknown fields
- rusqlite (bundled sqlite) for the 11 repos from `src/lib/db`
- jsonwebtoken (HS256 dashboard sessions, same guard path rules)
- Dashboard bundle served to the webview (rust-embed / tower-http ServeDir)

## Phases (gradual, each leaves a runnable binary)

- **Phase 0 — done.** Workspace, `/health`, `/` status page, `/v1/models` (registry data),
  `/v1/chat/completions` passthrough to a configured upstream — **verified live
  end-to-end through the 9router dev instance (Start Plan served)**.
- **Known issue (Tauri shell):** the window opens but the embedded gateway listener
  stops shortly after bind (serve task aborted when the webview loads the dead URL
  before bind — lifecycle race). Workaround until fixed: run the standalone
  `gateforge` binary (`start.cmd`); the Tauri shell then points its webview at the
  already-live port. Proper fix = serve the dashboard FROM the gateway and create
  the window after the bind event.
- **Phase 1 — translator**: openai↔claude request/response/stream pairs (table-driven
  tests with fixtures from 9router `tests/`), OpenAI pivot for the rest.
- **Phase 2 — store**: rusqlite + migrations ported from `src/lib/db`
  (connections, apiKeys, combos, settings, usage, proxyPools…), DATA_DIR rules kept.
- **Phase 3 — chatcore**: format detect → translate → dispatch → retry →
  next-account fallback → combo expansion; credential resolution via the store.
- **Phase 4 — executors**: default OpenAI-compatible + special ones
  (cursor protobuf, kiro EventStream, commandcode NDJSON, bedrock/vertex sigv4),
  ported incrementally with the 9router JS as behavioral reference.
- **Phase 5 — dashboard APIs**: the ~179 `/api/*` routes (providers, keys, combos,
  settings, oauth, cli-tools…), dashboardGuard port, WebLogin capture flow.
- **Phase 6 — extras**: proxy pools, MITM, tunnel, self-update, zcode stdio bridge
  (**done for the bridge**: native `zcode` crate resolves spawn targets, pins the
  Start Plan selection, decrypts the shared zcode JWT, and exchanges one JSON
  line with a bundled node bridge client `assets/zcode-bridge-client.cjs` that
  drives the runtime — server dist binary framing preferred, desktop CLI
  app-server NDJSON as fallback). Node hosts the protocol client because the
  server dist closes its stdio mid-handshake when the client process is not
  node — verified against node clients with byte-identical frames succeeding
  where the Rust client got EOF; the binary frame codec stays in Rust as the
  tested wire-format reference. Verified live end-to-end: Start Plan
  GLM-5.3-Flash through `/v1/chat/completions` (JSON + SSE) and `/v1/messages`.
- **Verification rule** (every phase): run 9router JS and GateForge side-by-side on
  the same requests and diff the SSE/JSON output.

## Data migration

On first boot: if `%APPDATA%/9router/db/data.sqlite` exists and GateForge's store is
empty, copy it (connections, keys, settings) — users keep their providers and quota.
