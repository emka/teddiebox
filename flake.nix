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
            # opus-embedded-sys vendors its own libopus copy and configures
            # it with autoreconf rather than using the nixpkgs libopus above.
            pkgs.autoconf
            pkgs.automake
            pkgs.libtool
            # opus-embedded-sys generates FFI bindings with bindgen, which
            # needs libclang at build time (distinct from the C compiler).
            pkgs.libclang
            # toniefile's build script shells out to protoc via prost-build.
            # The crate vendors a prebuilt protoc binary that NixOS can't
            # run (it's dynamically linked against a generic glibc), so we
            # supply one from nixpkgs instead.
            pkgs.protobuf
          ];

          # Tell prost-build to use the nixpkgs protoc rather than its
          # vendored binary.
          PROTOC = "${pkgs.protobuf}/bin/protoc";

          # bindgen (via opus-embedded-sys) links libclang at build time and
          # cannot find it without an explicit path on NixOS.
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        };
      });
}
