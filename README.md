# GateForge

**GateForge** is a self-hosted AI gateway — the Rust port of
[9Router](https://github.com/decolua/9router): 100+ LLM providers behind one
OpenAI-compatible endpoint, with combos, failover, multi-account rotation and
per-model quota tracking.

- Single static Rust binary; the React dashboard is served as prebuilt static files.
- Provider registry and model catalogs are **data** (extracted from the 9router JS
  registry), never hand-typed.
- Behavioral reference port: [TheOwlOps/proxygo](https://github.com/TheOwlOps/proxygo) (Go).

## Status

Phase 0 (scaffold) — see [docs/MIGRATION.md](docs/MIGRATION.md) for the full
gradual-migration plan from 9router.

## Run

```bash
cargo run -p gateway
# PORT=20228 UPSTREAM_BASE_URL=... UPSTREAM_API_KEY=... to configure
```

- `GET /health` — liveness + version
- `GET /v1/models` — registry model list (`provider/model`)
- `POST /v1/chat/completions` — direct dispatch or passthrough to the configured upstream
- `POST /v1/messages` — Claude format, translated both ways

## ZCode Start Plan bridge

`zcode/glm-5.3-flash` and `zcode/glm-5.3` are served through the locally
installed ZCode app: GateForge spawns a bundled node bridge client that drives
the ZCode runtime over stdio (server dist at `~/.zcode/server/` preferred,
desktop CLI app-server as fallback). Model calls are made by the genuine ZCode
process, so Start Plan quota works without touching the Z.ai gateway's
WAF/captcha material. No credentials are stored — the shared local ZCode
profile owns auth; a `node` runtime must be on PATH.

## Extract the provider registry

```bash
node tools/extract-registry.mjs <path-to-9router-checkout>
# -> crates/providers/registry.json
```
