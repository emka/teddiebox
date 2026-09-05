{
  description = "teddiebox — embassy-rs firmware for the ESP32 Toniebox";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Packages the ESP-IDF toolchain releases, which is where the only
    # xtensa C compiler comes from. Deliberately *not* following our nixpkgs:
    # its tool derivations still ask for python310, which unstable dropped.
    nixpkgs-esp-dev.url = "github:mirrexagon/nixpkgs-esp-dev";
  };

  outputs = { self, nixpkgs, flake-utils, nixpkgs-esp-dev }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        hostTarget = pkgs.stdenv.hostPlatform.rust.rustcTarget;
        deviceTarget = "xtensa-esp32s3-none-elf";

        # Espressif's Rust fork, as espup would install it.
        #
        # Upstream rustc can compile for xtensa but cannot produce an image:
        # its LLVM emits literal pools at offsets the ESP linker rejects as
        # "dangerous relocation", `core` itself included, and lld has no
        # Xtensa support to fall back on. This fork carries the Xtensa
        # patches that make the output linkable, and the CPU model for the
        # S3 — so it also stops silently discarding the chip's own
        # instructions, native atomics among them.
        #
        # It is nightly, which -Z build-std needs anyway, and it is a
        # complete toolchain: host builds use it too, rather than running two
        # compilers of different vintages against one Cargo.lock.
        espRustVersion = "1.95.0.0";

        espRustTarballs = {
          x86_64-linux = {
            arch = "x86_64-unknown-linux-gnu";
            hash = "sha256-qtL7JLrqtq1hxB8ALxNv4LQW7zmlAdKXUKtmwYpplDM=";
          };
          aarch64-linux = {
            arch = "aarch64-unknown-linux-gnu";
            hash = "sha256-DH2I5oBfm3egSPMH/LHfDGWGOjBsJ4Ej3HcanLbShEw=";
          };
        };

        espRustTarball = espRustTarballs.${system} or (throw
          "no esp-rs Rust build published for ${system}");

        toolchain = pkgs.stdenv.mkDerivation {
          pname = "rust-esp";
          version = espRustVersion;

          src = pkgs.fetchurl {
            url = "https://github.com/esp-rs/rust-build/releases/download/"
              + "v${espRustVersion}/rust-${espRustVersion}-${espRustTarball.arch}.tar.xz";
            inherit (espRustTarball) hash;
          };

          # Shipped separately from the compiler, and -Z build-std needs it:
          # there is no prebuilt `core` for any xtensa target to download.
          rustSrc = pkgs.fetchurl {
            url = "https://github.com/esp-rs/rust-build/releases/download/"
              + "v${espRustVersion}/rust-src-${espRustVersion}.tar.xz";
            hash = "sha256-cIvuM3rC1BwOhhr5MEe//skaBS+8tJV5JdhApB8ydxc=";
          };

          nativeBuildInputs = [ pkgs.autoPatchelfHook ];
          buildInputs = [ pkgs.zlib pkgs.stdenv.cc.cc.lib ];

          # Prebuilt binaries: nothing to configure, build, or strip.
          dontConfigure = true;
          dontBuild = true;
          dontStrip = true;

          installPhase = ''
            runHook preInstall

            patchShebangs install.sh

            # Skipping rust-docs keeps a 100 MB manual out of the closure.
            ./install.sh \
              --prefix=$out \
              --disable-ldconfig \
              --components=rustc,cargo,rustfmt-preview,clippy-preview,rust-std-${hostTarget}

            tar -xf $rustSrc
            patchShebangs rust-src-nightly/install.sh
            ./rust-src-nightly/install.sh --prefix=$out --disable-ldconfig

            rm -rf $out/lib/rustlib/{components,manifest-*,install.log,uninstall.sh,rust-installer-version}

            runHook postInstall
          '';

          passthru.rustSrcPath = "lib/rustlib/src/rust/library";
        };

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
          # Bare metal has nothing to initialise the stack guard, and a
          # check that reads an uninitialised canary is worse than no check.
          "-DOPUS_STACK_PROTECTOR=OFF"
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
            #
            # It gets the same fixed-point, intrinsics-free build the decoder
            # uses, not the nixpkgs one. nixpkgs builds libopus float with
            # run-time CPU detection, so its encoder picks NEON or AVX kernels
            # by host and the same input encodes to different bytes on
            # different machines — which is not a codec that can produce a
            # fixture the repository commits and CI re-derives.
            pkgs.pkg-config
            opusHost
            pkgs.cmake
            # toniefile's build script shells out to protoc via prost-build.
            # The crate vendors a prebuilt protoc binary that NixOS can't
            # run (it's dynamically linked against a generic glibc), so we
            # supply one from nixpkgs instead.
            pkgs.protobuf
            # The recipes in ./justfile mirror the CI gates, so a commit can be
            # checked the way the pipeline will check it.
            pkgs.just
            # mbedtls-rs-sys runs bindgen over MbedTLS's headers, and bindgen
            # loads libclang at run time to do it. The C itself is compiled by
            # the Xtensa GCC above; this is only the header parser.
            pkgs.libclang.lib
            # Flashing. Both, and in the shell rather than fetched ad hoc:
            # `just flash` has to run them in a fixed order with fixed flags,
            # and a recipe that reaches outside the environment for its tools
            # is one whose behaviour depends on what the network felt like.
            pkgs.espflash
            pkgs.esptool
          ];

          # bindgen finds libclang by this variable and by nothing else.
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

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
