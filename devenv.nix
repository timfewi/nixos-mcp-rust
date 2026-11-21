{ inputs, pkgs, ... }:
let
  system = pkgs.stdenv.hostPlatform.system;
in
{
  devenv.latestVersion = "2.4.0";

  packages = [
    inputs.project-check.packages.${system}.project-check
    pkgs.age
    pkgs.aws-lc
    pkgs.bashInteractive
    pkgs.biome
    pkgs.cargo
    pkgs.cargo-audit
    pkgs.clippy
    pkgs.coreutils
    pkgs.deadnix
    pkgs.git
    pkgs.husky
    pkgs.jq
    pkgs.just
    pkgs.nix
    pkgs.nixfmt
    pkgs.pkg-config
    pkgs.python3
    pkgs.ripgrep
    pkgs.secretspec
    pkgs.shfmt
    pkgs.sops
    pkgs.rust-analyzer
    pkgs.rustc
    pkgs.rustfmt
    pkgs.statix
  ];

  enterTest = "just check";
}
