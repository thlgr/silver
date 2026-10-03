# MCP integration

Rust port of the core protocol paths in Hermes' `tools/mcp_tool_*.py` family.

- `mod.rs` - shared constants (protocol version, timeouts, retry/backoff, caps, env allowlist)
  and pure helpers (Unicode-tag stripping, credential redaction, head/tail truncation, globs).
- `transport.rs` - stdio JSON-RPC framing with a filtered child environment, and Streamable
  HTTP with JSON / SSE response handling.
- `client.rs` - `initialize` + `notifications/initialized` + paginated `tools/list`,
  tools/call forwarding, lazy reconnect, and `McpManager`.
- `tools.rs` - registry naming, JSON-schema normalization, injection scanning, conservative
  risk classification and CallToolResult rendering.

## Wiring

```rust
let mcp = silver::mcp::McpManager::new();
let mcp_tools = mcp.connect_all(&config).await; // Vec<Arc<dyn Tool>>
silver::mcp::McpManager::register_into(&mut registry, mcp_tools);
// ... after the server exits:
mcp.shutdown().await;
```

## Security

MCP servers are untrusted. Stdio children receive only an allowlisted environment (never the
daemon's), descriptions are scanned for injection and results have Unicode TAG characters
stripped, error text is credential-redacted, and every tool is tagged into the `mcp` toolset
with a conservative risk (Write/Process unless the server's `readOnlyHint` is exactly `true`
or the name is unambiguously read-only).

## Known deviations from Hermes

- No OAuth, portal, sampling, elicitation or client-certificate support.
- Only the legacy `initialize` handshake (no `server/discover` / 2026-07-28 negotiation).
- Resource/prompt utility tools and media/document caching are not registered; embedded
  media is reported inline instead of cached to a `MEDIA:` tag.
- No per-server circuit breaker or keepalive; reconnect is lazy with the capped geometric
  connect cooldown.
- The 64-char name clamp uses a deterministic 8-hex FNV-1a suffix instead of SHA-256 (no hash
  dependency is declared for this crate); the stability invariant is unchanged.
