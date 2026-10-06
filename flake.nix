{
  description = "teddiebox — embassy-rs firmware for the ESP32 Toniebox";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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

        # libopus is built by scripts/build-opus.sh, the recipe a contributor
        # without Nix runs by hand, so both build it the same way.
        opusRecipe = pkgs.lib.fileset.toSource {
          root = ./scripts;
          fileset = pkgs.lib.fileset.unions [
            ./scripts/build-opus.sh
            ./scripts/xtensa-esp32s3.cmake
          ];
        };

        mkOpus = { pname, device ? false, extraInputs ? [ ] }: pkgs.stdenv.mkDerivation {
          inherit pname;
          inherit (pkgs.libopus) version src;
          nativeBuildInputs = [ pkgs.cmake pkgs.ninja ] ++ extraInputs;
          dontUseCmakeConfigure = true;
          buildPhase = "bash ${opusRecipe}/build-opus.sh \"$PWD\" \"$out\" ${pkgs.lib.optionalString device "--device"}";
          dontInstall = true;
          # Host binutils cannot touch xtensa objects, and there is nothing
          # to strip out of a static archive we link whole-program anyway.
          dontStrip = true;
        };

        opusHost = mkOpus { pname = "libopus-fixed"; };
        opusDevice = mkOpus {
          pname = "libopus-fixed-xtensa-esp32s3";
          device = true;
          extraInputs = [ xtensaGcc ];
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

          # What evaluating this flake reads but no package refers to: the
          # flake inputs, and the esp-idf checkout nixpkgs-esp-dev reads its
          # tool versions from while evaluating. CI collects garbage before
          # saving its Nix store; built as a GC root there, this keeps them,
          # or every job fetches esp-idf with its submodules again, 85 s.
          ci-gc-roots = pkgs.linkFarm "ci-gc-roots" [
            { name = "nixpkgs"; path = nixpkgs; }
            { name = "flake-utils"; path = flake-utils; }
            { name = "systems"; path = flake-utils.inputs.systems; }
            { name = "nixpkgs-esp-dev"; path = nixpkgs-esp-dev; }
            { name = "nixpkgs-esp-dev-nixpkgs"; path = nixpkgs-esp-dev.inputs.nixpkgs; }
            {
              name = "esp-idf";
              path = nixpkgs-esp-dev.packages.${system}.esp-idf-xtensa.src;
            }
          ];
        };

        devShells.default = pkgs.mkShell ({
          packages = [
            toolchain
            xtensaGcc
            # mbedtls-rs-sys builds MbedTLS for the device with cmake.
            pkgs.cmake
            # The recipes in ./justfile mirror the CI gates, so a commit can be
            # checked the way the pipeline will check it.
            pkgs.just
            # `just deny`: advisories, licences and sources of every
            # dependency, against deny.toml.
            pkgs.cargo-deny
            # `just lint-scripts`: the shell and Python under scripts/.
            pkgs.shellcheck
            pkgs.ruff
            # `just lint-workflows`: the CI workflows, including the shell in
            # their `run:` steps, which it hands to the shellcheck above, and
            # the Dependabot config against its schema.
            pkgs.actionlint
            pkgs.check-jsonschema
            # `just machete`: dependencies no code uses, which in firmware/
            # cost flash and build time.
            pkgs.cargo-machete
            # `just fuzz`: coverage-guided fuzzing of the parsers in fuzz/.
            pkgs.cargo-fuzz
            # `just mutants`: which changes to the code no test notices.
            pkgs.cargo-mutants
            # mbedtls-rs-sys runs bindgen over MbedTLS's headers, and bindgen
            # loads libclang at run time to do it. The C itself is compiled by
            # the Xtensa GCC above; this is only the header parser.
            pkgs.libclang.lib
            # Flashing. Both are pinned in the shell rather than fetched when
            # needed: `just flash` runs them in a fixed order with fixed
            # flags, and needs the same versions every time.
            pkgs.espflash
            pkgs.esptool
            # `just complexity`: size, per-file complexity, and per-function
            # cyclomatic/cognitive complexity, so the accidental-complexity
            # signals that found main.rs's god-functions stay a repeatable
            # command rather than an ad-hoc nix shell.
            pkgs.tokei
            pkgs.scc
            pkgs.rust-code-analysis
          ];

          # bindgen finds libclang by this variable and by nothing else.
          LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";
        }
        // builtins.listToAttrs [
          (opusEnv hostTarget opusHost)
          (opusEnv deviceTarget opusDevice)
        ]);
      });
}
