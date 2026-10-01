# Contributing

## Setup

- `just` lists the recipes. `HARDWARE.md` covers the board and download mode.
- Machine-local settings (identity directory, passwords) go in `.envrc.local`.
  It is gitignored. Never commit or echo its contents.

### With Nix (recommended)

`nix develop`, or `direnv allow`. `flake.nix` provides every tool below at the
versions CI uses, and builds libopus for the host and the device.

### Without Nix

Install the following, then follow the libopus steps.

| Tool | Used by | Install |
| --- | --- | --- |
| Espressif Rust fork 1.95.0.0 with `rust-src`, and the Xtensa GCC | firmware | `espup install --toolchain-version 1.95.0.0 --targets esp32s3`, then `source ~/export-esp.sh` |
| `cmake`, `libclang` | MbedTLS and libopus | package manager; set `LIBCLANG_PATH` to the directory holding `libclang.so` |
| `just` | all recipes | package manager or `cargo install just` |
| `espflash`, `esptool` | `just flash` | `cargo install espflash`; `pip install esptool` |
| `cargo-deny`, `cargo-machete` | `just deny`, `just machete` | `cargo install` |
| `shellcheck`, `ruff`, `actionlint`, `check-jsonschema` | `just lint-scripts`, `just lint-workflows` | package manager or `pip install` |
| `cargo-fuzz`, `cargo-mutants`, `tokei`, `scc`, `rust-code-analysis-cli` | `just fuzz`, `just mutants`, `just complexity` | optional |

The flake's versions are the reference. Other versions can change what the
gates report.

#### libopus

The build looks for a static libopus per target in
`TEDDIEBOX_OPUS_LIB_DIR_<TARGET>` and does not compile one itself. Unpack the
libopus 1.6.1 source, then build it for the host and for the device. The device
build needs the Xtensa GCC on `PATH`.

```sh
scripts/build-opus.sh path/to/opus-1.6.1 build/opus-host
scripts/build-opus.sh path/to/opus-1.6.1 build/opus-device --device
```

Point the variables at the `lib/` directories. The target name is upper-cased,
with `-` replaced by `_`:

```sh
export TEDDIEBOX_OPUS_LIB_DIR_X86_64_UNKNOWN_LINUX_GNU=$PWD/build/opus-host/lib
export TEDDIEBOX_OPUS_LIB_DIR_XTENSA_ESP32S3_NONE_ELF=$PWD/build/opus-device/lib
```

Put the exports in `.envrc.local`. Check the result with `just link`.

## Layout

- The root workspace holds the library crates under `crates/`. All are
  `#![no_std]` and are tested on the host.
- `firmware/` and `fuzz/` are workspaces of their own.
  - Build firmware from inside `firmware/`. `--manifest-path` from the root
    skips its cargo config and fails with an error inside a dependency.
  - `heapless` is declared once, in the root workspace, and used as
    `teddiebox_core::heapless`. Do not add it to `firmware/` or `fuzz/`.

## Making a change

- Small steps. Every commit builds, passes the gates and could be released.
  A gate is a check CI runs on every change; a failing gate blocks the merge.
- Work on a short-lived branch and integrate often. Hide unfinished work behind
  a flag or a module nothing calls yet, not behind a long-lived branch.
- Split a change that needs more than a few hundred lines of review.
- Refactoring commits keep behaviour. Behaviour changes are separate commits.

## Tests

Write the failing test first, watch it fail for the expected reason, then write
the code that passes it. Refactor after every green.

- Test behaviour through the public surface, not private fields.
- Name each test after the behaviour and outcome. One reason to fail per test.
- Structure each test as `// Given`, `// When`, `// Then`, separated by blank
  lines.
- Unit tests do no I/O, network access or sleeping, and share no state.
- Driver tests state the expected bus traffic as literal bytes. A test that
  builds its expectation from the table or encoder under test cannot disagree
  with it. The same goes for round trips: assert the bytes, not that a parser
  undoes its own writer.
- A test that needs heavy mocking or deep setup points at a design problem.
  Fix the design.
- `just mutants -p <crate>` finds behaviour no test pins down. It takes hours
  and each missed mutant needs a judgement.
- `just fuzz` searches the parsers for crashes and hangs. Each run finds
  different inputs, so it cannot give a commit a pass or fail.

## Before committing

Run `just check` and read its exit code. It runs every gate CI runs, in CI's
order. CI takes the gates from its dependency list, so a gate added to the
recipe is a gate in CI.

- `just fix` formats both workspaces.
- Chain the commit on the gate itself, for example
  `just check && git commit`. Never `just check; git commit`, and never
  `just check; echo $? && git commit`: both commit a failed run.
- CI minutes are limited. Prefer `just check` locally to pushing to see CI.
  A run that fails within seconds without starting a job is a billing refusal,
  not a code failure.
- Gates that build firmware also check the stack and OTA-slot budgets. Flash
  with `just flash`. `just check` leaves a release image behind, and
  `scripts/flash.sh` alone flashes whatever is there.

## Commits

Follow [Conventional Commits](https://www.conventionalcommits.org/):
`type(scope): summary`, for example `feat(portal): add CA upload`.

- One logical step per commit. A summary that needs "and" is two commits.
- The body says why: the motivation or the problem. The diff shows what.

## Comments and docs

- A comment says what the code does and why, for a reader with no history.
  How it got that way belongs in the commit message.
- A measurement that justifies a value is useful without the story of taking it.
- Do not write a number the code computes. Name the constant or type instead.
- A behaviour change updates every comment that describes it, in the same
  commit. Search for the old value or name first.
- Rewrite a comment as a whole instead of appending to it.
- Docs are plain and short: lists, tables, short sentences, no storytelling.

## Dependencies and licences

- Permissive licences only. `deny.toml` enforces the list. Do not add an
  exception, not even for a development tool.
- `cargo deny` and `cargo machete` are gates. Remove a dependency nothing uses.
- Dependabot does not manage `firmware/Cargo.lock`. After a bump that touches
  it, run `cargo update -p <crate>` inside `firmware/`.
- The patched copies of `mbedtls-rs`, `mbedtls-rs-sys` and `smoltcp` are
  fetched by `scripts/vendor-*.sh`, not committed. `just vendor` runs them.

## Working on the box

- One program at a time owns the serial port. Stop any capture before flashing.
  `just console` refuses to start while a capture runs, and `just flash`
  refuses while a console is open.
- After `esptool` or `espflash`, set the port again before reading it:
  `stty -F /dev/ttyUSB0 115200 raw -echo -echoe -echok -crtscts`.
  `scripts/flash.sh` and `scripts/battery-run.py` do this themselves.
- `espflash` must touch the port before `esptool` does. After a failed
  `espflash`, run no `esptool` command. A power cycle is the only recovery.
- Reproduce a network problem from the host with `curl` before debugging the
  box. teddyCloud here is TLS-only and needs legacy renegotiation.
- Code that takes a large value by value in `main`'s frame can overflow the
  stack on every boot and leave the box silent. Check the stack with the box's
  `stack` command, not with `nm`.

## Pull requests

- Branch from `main` and rebase onto it. History is linear: no merge commits,
  and branches are rebased onto `main`.
- `main` requires a pull request and a passing `all gates` check.
- Keep the description short: why the change exists, and anything a reviewer
  would otherwise miss.
- Say what was verified on hardware, and what was not.

## Licence

The project is `MIT OR Apache-2.0`. Contributions are under the same terms.
