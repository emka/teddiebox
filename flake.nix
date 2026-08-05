{
  description = "teddiebox — embassy-rs firmware for the ESP32 Toniebox";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, fenix }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        fx = fenix.packages.${system};

        # The bare-metal target proves the no_std crates stay no_std. It is
        # the cheap stand-in for xtensa until the firmware crate exists.
        toolchain = fx.combine [
          fx.stable.toolchain
          fx.targets.thumbv7em-none-eabihf.stable.rust-std
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          packages = [
            toolchain
            # toniefile and opus-embedded both build libopus through cc.
            pkgs.pkg-config
            pkgs.cmake
            pkgs.libopus
          ];
        };
      });
}
