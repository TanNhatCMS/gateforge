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
cargo run -p gf-gateway
# PORT=20228 UPSTREAM_BASE_URL=... UPSTREAM_API_KEY=... to configure
```

- `GET /health` — liveness + version
- `GET /v1/models` — registry model list (`provider/model`)
- `POST /v1/chat/completions` — passthrough to the configured upstream (phase 0)

## Extract the provider registry

```bash
node tools/extract-registry.mjs <path-to-9router-checkout>
# -> crates/gf-providers/registry.json
```
