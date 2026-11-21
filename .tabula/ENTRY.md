# mcp-nixos

## Request path

An MCP client starts the binary and exchanges JSON-RPC messages over stdio.
Both supported protocol revisions dispatch to the same two tools. Upstream
HTTP requests share a bounded client, and tool results are returned as plain
text. [Entry point](../src/main.rs) · [MCP handlers](../src/server.rs) · [HTTP client](../src/http.rs)

## Development and distribution

Cargo builds the server. The package flake exposes it as an app, a package and
an overlay. The Linux development shell supplies the Rust toolchain and the
checks invoked by CI and the pre-commit hook. [Package](../flake.nix) ·
[Development shell](../devenv.nix) · [Checks](../.project-checks.json) · [Recipes](../Justfile)
