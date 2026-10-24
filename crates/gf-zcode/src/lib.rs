// ZCode server dist client — placeholder for the native stdio bridge.
//
// The standalone server (~/.zcode/server/zcode-server.cjs) speaks a JSON
// hello followed by either NDJSON or a binary framed protocol. The exact
// format needs live probing — the OmniRoute Go port used binary frames, but
// the CLI app-server uses NDJSON. This module will be completed when the
// protocol is confirmed against a live server.
//
// In the meantime, the zcode provider falls back to the 9router passthrough
// (which serves Start Plan via the 9router's own zcode executor).
