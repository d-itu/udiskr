
{
  description = "Lightweight alternative to udiskie";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system}.extend rust-overlay.overlays.default;
        rust = pkgs.rust-bin.stable.latest.default;
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "udiskr";
          version = "0.2.0";
          src = self;

          nativeBuildInputs = [ rust ];

          cargoLock.lockFile = ./Cargo.lock;

          doCheck = false;
        };
      }
    );
}
