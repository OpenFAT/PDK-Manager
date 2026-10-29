# Remote and proprietary PDKs: feasibility and design

Status: design, 2026-10-18, with the maintainer's decisions (§3). Nothing here is
implemented.

Where references point:
- Spec references are to Schemify's `docs/spec/` (`pdk.md`, `plugin-abi.md`, `run.md`,
  `netlist.md`, `import.md`, `interop.md`, `config.md`, `format.md`, `web.md`).
- Code references are to this repository (`src/`) and to Schemify (`crates/`,
  `plugins/interop/`). Core line numbers are from the checkout at `07234abc`.

Verdict scale: **works today**, **small change** (one owner, no new interface), **core ABI
change** (the plugin ABI or a core crate's contract), **not feasible**.

## 0. Summary

| Question | Verdict |
|---|---|
| The plugin reads a PDK's model files today | No. It parses no model format (§1.1). |
| Reading SPICE dialects (ngspice, HSPICE, Xyce sections, `.include` trees, bins) | Small change in interop's scanner (§1.2). |
| Reading Spectre | Medium change: a new interop reader. No ABI change. |
| Netlisting a Spectre-only PDK | Works today, with `format spectre` on the cells. |
| Simulating Spectre or HSPICE | Through ESPice (`--tokenizer=spectre` or `--tokenizer=hspice`). A small core change: dialect-to-simulator rows, and later an `hspice` dialect (§5). Remote runs use the ssh wrapper (§2b). |
| A plugin contributing cells without files | Not possible today. A core ABI change (a plugin-served library) is needed. It is medium-sized, and smaller than it sounds, because the core already indexes and opens cells from bytes (§4.2). |
| Storing nothing from the PDK on local disk | Feasible with that library: the cells live in RAM for the session (§4). |
| (d) Agent: `ssh host schemify-interop pdk-scan` | The primary path. Small change in interop, plus a static musl release (§4.6). |
| (a) Framed, pre-filtered ssh reads parsed in memory | The fallback. Works today at the ABI level (§2a). |
| (b) Remote simulation over ssh | Works today for ngspice and ESPice through a user-config wrapper. |
| (c) sshfs | Ruled out. |
| A committed destination with a per-user trust prompt | Needs a small core ABI addition: per-user plugin state (§4.4). |
| Web target | Not over ssh. `spawn` is native only. |

## 1. Model formats

### 1.1 What the plugin reads today

- It parses no PDK model format. Device cells are recipe rows (`src/lib.rs:143-195`,
  `RECIPES` at `src/lib.rs:200-288`). For ciel families they come from
  `schemify-interop xschem-import` (`src/lib.rs:411-428`).
- The only code that reads model text is `derive_cmg` (`src/install.rs:281-313`). It copies
  whole model cards into `.schemify/pdks/F/V/models/` (`src/install.rs:107-111`). It must
  never run on a proprietary kit.
- Corners are opaque paths: `set PDK_MODELS …` (`src/lib.rs:705-712`) or a generated
  `corners.lib` (`src/install.rs:194-217`).
- Recipes are compile-time constants. A proprietary family's device list and corner names
  are NDA data, so a remote family's recipe has to come from the scan, never from this
  repository.

### 1.2 The scanner that exists

`plugins/interop/src/spice.rs` reads the ngspice dialect (`spice.rs:1-19`):
- `+` continuation lines and comments;
- `.include`/`.inc`, `.lib f s` and `.lib s … .endl`, nested at most 16 deep and at most
  4096 files (`spice.rs:28-30`);
- `.if` branches;
- binned `.model x.N` cards (`Bins`, `spice.rs:60-70`; `base_name` at `spice.rs:854`);
- `.subckt` names, `.param` and OSDI evidence.

It takes a synchronous loader closure (`spice.rs:314`), so it runs anywhere bytes are at hand.
It has only `std` and Schemify crates as dependencies (`plugins/interop/Cargo.toml`), so it
builds static for musl (§4.6).

Gaps:
- `Subckts` keeps the name, file and line only (`spice.rs:72-81`). It needs the port list and
  the parameter list.
- HSPICE `.subckt` parameters written without `params:`.
- `.option scale` is not applied to bins (`spice.rs:60-61`).
- Protected and encrypted blocks are not skipped.
- There is no Spectre syntax at all.

For encrypted Cadence PDKs the interface comes from Virtuoso instead: `SfyExportLib`
exports CDF, simInfo `termOrder` and spectre formats (`interop.md:198`, `:256`, `:287`).

### 1.3 Per format

| Format | Read (interface) | Netlist | Simulate |
|---|---|---|---|
| ngspice | Small change (subckt ports and params) | Works today | Works today |
| HSPICE (`.lib 'f' tt`, `.inc`, `.model x.N`, `$` comments, `.option scale`) | Small change | Works today through the ngspice dialect when the cards are ngspice-compatible (FreePDK45's HSPICE cards: `src/lib.rs:257-261`, `pdk.md:370`). There is no `hspice` dialect (`netlist.md:73`). | Today: ngspice. ESPice `--tokenizer=hspice`: §5 |
| Xyce | Small change | Works today (`netlist.md:73`) | Core change (`run.md:287`). Not prioritised. |
| Eldo, LTspice `.lib` | Small change (dialect flags) | spice templates | Not planned |
| Spectre (`simulator lang=` switching, `library`/`section`, `include … section=`, `parameters`, `subckt`/`inline subckt`, `model … { 1: … }` bins, `statistics`, `ahdl_include`) | Medium change (a new reader) | Works today (`netlist.md` §7.5 `:120`; corners `include "<p>" section=<s>` §7.6.11 `:149`) | ESPice `--tokenizer=spectre`, a small core change (§5) |
| Encrypted models (HSPICE `.prot`, Spectre `pragma protect`) | Not feasible to read. Use the clear wrapper `subckt` line or Virtuoso CDF. | As the cells allow | Vendor simulator only, remote |

Cells need, per device:
- the cell name and the netlist target (model or subckt name, without the bin suffix);
- the terminals in order (subckt ports, or the implicit `d g s b` of a MOS `.model`);
- the instance parameter names;
- the `format spice` and/or `format spectre` templates.

Defaults, bins and card parameters are not needed (§4.3).

## 2. Transport options (findings)

What the platform provides:
- `spawn:<binary>` is a generic grant and native only (`plugin-abi.md:125`).
- The child inherits the environment, so `SSH_AUTH_SOCK`, Kerberos tickets and
  `~/.ssh/config` all apply (`crates/cli/src/platform.rs:79-87`).
- stdin, stdout and stderr are in-memory pipes, and stdout and stderr are concatenated
  (`platform.rs:93-101`, `plugin-abi.md:211`).
- The child runs on a worker thread with **no timeout and no cancel**
  (`platform.rs:323-341`).
- One event carries at most 64 MiB and guest memory is capped at 256 MiB
  (`plugin-abi.md:195`, `:66`).
- `fs_read` takes project-relative paths only (`:131`).
- The guest is capped at 200 KB (`build.sh:36`), so the scanner is spawned, never linked.
- The panel tail and the status line show spawn output (`src/guest_glue.rs:399-401`,
  `:454-466`). Data-bearing spawns must bypass them.

### (a) Framed, pre-filtered reads parsed in memory: the fallback

- A constant POSIX script reaches the remote side on stdin
  (`ssh … -- <dest> sh -s -- '<abs path>'…`).
- It prints length-framed records of the structural lines only:
  - section, include and subckt lines (with their continuations);
  - the first three words of `.model`/`model`;
  - parameter names with their values stripped.
- The bytes go on in memory, as the stdin of a local `schemify-interop pdk-scan --stdin`.
- Include rounds stop at depth 16.
- Card parameters, bins and defaults never leave the PDK host. Structure and names do pass
  through local RAM. Swap is the residual risk.

### (b) Remote simulation: works today for ngspice and ESPice

- `lib ${PDK_MODELS} tt` with an absolute remote path is written verbatim into the netlist.
  Absolute paths are not rewritten (`crates/run/src/prepare.rs:361-367`), and nothing checks
  that model files exist locally.
- The user-config `sim` row (`config.md:76`, `run.md:288`) can name a wrapper. The wrapper:
  1. copies the job directory to a remote `mktemp -d` (`tar | ssh`);
  2. runs the simulator there;
  3. streams `out.raw` back;
  4. removes the remote directory.

  The runner substitutes `{netlist}`/`{raw}` relative to the job directory
  (`crates/run/src/runner.rs:539-560`).
- Verilog-A in a remote PDK uses a precompiled `veriloga` body
  (`pre_osdi /abs/remote/x.osdi`, `netlist.md:146`), never a `file` impl. A `file` impl
  compiles the source locally (`run.md:290`).

### (c) sshfs: ruled out

- It makes the kit look like local files that every tool can read, including this plugin's
  `local` install, whose `Derive` step copies the cards.
- It needs FUSE on every OS.
- The simulator then runs locally, which many licences forbid.

### (d) Agent on the PDK host: primary (decision 4, §3)

`ssh <dest> schemify-interop pdk-scan …` parses next to the kit and prints the cell records
only. The protocol is in §4.5, distribution in §4.6.

### Web

`spawn` is refused in the browser. The plugin-served library (§4.2) is target-neutral, so a
later `network` plugin could serve one there (`web.md:87-97`; `fetch` is not executed yet,
`crates/app/src/plugins.rs:1088`).

## 3. Decisions (2026-10-18)

1. **Nothing from the PDK is stored locally.** That excludes model text, and also interface
   data (cell names, pins, parameter names, corner names) and fingerprints wherever
   avoidable. Cells are served to the editor in memory each session (§4).
2. **The remote destination may be committed** (team convenience), hardened by:
   - a strict grammar, `--` before the destination, and ssh options set by the plugin only;
   - no `-o` and no ProxyCommand from the project;
   - a per-user trust prompt on first use, stored outside the project;
   - per-user overrides in `~/.ssh/config` and in user state (§4.4).
3. **Spectre and HSPICE support is high priority** (most proprietary PDKs are one or the
   other). Both run through **ESPice** (the maintainer's simulator), remotely over the ssh
   wrapper (§5).
4. **Agent mode (d) is the primary path.** Framed, pre-filtered reads (a) are the fallback
   when the agent binary is absent. A static musl `schemify-interop` is distributed for PDK
   hosts (§4.6).
5. Instances in saved design files that reference remote cell names, with their parameter
   overrides, are the user's own design data and are fine to save and commit.

Decided later the same day, on the questions of the first revision:

6. **Offline: accepted, no cache for now.** With the PDK host unreachable, a design opens, can
   be edited and saved, but cannot be netlisted or simulated.
7. **Run artifacts.**
   - The runner gains a "keep no logs" option (§5.2).
   - The remote wrapper writes the netlist to a temporary file on the PDK host and deletes it
     after the run. ESPice may later read the netlist from stdin instead (E3).
   - The local `netlist.cir` is design data (decision 5).
8. **Trust list: per-user plugin state** (`user_state`, §4.4).
9. **The design-hash digest on disk is acceptable.**
10. **The format gains a numeric parameter whose default is the simulator's** (§4.3). It
    replaces the string-parameter workaround.
11. **Spectre and HSPICE run through ESPice**:
    - `--tokenizer=spectre`, and `--tokenizer=hspice` with a new `hspice` netlist dialect;
    - no new runner and no new result readers, because ESPice writes an ngspice binary raw
      file by default;
    - no PSF reader;
    - ESPice is the CI oracle, and a licensed Spectre is an optional spot check.
12. **Spectre Monte Carlo waits for ESPice work** (E2: `statistics` and `montecarlo` in
    Spectre mode). One job per sample or one Monte Carlo job is decided after that.
13. **The committed destination is its own manifest record**, strictly validated (§4.4).

## 4. Design

### 4.1 What is on disk and where

| Where | What | PDK-derived? |
|---|---|---|
| `schemify.project` (committed) | `lib <F> plugin pdk-manager`, `remote <F> <dest>` (§4.4), `set PDK_MODELS <abs remote path>` | No. The user chose the location, and it is design configuration. |
| Design files (committed) | Instances `<F>:<cell>` with their parameter overrides (D40: overrides only) | The user's design data (decision 5) |
| `<state dir>/plugins/pdk-manager/` (per user, outside every project) | Trusted destinations and per-user path overrides | No |
| `.schemify/runs/<hash>/<job>/` | `netlist.cir`, `out.raw`, logs, the design-hash digest | Design data. With "keep no logs" (decision 7), there are no `stdout.log`/`stderr.log`. |
| Nowhere | Cell files, the `tech` map rows if the user does not write them, corner listings, scan output, fingerprints, model text | |

The plugin writes no `.schemify/pdks/` store for a remote family. It has no `INSTALLS` row, no
`STATE` and no `lib/` directory. `pdk.remove` and `pdk.prune` do not apply. Switching back
to a local family uses `pdk.use` as today.

### 4.2 Core ABI: a plugin-served library

Today a plugin cannot contribute cells. The contribution kinds are `ui commands keymap
annotations functions dialects runners checks constraint_types` (`plugin-abi.md:51`), and
library sources are `path git url map`, plus the implicit builtin `core` (`format.md:166`,
`crates/doc/src/library.rs:28-62`).

The core is already file-agnostic below the app:
- `Project::index_library(name, resolved, files: &[(&str, &[u8])])` indexes from bytes
  (`crates/doc/src/project.rs:483`).
- `Project::open(lib, relpath, bytes, journal)` opens from bytes (`project.rs:768`).
- The builtin `core` library is served from the binary: read-only and never journaled
  (`crates/cli/src/libs.rs:139-150`, `:196-197`).

A plugin-served library is "builtin, with bytes supplied by a plugin at runtime". Minimal
addition:

1. **Manifest.** `lib <name> plugin <plugin-name>` is a new `LibSource::Plugin`
   (`format.md` §4, `library.rs`). It is named explicitly, so the project says which plugin
   it trusts for the library. No D8 last-wins: only the named plugin's set is used.
2. **Contribution kind `library <name>`** (`plugin-abi.md` §1, §7), with the same
   replace semantics as every contribution:

   ```
   contribute library tsmc28
     | version 7f3c…            # opaque, from the agent: changes when the kit changes
     | file nch_lvt.shprim      # each file's bytes follow as `|` verbatim lines
     | …
   ```

   An `unavailable "<reason>"` record in place of the files marks the library as unserved,
   with a reason. Cap: 16 MiB per set (the per-call output cap), instead of 1 MiB per
   contribution (`plugin-abi.md:193`). Cells count against `CELLS_MAX` (2^16).
3. **The app** indexes a composed library set with `index_library(name, "plugin:<p>", files)`.
   - The cells are read-only, like the builtin `core`: opening one uses the set's bytes, it
     is never journaled, and there is no save.
   - A new set re-indexes that library and refreshes symbols, scenes and open documents, as
     `project.reload` does (`pdk.md` §9.2).
   - An empty or unavailable set, or the plugin unloading or being quarantined, removes the
     library.
   - **RAM only.** The app must never write these bytes anywhere: no journal, no
     `.schemify/build/`, no `recent` entry. Their lock status is `Unlocked`, as for
     builtin (`project.rs:509`), so `schemify.lock` holds no row and no hash for them.
4. **E001 reasons** (`netlist.md` §1): `library tsmc28 is served by pdk-manager: waiting`
   before the first set arrives, and `…: unavailable (<reason>)` after one. The checks panel
   lists them. Design files still open: instances keep their records and override lines byte
   for byte (D40) and draw as unresolved. Netlisting and runs are blocked by the existing
   gate (`run.md` §4).
5. **Design hash** (`run.md` §4): it already hashes each reached cell's canonical bytes. Add
   the `version` of each reached plugin library, so a card-only kit change that leaves the
   interface intact still makes results stale. Only the digest is written (the run directory
   name and the results header): a sha256 over the whole design, not the PDK data.
6. **Determinism.** The agent writes each cell with the format writer (canonical) and sorts
   the files by path. The library content hash already ignores listing order
   (`library.rs`, `content_hash` test). The same kit gives the same bytes, and therefore the
   same netlist and the same design hash, on every machine.

Offline: there is no disk cache (decision 1), so with ssh down a design opens, can be edited
and saved, but cannot be netlisted or simulated. `pdk.refresh` retries. A RAM cache lives for
the session only.

### 4.3 What the served cells contain

- Pins in order, the netlist target, parameter names, `format spice` and/or `format spectre`.
- A core MOS drawing for 4-terminal MOS devices. Any other device gets a generated box
  symbol, the same shape as "Symbol from ports" (`plugin-abi.md:218`).
- No defaults, bins or limits. The cells are never on disk, but the rule keeps card data on
  the PDK host.
- **Simulator-default parameters** (decision 10; core format, elab and netlist change F1).
  Today `param <name> <default>` requires a default (`format.md:92`). The new form is
  `param <name> -`, for kind `number` and `int`.
  - **Meaning:** the parameter has no value unless an instance overrides it, and the simulator
    applies the PDK's default.
  - **Overrides:** they stay typed and are evaluated as expressions, as for any number
    parameter (D20).
  - **Templates:** a new hole `{?w}` renders ` w=<value>` when the parameter has a value and
    nothing otherwise, instead of `E008`. A plain `{w}` hole on an unset parameter is still
    `E008`.
  - **Expressions:** one that reads an unset parameter (a `computed` parameter, a `tech.map`
    expression, `valid`) fails with `X004 w has the simulator's default`.
  - **Overridability:** the parameter is never `computed`.
  - **Hashing:** the design hash covers it as written (`-`).
  - **Replaces:** the string-parameter workaround (`interop.md:287`).
- `tech:` retargeting: the user writes the `tech.map` rows (cell names and parameter
  expressions are their own design configuration), committed like any project file.

### 4.4 Destination hardening and trust

The manifest record (decision 13; core format change F2, a new record kind of
`schemify.project` that the core validates on load, `E029` on a bad shape):

```
lib tsmc28 plugin pdk-manager
remote tsmc28 pdk@pdkhost:/proj/pdk/tsmc28/1.3
  models models/spectre/toplevel.scs      # the corner library, relative to the remote path
  dialect spectre                         # spice | hspice | spectre: the agent's reader
set PDK_MODELS /proj/pdk/tsmc28/1.3/models/spectre/toplevel.scs
```

- `remote <lib> <dest>`: `<lib>` must name a `lib … plugin` row.
- At most one `remote` record per library.
- The core checks `<dest>` against the grammar below, and the plugin checks it again before
  any spawn (defence in depth).
- `models` is a relative path made of grammar segments.
- `PDK_MODELS` stays a `set`, because corner paths expand from `set` entries (`run.md` §5).

Grammar:

```
dest  = [user "@"] host ":" path
user  = [a-z_][a-z0-9._-]{0,31}
host  = [A-Za-z0-9][A-Za-z0-9.-]{0,252}          # a name or an ~/.ssh/config alias; no IPv6 literal
path  = "/" seg ("/" seg)*   with  seg = [A-Za-z0-9._+-]+, never "." or "..";  at most 1024 bytes
```

No whitespace, quotes, `$`, `` ` ``, `;`, `|`, `&`, `\`, `*` or `?`, and nothing starts with
`-`. Paths are still single-quoted for the remote shell.

The plugin builds argv from constants plus the validated destination:

```
ssh -T -x -a -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15
    -o ServerAliveCountMax=2 -o ForwardAgent=no -o ClearAllForwardings=yes
    -o PermitLocalCommand=no -o StrictHostKeyChecking=yes -- <user@host> <remote command>
```

- No `-o`, `-F`, `-J` or ProxyCommand ever comes from the project.
- The plugin does not set `ProxyCommand=none`, because a command-line option would override
  the user's own `~/.ssh/config` ProxyJump or ProxyCommand. That file is the user's and is
  trusted.
- `StrictHostKeyChecking=yes`: an unknown host fails with "run `ssh <host>` once in a
  terminal". There is no trust on first use inside the editor.

Trust:
1. A destination read from `schemify.project` is used only after the user confirms it in a
   plugin dialog showing the exact `user@host:/path`. The plugin remembers that in per-user
   state, keyed by the destination string, and never in the project.
2. A malicious project can name a destination but cannot pre-trust it. Changing the
   committed destination asks again.

Per-user overrides:
- host, user, port, jump host and identity go in `~/.ssh/config`: the committed host is
  treated as an alias;
- the path goes in user state (`pdk.remote.path <F> </abs/path>`).

**Core ABI addition (small): per-user plugin state.** A plugin cannot read or write anything
outside the project today (`plugin-abi.md:131`). Add a capability `user_state` with
`state_read(key)` and `state_write(key, bytes)`, inverted like `fs_*`. It is stored by the
app under `<state dir>/plugins/<plugin name>/`, as `recent` is stored
(`crates/app/src/recent.rs:2`), at most 64 KiB, with keys as path-safe words. It holds the
trust list (decision 8). Seeding it from `init.lua` is a later option.

### 4.5 Plugin flow

1. On `Load` and `DocChanged` (manifest), and on `pdk.refresh`:
   - read `schemify.project` and find `lib <F> plugin pdk-manager` and its `remote` record;
   - check the trust state and show the prompt if needed;
   - contribute `library <F> unavailable "waiting"`.
2. Probe the agent: `ssh … -- <dest> schemify-interop pdk-scan --protocol`. The output is
   one line, `pdkscan 1`.
3. **Agent (d):**
   `ssh … -- <dest> schemify-interop pdk-scan --root '<path>' --entry '<models>' --dialect spice|hspice|spectre`.
   - stdout is a line-record file: the `schemify 1 pdkscan` header, `version <hex>`, and one
     `file <relpath>` record per cell with the cell's bytes as verbatim lines. On failure it
     is `error "<text>"`.
   - The agent writes nothing on the PDK host (no cache, no temp files), reads at most 4096
     files, and is bounded in memory.
   - The plugin finds the header (stderr is appended after stdout), checks the protocol, and
     contributes the library set unchanged.
4. **Fallback (a)** when the probe fails: the framed, pre-filtered reads of §2a, piped from
   memory into a local `schemify-interop pdk-scan --stdin` (grant
   `spawn:schemify-interop`). The output is the same.
5. The scan bytes never reach the panel tail or the log. Only a one-line outcome does
   (`tsmc28: 212 cells served`, or the error).

Grants: `read_project`, `spawn:ssh`, `spawn:schemify-interop` (fallback only), `user_state`.
`pdk.use` writes `schemify.project` (`write_project`) only when the user switches families.

### 4.6 Distributing the agent

- Build `schemify-interop` (feature `cli`) for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` in CI. It depends only on `std` and Schemify crates, so the
  binary is static. It runs on old PDK hosts (RHEL 7, glibc 2.17) with no shared
  dependencies.
- Make it byte-reproducible like `plugins/build.sh`: remapped paths, the pinned toolchain,
  `-C target-feature=+crt-static`, `codegen-units=1`, stripped.
- Publish it as a release asset with a sha256, and a `nix build .#schemify-interop-static`
  output.
- Install it without root into `~/bin` or `~/.local/bin` (it has to be on the remote login
  shell's `PATH`, since `ssh host cmd` does not read interactive rc files on every shell), or
  have a site admin put it in a shared tools directory. The plugin never uploads binaries.
- Versioning: `pdk-scan --protocol` prints the protocol version, and the plugin refuses a
  mismatch with "update schemify-interop on <host>".

## 5. Simulators: Spectre and HSPICE through ESPice

ESPice (github.com/OpenFAT/EGSpice) reads three netlist dialects, chosen with `--tokenizer`:
`ngspice`, `hspice` and `spectre` (ESPice `docs/using/dialects.md:3-8`, `cli.md:25`).

In Spectre mode it reads (ESPice `dialects.md:141-170`):
- `parameters`, `model` with `{ 1: … }` bin groups, `subckt` / `inline subckt`;
- `include "file" [section=name]`, and `simulator lang=spice|spectre` switching;
- `save`, `options`, and the `tran`, `dc`, `ac` and `noise` analyses.

It refuses `montecarlo`, `sweep`, `pss`, `hb` and the other analyses, and it reads
`statistics` blocks but sets them aside with a warning.

Its default output is an ngspice binary raw file, which Schemify's reader already parses
(`run.md:288`). It reads no netlist from stdin, and it writes no device operating-point
vectors, so `op()` gives `X006` (`run.md:276`).

So Spectre and HSPICE need no new runner, no new simulator integration and no result reader.
They need a dialect-to-simulator row and, for HSPICE, a netlist dialect.

### 5.1 Order (decided)

| # | Step | Owner | Size |
|---|---|---|---|
| 1 | **ESPice tokenizer rows** (S2, and the HSPICE row of S4 without the dialect) (§5.2) | Core (run, config, cli, app) | Small |
| 2 | **Plugin-served library, `user_state`, remote flow** (C1, C2, F1, F2, I1 to I4, P1) (§4) | Core ABI, interop, plugin | Medium |
| 3 | **E1: device OP vectors in ESPice's raw output.** Unblocks `op()`, OP annotation and gm/ID on every dialect (§5.4). | ESPice, then core run | Medium (ESPice), small (Schemify) |
| 4 | **E2: Spectre `statistics` and `montecarlo`** in ESPice, then the Schemify job model decision (decision 12) | ESPice, then core run | Medium |
| 5 | **The `hspice` netlist dialect** (S4) (§5.3) | Core netlist | Small to medium |
| — | E3: an optional stdin netlist in ESPice; the wrapper's temp file covers it until then | ESPice | Small |

Dropped:
- the separate Spectre and HSPICE runners and their raw readers;
- the PSF reader (S5);
- ngspice's HSPICE compatibility mode as a planned step. A user can still enable it in a
  `.spiceinit` for ngspice runs, but that is not a Schemify item.

### 5.2 Tokenizer rows (step 1)

- **Spectre.** `Dialect::Spectre` becomes runnable:
  - `schemify run --dialect spectre` (`crates/cli/src/main.rs:569`, `run.md:287`), and the
    editor's dialect menu (`sim.dialect.spectre`).
  - The simulator row is a new user-config `sim spectre` (`config.md:76` names four rows
    today). Its default command is
    `espice -b --tokenizer=spectre -r {raw} {netlist}`, with `path`, `args` and `env` as for
    `sim espice`.
  - The job netlist is `netlist.scs`.
  - `relative_paths` extends to `include "<p>" section=<s>` (`prepare.rs:323-327`), so
    relative corner paths resolve as they do for SPICE.
  - The design hash already covers the dialect word (`run.md` §4).
- **HSPICE** (until step 5): `sim hspice` defaults to
  `espice -b --tokenizer=hspice -r {raw} {netlist}` and netlists in the `ngspice` dialect. ESPice's HSPICE
  line rules (`dialects.md:91`) read the ngspice text Schemify emits, apart from the
  differences step 5 removes. This route is for HSPICE-syntax kits.
- **Analyses.** For Spectre they come from the bench's `code` blocks of lang `spectre`,
  verbatim (`netlist.md:149`), within what ESPice accepts (`tran`, `dc`, `ac`, `noise`).
  Sweeps and Monte Carlo are Schemify jobs (`run.md` §2), so ESPice's refusal of the
  `sweep` and `montecarlo` analyses costs nothing until PDK statistics are needed (E2).
  *New bench* writes a `spectre` block (`dcOp dc`) beside the `.op` when the kit's dialect
  is Spectre.
- **Verilog-A.** Spectre loads it with `ahdl_include`, and the generated OSDI cells have no
  `format spectre` (E013, `pdk.md` §2.2). Unchanged.
- **Keep no logs** (decision 7). A `sim` row field `logs = false`:
  - the runner pipes the simulator's stdout and stderr into memory, never into
    `stdout.log`/`stderr.log`;
  - it keeps the last complete line of each, which stays the progress line and the failure
    message (`run.md` §5);
  - the run log view (`run.md`, "Run log view") shows "logs not kept";
  - `command.log` stays.
- **Remote.** The same wrapper as in §2b, per row:

  ```lua
  schemify.sim("spectre", { path = "schemify-ssh-sim",
    args = { "pdkhost", "espice", "-b", "--tokenizer=spectre", "-r", "{raw}", "{netlist}" },
    logs = false })
  ```

  The wrapper:
  1. uploads the job directory to a remote `mktemp -d`, so the netlist exists on the PDK host
     only as a temp file (E3 would make it a pipe);
  2. runs ESPice there, where the `include "/abs/…" section=…` paths resolve;
  3. returns `out.raw`;
  4. deletes the remote directory, even when the run fails (`trap`).
- **Tests.**
  - ESPice is the CI oracle: Spectre and HSPICE benches run in CI, so the Spectre dialect
    gets a simulator oracle beyond its snapshots (`netlist.md:152`).
  - Runner tests: the default rows, `netlist.scs`, relative `include` resolution, and that
    "keep no logs" writes no log files.
  - A licensed Spectre is an optional spot check of the same benches.

### 5.3 The `hspice` dialect (step 5)

- `spice` templates serve `hspice` unless a `format hspice` exists, as they do for `xyce`,
  `cdl` and `espice` (`netlist.md:73`).
- Differences from the ngspice text:
  - corners as `.lib '<path>' <s>`;
  - no `.control` blocks (`E033`, as for Xyce);
  - Verilog-A loaded with `.hdl "<stem>.va"`, as for espice (`netlist.md` §7.2a, and the
    runner's copy step).
- `sim hspice` then netlists in `hspice`. Results are unchanged: ESPice writes an ngspice raw
  file.
- Tests: golden netlists, and ESPice `--tokenizer=hspice` runs of the same benches in CI.

### 5.4 E1 and E2 (ESPice items, then Schemify)

- **E1: device OP vectors.** ESPice writes ngspice-named device vectors (`@<dev>[gm]`, …) in
  its raw output for an operating point. Schemify then lifts the `X006` for espice-run
  dialects (`run.md:276`) and emits the probe `.save` lines (`run.md` §7a) for them. The
  Spectre dialect saves the same quantities with `save <inst>:<param>`, which needs the name
  map's Spectre spelling (`netlist.md:149`). `inline subckt` names a wrapper's inner device
  after the instance. This unblocks `op()`, OP annotation and the gm/ID plugin on every
  dialect.
- **E2: Spectre `statistics` and `montecarlo`.** Once ESPice applies `statistics` blocks and
  runs `montecarlo`, Schemify chooses between one job per sample (today's model: the
  simulator's seed per job, `run.md` §3) and one Monte Carlo job holding many samples
  (decision 12).

## 6. Change list

| # | Owner | Change | Size | Order |
|---|---|---|---|---|
| S2 | Core (run, config, cli, app) | Runnable Spectre dialect: a `sim spectre` row defaulting to ESPice `--tokenizer=spectre`, `netlist.scs`, Spectre corner paths | Small | 1 |
| S4a | Core (run, config) | A `sim hspice` row defaulting to ESPice `--tokenizer=hspice` | Small | 1 |
| L1 | Core (run, config, app) | "Keep no logs" `sim` field | Small | 1 |
| C1 | Core ABI (format, doc, plugin, app) | Plugin-served library: `lib <n> plugin <p>`, contribution `library` (16 MiB), RAM-only read-only indexing and open, re-index on change, E001 reasons, `Unlocked`, the library `version` in the design hash (§4.2) | Medium | 2 |
| C2 | Core ABI (plugin, app) | The `user_state` capability, which holds the trust list (§4.4) | Small | 2 |
| C3 | Core (app) | A timeout on `spawn` (`platform.rs:79-104`) | Small | 2 |
| F1 | Core (format, elab, netlist) | Simulator-default number parameters: `param <n> -`, the `{?n}` hole, `X004` on reads (§4.3) | Small to medium | 2 |
| F2 | Core (format, doc) | The `remote <lib> <dest>` manifest record, validated (§4.4) | Small | 2 |
| I1 | Interop | Scanner: subckt ports and params, HSPICE parameter lists, `.option scale`, skipping protected blocks | Small | 2 |
| I2 | Interop | `pdk-scan` (agent and `--stdin`, the protocol, canonical cells using F1, no defaults or bins) | Small | 2 |
| I3 | Interop | Spectre reader for `pdk-scan` | Medium | 2 |
| I4 | Interop CI | Static musl release, reproducible, with sha256 | Small | 2 |
| P1 | Plugin | Remote family: reading the `remote` record, the argv builder, the trust dialog, the agent probe and fallback, the library contribution, `pdk.refresh`, no store | Small to medium | 2 |
| E1 | ESPice, then core run | Device OP vectors, then `op()`, probes and OP annotation on espice-run dialects | Medium | 3 |
| E2 | ESPice, then core run | Spectre `statistics` and `montecarlo`, then the job-model decision | Medium | 4 |
| S4 | Core (netlist, run) | The `hspice` netlist dialect | Small to medium | 5 |
| E3 | ESPice | Optional netlist on stdin | Small | any |
| D1 | Specs | `pdk.md` (§2.1 remote, §4 panel), `plugin-abi.md` (`library`, `user_state`), `format.md` (`lib … plugin`, `remote`, `param <n> -`), `run.md`/`config.md` (`sim spectre`, `sim hspice`, `logs`), `netlist.md` (`hspice`, `{?n}`) | Small | with each item |

A Spectre PDK end to end needs steps 1 and 2. `op()` and gm/ID on it need step 3.
