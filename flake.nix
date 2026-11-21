{
  description = "Rust MCP server for Nix packages, options and flakes";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/d5cc6833999746cffdfe0598f7bc801a57b887a9";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      forSystems = lib.genAttrs [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      mkMcpNixos =
        { pkgs }:
        let
          inherit (pkgs) lib;
          manifest = lib.importTOML ./Cargo.toml;
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = manifest.package.name;
          inherit (manifest.package) version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./LICENSE
              ./README.md
              ./src
              ./tests
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.aws-lc ];
          meta = {
            inherit (manifest.package) description;
            homepage = manifest.package.repository;
            license = lib.licenses.mit;
            mainProgram = manifest.package.name;
          };
        };
    in
    {
      lib.mkMcpNixos = mkMcpNixos;
      overlays.default = final: _: { mcp-nixos = mkMcpNixos { pkgs = final; }; };

      packages = forSystems (system: rec {
        mcp-nixos = mkMcpNixos { pkgs = nixpkgs.legacyPackages.${system}; };
        default = mcp-nixos;
      });

      apps = forSystems (system: rec {
        mcp-nixos = {
          type = "app";
          program = lib.getExe self.packages.${system}.mcp-nixos;
          meta.description = self.packages.${system}.mcp-nixos.meta.description;
        };
        default = mcp-nixos;
      });

      checks = forSystems (system: {
        inherit (self.packages.${system}) mcp-nixos;
      });

      devShells = forSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              cargo-audit
              clippy
              rust-analyzer
              rustfmt
            ];
          };
        }
      );

      formatter = forSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
    };
}
