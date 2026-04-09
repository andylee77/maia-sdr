# 006 -- P25 Phase 2B: Web Dashboard

**Date:** 2026-04-08
**Phase:** 2B (Web UI + REST API + WebSocket)
**Branch:** fishball-p25

---

## Summary

Built a complete embedded web dashboard for real-time P25 trunking radio
monitoring. The dashboard is served as a single-page app from the Rust binary
with no external dependencies. Features live activity feed via WebSocket,
frequency map, talkgroup aliases, and dark/light theme.

## Architecture

- **Embedded SPA**: Dashboard HTML/JS/CSS is a const string in the binary
- **Axum HTTP server**: REST endpoints + WebSocket on port 8080
- **Real-time events**: tokio broadcast channel pushes TSBK events to WebSocket clients
- **State sharing**: `Arc<RwLock<ControlChannelDecoder>>` for thread-safe access

## API Endpoints

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/` | Dashboard HTML |
| GET | `/api/system` | System identity |
| GET | `/api/grants` | Active voice grants |
| GET | `/api/bands` | Frequency band table |
| GET | `/api/stats` | Decoder statistics |
| GET | `/api/aliases` | Talkgroup alias map |
| PUT | `/api/aliases` | Update aliases |
| WS | `/ws/events` | Real-time events |

## Dashboard Features

- **Live activity feed**: WebSocket-driven, newest-first, color-coded by event type (GRP_GRANT=green, GRANT_UPD=blue, NET_STS=orange, IDEN_UP=purple)
- **System identity card**: NAC, WACN, System ID, RFSS/Site, control channel, tracking status
- **Decode stats card**: Message count, active grants, bands known, dibit count, overflow status
- **Frequency map**: All 11 Clay County LCNs positioned proportionally by frequency, active grants marked with stars, control channel labeled
- **Active grants table**: Channel, talkgroup (with alias), source radio, frequency, age
- **Frequency bands table**: Band ID, base frequency, spacing, TX offset, bandwidth
- **Dark/light theme**: CSS custom properties, toggle button, localStorage persistence
- **Talkgroup aliases**: Settings modal to edit JSON map (ID -> name), saved via PUT

## Files Modified

- `p25-httpd/src/httpd/mod.rs` -- Full REST API + WebSocket + embedded dashboard (~320 lines HTML/JS/CSS)
- `p25-httpd/src/main.rs` -- AppState wiring, broadcast channel, HTTP server startup
- `p25-httpd/src/p25/control_channel.rs` -- Event broadcasting via `set_event_tx()`, `tsbk_to_event()` for all 6 opcode types
- `p25-httpd/p25-json/src/lib.rs` -- TsbkEvent, AliasMap, DecoderStats, updated ChannelGrant/BandInfo/SystemInfo types

## Design Decisions

- **Embedded HTML vs separate WASM**: Chose embedded for simplicity -- no build pipeline, single binary deployment, works on resource-constrained Zynq ARM
- **WebSocket vs polling**: WebSocket for live events (instant), REST polling as background state sync (every 2s)
- **Vanilla JS**: No framework -- keeps the embedded HTML small (~8KB) and avoids build complexity
- **LCN data hardcoded in JS**: The 11 Clay County frequencies are static in the frontend for the frequency map -- will need to be dynamic once the band table is populated from IDEN_UP messages
