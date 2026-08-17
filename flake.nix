{
  description = "cordis-rs: spatiotemporal composability kernel with a WASM plugin boundary";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        lib = pkgs.lib;
        sourceFiles = lib.fileset.unions [
          ./crates
          ./Cargo.toml
          ./Cargo.lock
          (lib.fileset.fileFilter (file: lib.hasPrefix "LICENSE" file.name) ./.)
        ];
        src = lib.fileset.toSource {
          root = ./.;
          fileset = sourceFiles;
        };
        mkCordis =
          args:
          pkgs.rustPlatform.buildRustPackage (
            {
              pname = "cordis";
              version = "0.1.0";
              inherit src;
              cargoLock.lockFile = ./Cargo.lock;
              doCheck = true;
            }
            // args
          );
      in
      {
        packages.default = mkCordis {
          meta = {
            description = "spatiotemporal composability kernel with a WASM plugin boundary";
            license = pkgs.lib.licenses.mit;
          };
        };

        # buildRustPackage's checkPhase runs `cargo test`, which proves the
        # config-as-WASM load-at-startup and revert-on-unmount tests pass.
        checks.default = mkCordis { };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = [
            pkgs.cargo
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
        };
      }
    );
}
