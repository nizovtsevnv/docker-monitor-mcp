{
  description = "docker-monitor-mcp — read-only MCP server for Docker logs and host/container metrics";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" "rustfmt" ];
          targets = [ "x86_64-unknown-linux-musl" ];
        };
      in {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.pkg-config
          ];

          shellHook = ''
            # Install a pre-commit hook mirroring CI (fmt + clippy + test).
            if [ -d .git ]; then
              mkdir -p .git/hooks
              cat > .git/hooks/pre-commit << 'HOOK'
#!/usr/bin/env bash
set -e
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
HOOK
              chmod +x .git/hooks/pre-commit
            fi
          '';
        };
      }
    );
}
