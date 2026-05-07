# SPDX-FileCopyrightText: 2026 LunNova
#
# SPDX-License-Identifier: CC0-1.0

{
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    pre-commit-hooks = {
      url = "github:cachix/pre-commit-hooks.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      pre-commit-hooks,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rust-bin = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in
      {
        checks = {
          pre-commit-check = pre-commit-hooks.lib.${system}.run {
            src = ./.;
            hooks = {
              statix.enable = true;
              nixfmt-rfc-style.enable = true;
              deadnix.enable = true;
              rustfmt = {
                enable = true;
                entry = "rustfmt --config-path ${./rustfmt.toml}";
                package = rust-bin;
              };
            };
          };
        };
        formatter = pkgs.treefmt.withConfig {
          runtimeInputs = with pkgs; [
            nixfmt-rfc-style
            rust-bin
          ];
          settings = {
            tree-root-file = ".git/index";
            formatter = {
              nixfmt = {
                command = "nixfmt";
                includes = [ "*.nix" ];
              };
              rustfmt = {
                command = "rustfmt";
                options = pkgs.lib.mkForce [
                  "--config-path"
                  ./rustfmt.toml
                ];
                includes = [ "*.rs" ];
              };
            };
          };
        };
        devShells.default = pkgs.mkShell {
          inherit (self.checks.${system}.pre-commit-check) shellHook;

          buildInputs = [
            pkgs.pkg-config
            pkgs.cargo-nextest
            pkgs.cargo-machete
            pkgs.openssl.dev
            pkgs.openssl
            rust-bin
          ];
        };
      }
    );
}
