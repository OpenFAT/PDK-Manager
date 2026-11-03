{
  description = "Schemify PDK manager plugin: per-project PDK installs";

  # nixpkgs and rust-overlay pinned to the revs Schemify's flake.lock pins: Rust 1.95.0 with
  # rust-lld, so `nix develop -c ./build.sh` reproduces the committed pdk_manager.wasm byte for byte.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # The Interop plugin's CLI (`schemify-interop`), which the plugin spawns (xschem import,
    # exports, pdk-scan): in the dev shell, so the guest tests can run the real one.
    interop = {
      url = "github:OpenFAT/Interoperate";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.rust-overlay.follows = "rust-overlay";
    };
  };

  outputs =
    {
      nixpkgs,
      rust-overlay,
      interop,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAll =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
    in
    {
      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          # Rust 1.95.0 (the rust-version) with the wasm32 std and rust-lld, and the real
          # `schemify-interop` (tests/guest.rs puts its fakes ahead of it on PATH).
          packages = [
            (pkgs.rust-bin.stable."1.95.0".default.override {
              targets = [ "wasm32-unknown-unknown" ];
            })
            interop.packages.${pkgs.stdenv.hostPlatform.system}.default
          ];
        };
      });
      formatter = forAll (pkgs: pkgs.nixfmt);
    };
}
