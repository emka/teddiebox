{
  description = "teddiebox — embassy-rs firmware for the ESP32 Toniebox";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Packages the ESP-IDF toolchain releases, which is where the only
    # xtensa C compiler comes from. Deliberately *not* following our nixpkgs:
    # its tool derivations still ask for python310, which unstable dropped.
    nixpkgs-esp-dev.url = "github:mirrexagon/nixpkgs-esp-dev";
  };

  outputs = { self, nixpkgs, flake-utils, fenix, nixpkgs-esp-dev }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        fx = fenix.packages.${system};

        hostTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;
        deviceTarget = "xtensa-esp32s3-none-elf";

        # Nightly, because rustc has no prebuilt core for any xtensa target:
        # it has to be built from source with -Z build-std, which needs
        # rust-src and an unstable cargo. The exact nightly is pinned by
        # flake.lock, so this is reproducible despite the channel name.
        toolchain = fx.complete.withComponents [
          "cargo"
          "rustc"
          "rust-src"
          "clippy"
          "rustfmt"
        ];

        # The xtensa GCC ships as a propagated input of the ESP-IDF bundle
        # rather than as a package of its own. We want the compiler and not
        # the 500 MB framework around it, so pick it out by name.
        xtensaGcc =
          let
            idf = nixpkgs-esp-dev.packages.${system}.esp-idf-xtensa;
            matches = builtins.filter
              (d: builtins.match "xtensa-esp-elf-esp-idf.*" (d.name or "") != null)
              idf.propagatedBuildInputs;
          in
          if matches == [ ]
          then throw "no xtensa-esp-elf toolchain among esp-idf-xtensa's propagated inputs"
          else builtins.head matches;

        xtensaToolchainFile = pkgs.writeText "xtensa-esp32s3.cmake" ''
          set(CMAKE_SYSTEM_NAME Generic)
          set(CMAKE_SYSTEM_PROCESSOR xtensa)
          set(CMAKE_C_COMPILER ${xtensaGcc}/bin/xtensa-esp32s3-elf-gcc)
          set(CMAKE_ASM_COMPILER ${xtensaGcc}/bin/xtensa-esp32s3-elf-gcc)
          set(CMAKE_AR ${xtensaGcc}/bin/xtensa-esp32s3-elf-ar)
          set(CMAKE_RANLIB ${xtensaGcc}/bin/xtensa-esp32s3-elf-ranlib)
          # A freestanding compiler cannot link a hosted executable, so the
          # compiler check has to stop at a static library. Link tests are
          # exactly what made opus-embedded-sys's autotools configure fail;
          # nothing about libopus itself was ever the obstacle.
          set(CMAKE_TRY_COMPILE_TARGET_TYPE STATIC_LIBRARY)
          set(CMAKE_C_FLAGS_INIT "-mlongcalls -ffunction-sections -fdata-sections")
        '';

        # Fixed point on both sides on purpose: the host is the reference the
        # device gets compared against in Phase B, and a float host build
        # would not produce the samples the device produces. The neural
        # extensions are float-only and megabytes of weights, so they stay off.
        opusCmakeFlags = [
          "-DCMAKE_BUILD_TYPE=Release"
          "-DOPUS_BUILD_SHARED_LIBRARY=OFF"
          "-DOPUS_BUILD_PROGRAMS=OFF"
          "-DOPUS_BUILD_TESTING=OFF"
          "-DBUILD_TESTING=OFF"
          "-DOPUS_FIXED_POINT=ON"
          "-DOPUS_ENABLE_DEEP_PLC=OFF"
          "-DOPUS_DRED=OFF"
          "-DOPUS_OSCE=OFF"
          # The host build is a stand-in for the device, which has no SIMD,
          # so timing it against hand-written NEON or SSE kernels would
          # measure the wrong machine. It also keeps the build off the
          # architecture-specific assembly paths, which is what makes one
          # recipe work for every host CI might run on.
          "-DOPUS_DISABLE_INTRINSICS=ON"
        ];

        mkOpus = { pname, extraFlags ? [ ] }: pkgs.stdenv.mkDerivation {
          inherit pname;
          inherit (pkgs.libopus) version src;
          nativeBuildInputs = [ pkgs.cmake pkgs.ninja ];
          cmakeFlags = opusCmakeFlags ++ extraFlags;
          # Host binutils cannot touch xtensa objects, and there is nothing
          # to strip out of a static archive we link whole-program anyway.
          dontStrip = true;
        };

        opusHost = mkOpus { pname = "libopus-fixed"; };
        opusDevice = mkOpus {
          pname = "libopus-fixed-xtensa-esp32s3";
          extraFlags = [ "-DCMAKE_TOOLCHAIN_FILE=${xtensaToolchainFile}" ];
        };

        # teddiebox-opus-sys resolves the archive for whatever target cargo
        # is building, so each one gets its own variable.
        opusEnv = target: lib: {
          name = "TEDDIEBOX_OPUS_LIB_DIR_${
            pkgs.lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] target)
          }";
          value = "${lib}/lib";
        };
      in
      {
        packages = {
          inherit opusHost opusDevice xtensaGcc;
        };

        devShells.default = pkgs.mkShell ({
          packages = [
            toolchain
            xtensaGcc
            # toniefile (fixturegen only) links libopus through pkg-config,
            # and falls back to a cmake build of its own if it can't find it.
            pkgs.pkg-config
            pkgs.libopus
            pkgs.cmake
            # toniefile's build script shells out to protoc via prost-build.
            # The crate vendors a prebuilt protoc binary that NixOS can't
            # run (it's dynamically linked against a generic glibc), so we
            # supply one from nixpkgs instead.
            pkgs.protobuf
          ];

          # Tell prost-build to use the nixpkgs protoc rather than its
          # vendored binary.
          PROTOC = "${pkgs.protobuf}/bin/protoc";
        }
        // builtins.listToAttrs [
          (opusEnv hostTarget opusHost)
          (opusEnv deviceTarget opusDevice)
        ]);
      });
}
