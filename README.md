<div align="center">

# PDK Manager

**Swap the process under your schematic with one command.**

A [Schemify](https://github.com/OpenFAT/Schemify) plugin that installs PDKs into your project and
switches a design between technologies by rewriting `schemify.project`. You don't touch the
schematic.

[![ci](https://github.com/OpenFAT/PDK-Manager/actions/workflows/ci.yml/badge.svg)](https://github.com/OpenFAT/PDK-Manager/actions/workflows/ci.yml)
[![nightly](https://github.com/OpenFAT/PDK-Manager/actions/workflows/nightly.yml/badge.svg)](https://github.com/OpenFAT/PDK-Manager/actions/workflows/nightly.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`sky130` · `gf180mcu` · `ihp-sg13g2` · `freepdk45` · `asap7` · your own kit

</div>

---

```
pdk.install sky130 <version>      # fetch it into .schemify/pdks/
pdk.use     sky130 <version>      # point the project at it
```

Draw once with `tech:nmos` and `tech:pmos`, then try the same inverter in 130 nm, 180 nm, 45 nm
and 7 nm FinFET. The plugin updates the manifest, and the shipped tech maps turn every class
device into the right cell with the right parameters for the new kit: microns for sky130, fin
counts for ASAP7.

## Why

- **Your design stays the same.** Switching rewrites only the PDK lines of `schemify.project`.
  Your libraries, settings, plugins and comments are kept byte for byte.
- **Benches carry over.** `set PDK_MODELS` always names the active kit's corner file, so a bench
  corner written as `lib ${PDK_MODELS} tt` runs against whichever PDK is in use.
- **Installs are per project.** Each version lives under `.schemify/pdks/<family>/<version>/`.
  Two projects can use two versions of the same PDK without stepping on each other.
- **Cleanup is safe.** `pdk.prune` removes only what nothing references. It never removes the
  active install or one pinned by `schemify.lock`.
- **Tested end to end.** CI drives the built module through the real Schemify plugin host. The
  network tests install FreePDK45 and ASAP7 for real and simulate the same `tech:` inverter in
  ngspice on both.

## What a switch does

`pdk.use gf180mcu <version>` on a project that was using sky130:

```diff
 schemify 1 project

 # my own library
 lib mine path lib
-lib sky130_fd_pr path .schemify/pdks/sky130/<old>/lib/sky130_fd_pr
-lib tech map .schemify/pdks/sky130/<old>/tech.map

 plugin pdk-manager path plugins/pdk-manager
   grant write_project

-set PDK_ROOT .schemify/pdks/sky130/<old>/root
-set SKYWATER_MODELS ${PDK_ROOT}/${PDK}/libs.tech/combined
 set MINE 1
+
+lib gf180mcu_fd_pr path .schemify/pdks/gf180mcu/<new>/lib/gf180mcu_fd_pr
+lib tech map .schemify/pdks/gf180mcu/<new>/tech.map
+
+set PDK_ROOT .schemify/pdks/gf180mcu/<new>/root
+set PDK gf180mcuD
+set PDK_MODELS ${PDK_ROOT}/gf180mcuD/libs.tech/ngspice/sm141064.ngspice
+set GF_MODELS ${PDK_ROOT}/${PDK}/libs.tech/ngspice
```

This is the expected output of the `use_rewrites_only_what_it_owns` test in `src/lib.rs`. The
plugin removes every `lib` and `set` that pointed into the store, plus `lib tech`, keeps
everything else, and appends the new kit's lines.

## Supported PDKs

| Family | Node | Source | Versions | Needs on `PATH` |
|---|---|---|---|---|
| `sky130` | 130 nm | [ciel](https://github.com/fossi-foundation/ciel) (open_pdks builds), xschem symbols imported | any ciel lists | `ciel`, `schemify-interop` |
| `gf180mcu` | 180 nm | ciel, xschem symbols imported | any ciel lists | `ciel`, `schemify-interop` |
| `ihp-sg13g2` | 130 nm SiGe BiCMOS | ciel, xschem symbols imported | any ciel lists | `ciel`, `schemify-interop` |
| `freepdk45` | 45 nm | pinned, sha256-checked download; device cells generated | `5b93a0b1e12d` | `curl`, `tar` |
| `asap7` | 7 nm FinFET | pinned `git` checkout; device cells generated, BSIM-CMG through OSDI | `58d72c9d291e186a77468586ab0c43d8a21eda6a` | `git` |

**Bring your own kit.** Any family also installs from a copy you already have on disk, such as a
registered NCSU FreePDK45:

```
pdk.install freepdk45 ncsu /opt/FreePDK45
```

### Technology classes

The class names a `tech:` device resolves through, from `maps/`:

| Family | `nmos` / `pmos` classes |
|---|---|
| `sky130`, `gf180mcu`, `ihp-sg13g2` | `core` |
| `freepdk45` | `core` (VTL), `hvt` (VTH), `io` (thick oxide) |
| `asap7` | `core` (RVT), `lvt` |

To use a device that has no class, place it directly, for example `freepdk45:nmos_vtg` or
`asap7:nmos_slvt`. On ASAP7 the gate length is fixed at 21 nm by the model cards, so the class
`l` is ignored and `w` becomes a fin count.

## Requires

- [Schemify](https://github.com/OpenFAT/Schemify), native editor.
- The Interop plugin's CLI, `schemify-interop`
  ([OpenFAT/Interoperate](https://github.com/OpenFAT/Interoperate)), on `PATH`. The plugin
  spawns it to import xschem symbols, to export installs, and to scan remote kits. It is
  never linked into the module.
- `ssh` on `PATH` for remote PDKs, and `schemify-interop` on the PDK host for agent mode.

## Install

In your Schemify config (`init.lua`), then `schemify pack sync`:

```lua
schemify.pack.add {
  {
    src = "https://github.com/OpenFAT/PDK-Manager",
    grant = {
      "read_project", "write_project",
      "spawn:ciel", "spawn:schemify-interop",  -- sky130, gf180mcu, ihp-sg13g2
      "spawn:git",                             -- asap7
      "spawn:curl", "spawn:tar",               -- freepdk45
      "spawn:ssh",                             -- remote PDKs
    },
  },
}
```

The plugin gets only the grants that appear both in your list and in its own `schemify.project`,
so leave out the spawns for families you don't use. `schemify pack status` shows anything that
was denied. To pin a commit, add `rev = "<sha>"`.

<details>
<summary>Or load it in a single project, from a checkout</summary>

Add this to the project's `schemify.project`:

```
plugin pdk-manager path <checkout>/pdk_manager.wasm
  grant read_project
  grant write_project
  grant spawn:ciel
  grant spawn:schemify-interop
  grant spawn:git
  grant spawn:curl
  grant spawn:tar
  grant spawn:ssh
```

</details>

> [!NOTE]
> The plugin needs `spawn`, so it only runs in the native editor. It does not work in the
> browser build.

## Use

**View ▸ PDKs** (command `pdk.panel`) opens the panel. It lists installed versions with **Use**,
**Remove**, **→ xschem** and **→ Virtuoso** buttons, the versions available for a family, the
remote PDKs with **Trust**, **Refresh** and **Forget**, and the steps of a running install.

Everything the panel does is also a command, so scripts and the JSON-RPC interface can drive it:

| Command | What it does |
|---|---|
| `pdk.install <family> <version>` | Installs into `.schemify/pdks/<family>/<version>/` |
| `pdk.install <family> <name> <path>` | Installs a kit you already have on disk |
| `pdk.use <family> <version>` | Switches the project to that install (`lib`, `lib tech map`, `set PDK_ROOT`, `set PDK_MODELS`) |
| `pdk.remove <family> <version>` | Removes an install. Refuses the active one and the locked one |
| `pdk.prune`, then `pdk.prune.confirm` | Lists, then removes, every install nothing references |
| `pdk.list_remote [family]` | Refreshes the versions available for a family |
| `pdk.export <family> <version> xschem\|virtuoso` | Exports a local install's cells with `schemify-interop` into `.schemify/pdks/<family>/<version>/export/` (generated families; never a remote one) |
| `pdk.remote.add <family> <user@host:/path> <models> <dialect> [section]` | Records a remote PDK in the committed `pdks.remote` |
| `pdk.remote.trust <family>` | Trusts that exact destination in this checkout (`.schemify/pdk-manager/trusted`) |
| `pdk.remote.refresh <family>` | Scans the kit over `ssh` and makes it the project's PDK |
| `pdk.remote.forget <family>` | Deletes its local cache and its manifest rows |

The full behaviour is specified in Schemify's
[docs/spec/pdk.md](https://github.com/OpenFAT/Schemify/blob/main/docs/spec/pdk.md).

### Remote PDKs

A proprietary kit stays on its host. `pdks.remote` names it, one line per family:

```
remote tsmc28 pdk@pdkhost:/proj/pdk/tsmc28/1.3 models models/spectre/toplevel.scs dialect spectre
```

Refresh runs `schemify-interop pdk-scan` on the host over `ssh` (or, without it there, reads
the kit's structural lines through a script and scans them locally, in memory). Only the
cells' interface (pins, parameter names, formats; no model data) is written, to
`.schemify/pdk-manager/remote/<family>/`, and `set PDK_MODELS` names the corner library's path
on the host. The plugin's ssh options are fixed; nothing in the project can add one. A
destination is used only after you trust it in this checkout, and a changed one asks again.
The design, and what still needs the Schemify core, is in
[docs/remote-pdks.md](docs/remote-pdks.md).

## Develop

The plugin is a Rust crate built for `wasm32-unknown-unknown`. The Schemify crates it uses
(`schemify-abi`, plus `schemify-format` and `schemify-plugin` for tests) are git dependencies,
pinned to one Schemify commit in `Cargo.toml`.

```sh
nix develop                  # Rust 1.95.0 (rust-overlay), wasm32 std, rust-lld, schemify-interop
./build.sh                   # -> pdk_manager.wasm (fails at 200 KB)
cargo test                   # pure modules, then the built guest through the real plugin host
cargo test -- --ignored      # network: real ciel, FreePDK45 and ASAP7 installs simulated in ngspice
```

<details>
<summary>Reproducible module, nightly drift, local Schemify checkout</summary>

`build.sh` produces the same bytes every time under `nix develop`. CI rebuilds the module and
fails if the committed `pdk_manager.wasm` differs, so commit the module together with the source
change, or commit the `pdk-manager-wasm` artifact from the CI run. A nightly job builds and tests
against Schemify `main`. When it goes red, bump `rev` in `Cargo.toml` and fix whatever broke.

To test against a local Schemify checkout without editing `Cargo.toml`:

```sh
cargo test --config 'patch."https://github.com/OpenFAT/Schemify".schemify-abi.path="../Schemify/crates/abi"' \
           --config 'patch."https://github.com/OpenFAT/Schemify".schemify-format.path="../Schemify/crates/format"' \
           --config 'patch."https://github.com/OpenFAT/Schemify".schemify-plugin.path="../Schemify/crates/plugin"'
```

The patch rewrites `Cargo.lock`. Restore it (`git checkout Cargo.lock`) before you commit.

The dev shell (and so CI) has the real `schemify-interop` from the flake's `interop` input
(`github:OpenFAT/Interoperate`, pinned in `flake.lock`; `nix flake update interop` bumps it).
The guest tests put fakes ahead of it on `PATH`; `real_interop_scans_a_remote_kit` runs the
real one behind a fake `ssh`, and skips while it does not speak `pdk-scan`.

`devices/` and `tests/fixtures/schemify/` hold copies of Schemify files (each has a README).
Update them whenever you bump `rev`.

**Adding a family** means adding one row to `RECIPES` in `src/lib.rs` and a tech map in `maps/`.

</details>

## License

[MIT](LICENSE)
