# mcp-nixos

A Rust [Model Context Protocol](https://modelcontextprotocol.io/) server for looking up
Nix packages, module options, flakes and package history. It returns plain text and
communicates over stdio. It supports MCP revisions `2026-07-28` and `2025-11-25`.

## Connect a client

With Nix and flakes enabled, add this entry to your MCP client's configuration and
reload the client. Run `nix run github:timfewi/nixos-mcp-rust -- --version` to check
that the server launches.

```json
{
  "mcpServers": {
    "nixos": {
      "command": "nix",
      "args": ["run", "github:timfewi/nixos-mcp-rust"]
    }
  }
}
```

To install with Cargo on Linux or macOS, use the command below. Point your client
at `mcp-nixos` on its PATH with an empty argument list. Building from source
requires a Rust toolchain, a C compiler, CMake and Perl.

```sh
cargo install --locked --git https://github.com/timfewi/nixos-mcp-rust
```

To pin the server in a Nix project, add `inputs.mcp-nixos.url = "github:timfewi/nixos-mcp-rust";`
and use `mcp-nixos.packages.${system}.default`. The flake also exports
`overlays.default` for consumers that install `pkgs.mcp-nixos`.

## Tools

Pass these arguments to the named tool in your MCP client.

| Lookup                                   | Tool           | Arguments                                                                                       |
| ---------------------------------------- | -------------- | ----------------------------------------------------------------------------------------------- |
| Package in a release channel             | `nix`          | `{"action":"info","query":"ripgrep","channel":"stable"}`                                        |
| Home Manager options                     | `nix`          | `{"action":"browse","source":"home-manager","query":"programs.git"}`                            |
| Inputs in the local lockfile             | `nix`          | `{"action":"flake-inputs","type":"lock","flake_dir":"."}`                                       |
| Package at the project's pinned revision | `nix`          | `{"action":"info","source":"pinned","query":"ripgrep","flake_dir":".","system":"x86_64-linux"}` |
| Cached binary for a system               | `nix`          | `{"action":"cache","query":"hello","system":"aarch64-linux"}`                                   |
| Versions and nixpkgs commits             | `nix_versions` | `{"package":"ripgrep","limit":3}`                                                               |

Online queries use the upstream catalogues directly. Local queries run on the
server's machine. Set `flake_dir` to the project directory when the client starts
the server elsewhere. Pinned package evaluation and flake archiving require the Nix
CLI. File lookup requires `nix-locate` and a nix-index database. Use
`NIX_INDEX_DATABASE` to select its directory.

For all sources and supported actions, see the [tool reference](src/nix_tool.md)
and the argument schemas for [nix](src/nix_schema.json) and [nix_versions](src/nix_versions_schema.json).

## Development

On Linux, enter the pinned Rust development environment and run the project checks.
Use `just hooks` once per clone to install the pre-commit check.

```sh
devenv shell
just fmt-check
just check
just test
just start --version
```

`just check` runs the fast scaffold checks, including Clippy. `just verify` also builds
the release package and runs the test suite. Tests use local upstream fixtures and
exercise both MCP revisions over stdio. Run `just start` to serve a client until stdin
closes. Connect to this checkout with the [local MCP configuration](.mcp.json).

For a Cargo build on Linux or Apple Silicon, `nix develop` provides the build tools.
Run `nix build` to produce `result/bin/mcp-nixos`, or `nix run . -- --version` for a smoke test.

This fork is a Rust rewrite of the original server by James Brink. [Upstream project](https://github.com/utensils/mcp-nixos) · [MIT license](LICENSE)
