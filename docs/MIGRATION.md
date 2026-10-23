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

- tokio + axum (server), reqwest (upstream calls, streaming)
- serde_json + `map[string]any` semantics: preserve unknown fields
- rusqlite (bundled sqlite) for the 11 repos from `src/lib/db`
- jsonwebtoken (HS256 dashboard sessions, same guard path rules)
- rust-embed / tower-http ServeDir for the dashboard bundle

## Phases (gradual, each leaves a runnable binary)

- **Phase 0 — done.** Workspace, `/health`, `/v1/models` (registry data),
  `/v1/chat/completions` passthrough to a configured upstream.
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
  (replaces the spawned-runtime hack: the bridge logic becomes native GateForge
  providers).
- **Verification rule** (every phase): run 9router JS and GateForge side-by-side on
  the same requests and diff the SSE/JSON output.

## Data migration

On first boot: if `%APPDATA%/9router/db/data.sqlite` exists and GateForge's store is
empty, copy it (connections, keys, settings) — users keep their providers and quota.
