{
  inputs = {
    nixpkgs-unstable.url = "github:nixos/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs-unstable";
    };
    utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs-unstable,
    rust-overlay,
    utils,
    ...
  }:
    utils.lib.eachDefaultSystem (
      system: let
        overlays = [(import rust-overlay)];

        pkgs = import nixpkgs-unstable {
          inherit system overlays;

          # Best practice - avoids allowing impure options set by default.
          config = {};
        };

        rust-toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          name = "plan";
          version = "0.1.0-alpha.1";
          src = pkgs.lib.cleanSource ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          nativeBuildInputs =
            [
              rust-toolchain
              pkgs.installShellFiles
            ];

          cargoTestFlags = ["--features" "test-clock"];

          postInstall = ''
            installManPage plan.1
          '';
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs;
            [
              rust-toolchain
              cargo-nextest
            ];
        };

        apps.formatter = {
          type = "app";
          program = "${pkgs.alejandra}/bin/alejandra";
        };

        formatter = pkgs.alejandra;
      }
    );
}
