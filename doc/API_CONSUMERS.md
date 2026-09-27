# P25 HTTPD — API consumer contract

Short governance doc. Introduced with the Stage 2 API-first split
(2026-04-17). See also [`P25_API.md`](P25_API.md) for the endpoint
catalogue.

## The rule

**The HTTP + WebSocket API is the only supported way to interact with
the P25 daemon.** The embedded dashboard, the Python diagnostic tools
under [`tools/`](../tools/), and any future consumer (Android app, CLI,
third-party panel) are equals. None may reach into daemon-private
state.

In practice that means:

- No private sidecar protocols, no devmem shortcuts, no SSH-only
  control paths that the API doesn't mirror. If a feature needs a
  non-HTTP path, add the HTTP endpoint first.
- The web UI modules under [`p25-httpd/src/httpd/ui/`](../p25-httpd/src/httpd/ui/)
  only use `fetch()` against `/api/*` and WebSocket against `/ws/*`.
  It has no special visibility into daemon internals.
- Diagnostic tools under `tools/` do the same: they hit the same
  endpoints any external client would. This is deliberate — it keeps
  tools runnable from a laptop against the board without ssh.

## Why this matters

1. **Android-app parity.** Every feature the dashboard can do, the
   Android app can do, because there's exactly one API. No "that's
   dashboard-only" features.
2. **Debuggability.** A user reporting a bug can `curl` the same
   endpoint we'd hit internally. No mystery about what state is
   visible.
3. **Refactor safety.** We can move code, split modules (Stage 2 did
   exactly this), or swap the daemon's internals without breaking
   consumers, because the contract is the JSON shape on the wire.

## Authoritative sources

| Source | What it defines |
|---|---|
| [`p25-httpd/src/httpd/api/system.rs`](../p25-httpd/src/httpd/api/system.rs) `ENDPOINT_CATALOGUE` | Live endpoint list. Served via `GET /api/endpoints` |
| [`p25-httpd/p25-json/src/lib.rs`](../p25-httpd/p25-json/src/lib.rs) | Typed JSON shapes (`SystemInfo`, `ChannelGrant`, `BandInfo`, `DecoderStats`, `AliasMap`, `TsbkEvent`, …) |
| [`doc/P25_API.md`](P25_API.md) | Human-readable reference, updated per Stage 2. Ephemeral — trust the catalogue endpoint over the doc if they disagree |

## Rules for adding an endpoint

1. Place the handler in the appropriate [`api/<module>.rs`](../p25-httpd/src/httpd/api/) by consumer-facing category (see `P25_API.md` groupings). If it spans categories, pick the one that matches the primary user-visible screen.
2. Register it in [`httpd/mod.rs`](../p25-httpd/src/httpd/mod.rs) `router()`.
3. Add an entry to `ENDPOINT_CATALOGUE` in [`api/system.rs`](../p25-httpd/src/httpd/api/system.rs) in the **same commit**. The catalogue is the Android app's discovery mechanism — it MUST stay in sync.
4. If the response shape is non-trivial, add a typed struct to `p25-json` and return `Json<YourStruct>` instead of `Json<serde_json::Value>`. Typed responses are self-documenting and survive refactors.
5. If it's a WebSocket endpoint, implement `Lagged` handling as in [`api/ws.rs`](../p25-httpd/src/httpd/api/ws.rs) — a slow consumer must never take down the connection.
6. Update [`doc/P25_API.md`](P25_API.md).
7. Bump `BUILD_TAG` in [`p25-httpd/src/main.rs`](../p25-httpd/src/main.rs) so consumers can tell which version is responding.

## Rules for adding a consumer

1. Hit `GET /api/endpoints` on startup to discover the available endpoints. Don't hardcode paths against a stale copy of `P25_API.md`.
2. Read `GET /api/system.build` so logs show which daemon build the consumer is talking to.
3. Handle WebSocket `Lagged` markers (`event_type: "ws_lag"` on `/ws/events`, `{"type":"lag"}` text frame on `/ws/audio`). Don't reconnect on lag — the server stays connected and sends these as a signal to flush local buffers.
4. Use exponential backoff for WebSocket reconnect (1s → 15s ceiling). See the dashboard's `connectWs()` pattern for reference.
5. Default target is `http://192.168.2.1:8080` (RNDIS-over-USB) or `http://192.168.120.50:8080` (wired Ethernet). No auth — the radio is on a private subnet.
6. Consumers MAY poll `/api/stats` and `/api/sys_health` at 1 Hz without concern. Anything beyond that, profile the daemon first.

## What is NOT guaranteed

- **Response shape stability across major phases.** Phase 9 removed the `ps_iq_lsm` / `ps_phase6d` sections from `/api/decoder_compare`. Phase 10 added the `agc_enabled` bit to the LSM control endpoints. Major changes are called out in [`CHANGELOG_FORK.md`](../CHANGELOG_FORK.md).
- **Persistent state across daemon restarts.** The monitor list, encryption blocklist, event log, and recordings are all process-lifetime only unless explicitly marked otherwise. A daemon restart is a clean slate.
- **Backwards compatibility for deprecated endpoints.** Endpoints retired in a phase are *gone*, not hidden. `/api/lsm` is not coming back.

## In short

One API, many consumers, no private backchannels. Any deviation should
be flagged in PR review and either fixed or explicitly documented.
