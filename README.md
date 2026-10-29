# Schemify PDK Manager

A [Schemify](https://github.com/OpenFAT/Schemify) plugin that installs PDKs per project, switches
a design between versions and technologies by rewriting `schemify.project`, and removes what no
project references. The behaviour is specified in Schemify's
[docs/spec/pdk.md](https://github.com/OpenFAT/Schemify/blob/main/docs/spec/pdk.md).

| Family | Source | Needs on `PATH` |
|---|---|---|
| `sky130`, `gf180mcu`, `ihp-sg13g2` | [ciel](https://github.com/fossi-foundation/ciel) (open_pdks builds); xschem symbols imported | `ciel`, `schemify-interop` |
| `freepdk45` | a pinned, sha256-checked download; device cells generated | `curl`, `tar` |
| `asap7` | a pinned `git` checkout; device cells generated, BSIM-CMG through OSDI | `git` |

Any family also installs from a kit already on disk: `pdk.install <family> <name> <path>`.

The plugin needs `spawn`, so it runs in the native editor only, not in the browser.

## Install

In your Schemify config (`init.lua`), then `schemify pack sync`:

```lua
schemify.pack.add {
  {
    src = "https://github.com/OpenFAT/PDK-Manager",
    grant = {
      "read_project", "write_project",
      "spawn:ciel", "spawn:schemify-interop",  -- ciel families
      "spawn:git",                             -- asap7
      "spawn:curl", "spawn:tar",               -- freepdk45
    },
  },
}
```

The repository root is the package: `schemify.project` names the module (`pdk_manager.wasm`,
committed) and the grants it requests. It gets the intersection of those and your `grant` list,
so leave out the families you don't use; `schemify pack status` shows what was denied. Pin a
commit with `rev = "<sha>"`.

Or per project, with a checkout of this repository, in the project's `schemify.project`:

```
plugin pdk-manager path <checkout>/pdk_manager.wasm
  grant read_project
  grant write_project
  grant spawn:ciel
  grant spawn:schemify-interop
  grant spawn:git
  grant spawn:curl
  grant spawn:tar
```

## Use

**View ▸ PDKs** (command `pdk.panel`) opens the panel: installed versions with Use and Remove,
the available versions of a family, and the running install's steps. Every action is also a
command, so scripts and the JSON-RPC interface can drive it:

| Command | Does |
|---|---|
| `pdk.install <family> <version>` | Installs into `.schemify/pdks/<family>/<version>/` |
| `pdk.install <family> <name> <path>` | Installs a kit already on disk |
| `pdk.use <family> <version>` | Points the project's manifest at that install (`lib`, `lib tech map`, `set PDK_ROOT`, `set PDK_MODELS`) |
| `pdk.remove <family> <version>` | Removes an install; refuses the active and the locked one |
| `pdk.prune`, then `pdk.prune.confirm` | Lists, then removes, every install nothing references |
| `pdk.list_remote` | Refreshes the available versions of the open family |

Designs drawn with `tech:nmos` / `tech:pmos` retarget between families through the shipped maps
(`maps/`).

## Develop

The plugin is a Rust crate built for `wasm32-unknown-unknown`. The Schemify crates it uses
(`schemify-abi`, and `schemify-format` and `schemify-plugin` for tests) are git dependencies
pinned to one Schemify commit in `Cargo.toml`.

```sh
nix develop                  # Rust 1.95.0 (rust-overlay) with the wasm32 std and rust-lld
./build.sh                   # -> pdk_manager.wasm (fails at 200 KB)
cargo test                   # pure modules, then the built guest through the real plugin host
cargo test -- --ignored      # network: real ciel, FreePDK45 and ASAP7 installs simulated in ngspice
```

`build.sh` is byte-reproducible under `nix develop`: CI rebuilds the module and fails if the
committed `pdk_manager.wasm` differs, so commit the module with the source change (or the
`pdk-manager-wasm` artifact of the CI run). A nightly job builds and tests against Schemify
`main`; when it goes red, bump `rev` in `Cargo.toml` and fix.

To test against a local Schemify checkout, without changing `Cargo.toml`:

```sh
cargo test --config 'patch."https://github.com/OpenFAT/Schemify".schemify-abi.path="../Schemify/crates/abi"' \
           --config 'patch."https://github.com/OpenFAT/Schemify".schemify-format.path="../Schemify/crates/format"' \
           --config 'patch."https://github.com/OpenFAT/Schemify".schemify-plugin.path="../Schemify/crates/plugin"'
```

The patch rewrites `Cargo.lock`; restore it (`git checkout Cargo.lock`) before committing.

`devices/` and `tests/fixtures/schemify/` hold copies of Schemify files (see the README in
each); keep them in step when you bump `rev`.

## License

MIT
