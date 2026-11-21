# Working on mcp-nixos

See [README.md](README.md) for setup and development commands.

- Keep the stdio tools `nix` and `nix_versions` compatible with both protocol
  revisions. Codex, Claude, OMP and MemoryCreep use the `2025-11-25` handshake.
  The native `2026-07-28` path uses per-request metadata. Both share the handlers
  in `src/server.rs`. Keep the schemas and tool instructions in `src/` in sync.
- Implement data sources in `src/sources/`. Use `src/http.rs` for bounded upstream
  requests. Preserve cancellation of HTTP requests and Nix children. Run blocking
  HTML parsing and file I/O off the async workers.
- Cache upstream data at process scope. Discover channels dynamically and retry
  discovery after a fallback. Validate local store paths before reading them.
- Keep tool responses in plain text. Count characters for output limits and use
  `text::casefold` for case-insensitive ordering. Fix lint findings at their source.
- Put pure logic tests beside the code, source behavior in `tests/` and wire
  behavior in `tests/protocol.rs`. The fixture in `tests/common/mod.rs` replaces
  upstream services using `MCP_NIXOS_UPSTREAM`.
- Use `type: summary` for commit messages. Update the Cargo manifest, lockfile and
  release notes together for a version change. Consumers pin the flake from Git.
