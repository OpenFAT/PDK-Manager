//! PDK manager (docs/spec/pdk.md): installs PDKs per project (through `ciel`, a pinned `git`
//! checkout, a sha256-checked download, or a kit already on disk), imports their xschem
//! symbols with `schemify-interop xschem-import` or generates device cells from the recipe, switches the
//! project between versions and technologies by rewriting `schemify.project`, and removes what
//! nothing references.
//!
//! - this module — the pure parts: [`RECIPES`] (spec §2, one row per PDK family), store paths
//!   (§1), the exact argv of the ciel steps (§3), the manifest rewrite of `pdk.use` (§3.1),
//!   which versions are active or locked, and the prune plan;
//! - [`install`] — an install as a list of [`install::Step`]s (§3), and the files it generates:
//!   device cells, the corner library, OSDI model cards (§2.2, §7);
//! - [`remote`] — remote PDKs over SSH (docs/remote-pdks.md): `pdks.remote`, trust, the ssh
//!   argv, the `pdk-scan` reader;
//! - [`panel`] — the docked "PDKs" panel (§4) as UI records over a [`Model`];
//! - on wasm32, the guest glue: commands → I/O chains (`fs_read`, `fs_write`, `fs_remove`,
//!   `spawn`) → model → panel.
//!
//! Six questions: in, command args and I/O completions (manifest, lockfile, the install
//! index, `STATE`/`ROOT` files, child-process output, kit model cards); out, I/O requests and
//! UI/command sets. Rows: ≤ [`INSTALLS_MAX`] installs, ≤ [`REMOTE_MAX`] remote versions shown,
//! ≤ [`install::STEPS_MAX`] steps per install. Lifetime: the plugin session; everything durable
//! is on disk under `.schemify/pdks/`.

pub mod install;
pub mod panel;
pub mod remote;

use schemify_abi::text;

/// Where a family's kit comes from (spec §2 "source").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `ciel enable` of an open_pdks build. Any ciel version id installs.
    Ciel {
        /// ciel's `--pdk-family` (`ihp-sg13` for the `ihp-sg13g2` family: ciel names the
        /// family, not the variant).
        family: &'static str,
        /// The variant directory under ciel's PDK root (`sky130A`, `set PDK`).
        variant: &'static str,
        /// ciel `-l` libraries; empty = ciel's default set.
        libs: &'static [&'static str],
    },
    /// `git clone` + `git checkout <rev>` into the version's `root/`. The version id is the
    /// rev, and only the pinned rev installs (the recipe's paths and devices were checked
    /// against it).
    Git {
        /// Clone URL.
        url: &'static str,
        /// The pinned commit (40 hex digits).
        rev: &'static str,
    },
    /// A `.tar.gz` downloaded with `curl`, refused unless its sha256 matches, unpacked with
    /// `tar` into `root/` without its top directory. The version id is the first
    /// [`URL_VERSION_HEX`] hex digits of the sha256.
    Url {
        /// Download URL.
        url: &'static str,
        /// Lowercase hex sha256 of the archive.
        sha256: &'static str,
    },
}

/// Hex digits of a `url` source's sha256 that name its version.
pub const URL_VERSION_HEX: usize = 12;

/// A pinned git checkout a recipe needs besides the kit (ASAP7's BSIM-CMG Verilog-A source).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkout {
    /// Clone URL.
    pub url: &'static str,
    /// The pinned commit.
    pub rev: &'static str,
    /// Directory under the version directory it is checked out into.
    pub dir: &'static str,
    /// The Verilog-A file the device cells compile, relative to [`Checkout::dir`].
    pub source: &'static str,
}

/// One generated device cell (spec §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    /// Cell name in the generated library (`nmos_vtl`).
    pub cell: &'static str,
    /// The kit's `.model` name (`NMOS_VTL`).
    pub model: &'static str,
    /// P-channel: the `core:pmos4` pins and drawing; else `core:nmos4`'s.
    pub pmos: bool,
}

/// Where a family's device cells come from (spec §2, §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Devices {
    /// `schemify-interop xschem-import` of the kit's xschem symbols (`<root>/<variant>/libs.tech/xschem`).
    Import,
    /// Generated M-element cells sized by `w`, `l` (metres) and `m` (BSIM4 cards).
    Mos {
        /// The cells.
        cells: &'static [Device],
        /// Default `w` of a placed cell (the kit's minimum).
        w: &'static str,
        /// Default `l` of a placed cell (the kit's minimum).
        l: &'static str,
    },
    /// Generated N-element (OSDI) cells sized by `nfin` and `m`, compiled from `va` by the
    /// runner; the kit's level-72 cards are derived for that module at install
    /// ([`install::derive_cmg`]).
    FinOsdi {
        /// The cells.
        cells: &'static [Device],
        /// The BSIM-CMG Verilog-A checkout.
        va: Checkout,
    },
}

/// How a family's model files are organised (spec §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Models {
    /// One file with `.lib` sections: `set PDK_MODELS ${PDK_ROOT}/<variant>/<file>`.
    Sections {
        /// The file under the variant.
        file: &'static str,
        /// Its section names in `tt ff ss sf fs` order.
        corners: [&'static str; 5],
    },
    /// Files per corner, under the kit root. The install writes `models/corners.lib` with one
    /// `.lib <corner>` section per row including that corner's files, and `set PDK_MODELS`
    /// names it, so a bench corner reads `lib ${PDK_MODELS} tt` whatever the kit.
    Files(&'static [(&'static str, &'static [&'static str])]),
}

/// One PDK family (pdk spec §2). Adding a family is adding a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recipe {
    /// Family name: the store directory, the generated library and the name commands take.
    pub family: &'static str,
    /// Where the kit comes from.
    pub source: Source,
    /// Where its device cells come from.
    pub devices: Devices,
    /// How its models are organised.
    pub models: Models,
    /// The shipped `tech.map` (spec §6), written into the store at install.
    pub map: &'static str,
}

const fn dev(cell: &'static str, model: &'static str, pmos: bool) -> Device {
    Device { cell, model, pmos }
}

const FREEPDK45_CELLS: [Device; 8] = [
    dev("nmos_vtl", "NMOS_VTL", false),
    dev("pmos_vtl", "PMOS_VTL", true),
    dev("nmos_vtg", "NMOS_VTG", false),
    dev("pmos_vtg", "PMOS_VTG", true),
    dev("nmos_vth", "NMOS_VTH", false),
    dev("pmos_vth", "PMOS_VTH", true),
    dev("nmos_thkox", "NMOS_THKOX", false),
    dev("pmos_thkox", "PMOS_THKOX", true),
];

/// The per-device cards of one FreePDK45 corner directory (`models_nom|ff|ss`).
macro_rules! freepdk45_corner {
    ($dir:literal) => {
        &[
            freepdk45_card!($dir, "NMOS_VTL"),
            freepdk45_card!($dir, "PMOS_VTL"),
            freepdk45_card!($dir, "NMOS_VTG"),
            freepdk45_card!($dir, "PMOS_VTG"),
            freepdk45_card!($dir, "NMOS_VTH"),
            freepdk45_card!($dir, "PMOS_VTH"),
            freepdk45_card!($dir, "NMOS_THKOX"),
            freepdk45_card!($dir, "PMOS_THKOX"),
        ]
    };
}

macro_rules! freepdk45_card {
    ($dir:literal, $model:literal) => {
        concat!(
            "FreePDK45/ncsu_basekit/models/hspice/tran_models/",
            $dir,
            "/",
            $model,
            ".inc"
        )
    };
}

const ASAP7_CELLS: [Device; 8] = [
    dev("nmos_rvt", "nmos_rvt", false),
    dev("pmos_rvt", "pmos_rvt", true),
    dev("nmos_lvt", "nmos_lvt", false),
    dev("pmos_lvt", "pmos_lvt", true),
    dev("nmos_slvt", "nmos_slvt", false),
    dev("pmos_slvt", "pmos_slvt", true),
    dev("nmos_sram", "nmos_sram", false),
    dev("pmos_sram", "pmos_sram", true),
];

/// The families (spec §2). ciel rows: section names checked against the upstream model files
/// and installed trees (spec Q1, ciel 3.0.1, 2026-10-12). freepdk45 and asap7: paths, model
/// names and an inverter DC sweep in ngspice checked at the pinned revisions.
pub const RECIPES: [Recipe; 5] = [
    Recipe {
        family: "sky130",
        source: Source::Ciel {
            family: "sky130",
            variant: "sky130A",
            libs: &["sky130_fd_pr"],
        },
        devices: Devices::Import,
        models: Models::Sections {
            file: "libs.tech/combined/sky130.lib.spice",
            corners: ["tt", "ff", "ss", "sf", "fs"],
        },
        map: include_str!("../maps/sky130.map"),
    },
    Recipe {
        family: "gf180mcu",
        source: Source::Ciel {
            family: "gf180mcu",
            variant: "gf180mcuD",
            libs: &["gf180mcu_fd_pr"],
        },
        devices: Devices::Import,
        models: Models::Sections {
            file: "libs.tech/ngspice/sm141064.ngspice",
            corners: ["typical", "ff", "ss", "sf", "fs"],
        },
        map: include_str!("../maps/gf180mcu.map"),
    },
    Recipe {
        family: "ihp-sg13g2",
        source: Source::Ciel {
            family: "ihp-sg13",
            variant: "ihp-sg13g2",
            libs: &[],
        },
        devices: Devices::Import,
        models: Models::Sections {
            file: "libs.tech/ngspice/models/cornerMOSlv.lib",
            corners: ["mos_tt", "mos_ff", "mos_ss", "mos_sf", "mos_fs"],
        },
        map: include_str!("../maps/ihp-sg13g2.map"),
    },
    // NCSU's own download needs a registration; this is a mirror of the v1.4 release
    // (Apache-2.0), fetched as GitHub's archive of a pinned commit. A registered copy installs
    // in place with `pdk.install freepdk45 <name> <path>` (a `local` kit).
    Recipe {
        family: "freepdk45",
        source: Source::Url {
            url: "https://codeload.github.com/baichen318/FreePDK45/tar.gz/a75cb116a3efd1d3ae05b695285452e6ffbc7ca5",
            sha256: "5b93a0b1e12d30ae1fba27e86ac0fb61a3e61f8129813e2503824429d05a532e",
        },
        devices: Devices::Mos {
            cells: &FREEPDK45_CELLS,
            w: "90n",
            l: "50n",
        },
        models: Models::Files(&[
            ("tt", freepdk45_corner!("models_nom")),
            ("ff", freepdk45_corner!("models_ff")),
            ("ss", freepdk45_corner!("models_ss")),
        ]),
        map: include_str!("../maps/freepdk45.map"),
    },
    Recipe {
        family: "asap7",
        source: Source::Git {
            url: "https://github.com/The-OpenROAD-Project/asap7_pdk_r1p7",
            rev: "58d72c9d291e186a77468586ab0c43d8a21eda6a",
        },
        devices: Devices::FinOsdi {
            cells: &ASAP7_CELLS,
            // BSIM-CMG 111.2.1 (ECL-2.0); ngspice has no built-in level 72.
            va: Checkout {
                url: "https://github.com/dwarning/VA-Models",
                rev: "6099b4e12e6f66ff530f59b8d01fb909094be85a",
                dir: "va",
                source: "code/bsimcmg/vacode/bsimcmg.va",
            },
        },
        // The dated files: `7nm_TT.pm` and its siblings are symlinks.
        models: Models::Files(&[
            ("tt", &["models/hspice/7nm_TT_160803.pm"]),
            ("ff", &["models/hspice/7nm_FF_160803.pm"]),
            ("ss", &["models/hspice/7nm_SS_160803.pm"]),
        ]),
        map: include_str!("../maps/asap7.map"),
    },
];

/// A `(family, version)` pair: one install.
pub type Fv = (String, String);

/// Most installs the index tracks.
pub const INSTALLS_MAX: usize = 256;
/// Most remote versions the panel lists (newest first, as ciel prints them).
pub const REMOTE_MAX: usize = 24;
/// The store (spec §1).
pub const STORE: &str = ".schemify/pdks";
/// The install index: one `<family> <version>` line per install started (the plugin cannot
/// list directories, so it records what it created).
pub const INDEX: &str = ".schemify/pdks/INSTALLS";
/// Most bytes of a `local` kit path.
pub const LOCAL_PATH_MAX: usize = 1024;

/// Returns the recipe of `family`.
pub fn recipe(family: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.family == family)
}

/// The one version a `git` or `url` recipe installs (its rev, or its sha256 prefix); `None`
/// for ciel, which installs any version ciel lists.
pub fn pinned_version(r: &Recipe) -> Option<&'static str> {
    let v = match r.source {
        Source::Ciel { .. } => None,
        Source::Git { rev, .. } => Some(rev),
        Source::Url { sha256, .. } => Some(&sha256[..URL_VERSION_HEX]),
    };
    assert!(v.is_none_or(valid_version));
    v
}

/// Whether `v` may name a version: a ciel version id (letters, digits, `.`, `_`, `-`; at
/// most 64 bytes), never `latest`, never a path.
pub fn valid_version(v: &str) -> bool {
    let ok = !v.is_empty()
        && v.len() <= 64
        && v != "latest"
        && !v.starts_with(['.', '-'])
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    assert!(!ok || !v.contains('/'));
    ok
}

/// Why `path` cannot be the root of a `local` kit of `r`, or `None` when it can (spec §2.1):
/// empty, longer than [`LOCAL_PATH_MAX`], a control character, a `.schemify` segment (a kit
/// in the store could be deleted by `pdk.remove`), or — for a recipe that reads the kit's
/// cards at install ([`Devices::FinOsdi`]) — not project-relative (`fs_read` takes only
/// those: no leading `/`, no `\` or `:`, no empty, `.` or `..` segment).
pub fn local_refusal(r: &Recipe, path: &str) -> Option<&'static str> {
    let segments = || path.split(['/', '\\']);
    let why = if path.is_empty() || path.len() > LOCAL_PATH_MAX {
        Some("a local kit path is 1 to 1024 bytes")
    } else if path.chars().any(char::is_control) {
        Some("a local kit path has no control characters")
    } else if segments().any(|s| s == ".schemify") {
        Some("a local kit is never inside .schemify (pdk.remove would delete it)")
    } else if matches!(r.devices, Devices::FinOsdi { .. })
        && (path.starts_with('/')
            || path.contains(['\\', ':'])
            || segments().any(|s| matches!(s, "" | "." | "..")))
    {
        Some("this family reads the kit's model cards: give a project-relative path")
    } else {
        None
    };
    assert!(why.is_some() || !path.is_empty());
    why
}

/// `.schemify/pdks/<family>/<version>`.
pub fn version_dir(family: &str, version: &str) -> String {
    assert!(recipe(family).is_some() && valid_version(version));
    format!("{STORE}/{family}/{version}")
}

/// `ciel ls-remote --pdk-family <family>`.
///
/// # Panics
///
/// If `r` is not a ciel recipe.
pub fn ls_remote_argv(r: &Recipe) -> Vec<String> {
    let Source::Ciel { family, .. } = r.source else {
        panic!("{} is not a ciel family", r.family)
    };
    ["ciel", "ls-remote", "--pdk-family", family]
        .map(str::to_owned)
        .to_vec()
}

/// `ciel enable --pdk-family F --pdk-root <dir>/root [-l LIB]… V`: fetches the version and
/// links `<root>/<variant>` to it (spec §3 step 1).
///
/// # Panics
///
/// If `r` is not a ciel recipe.
pub fn fetch_argv(r: &Recipe, version: &str) -> Vec<String> {
    let Source::Ciel { family, libs, .. } = r.source else {
        panic!("{} is not a ciel family", r.family)
    };
    let root = format!("{}/root", version_dir(r.family, version));
    let mut argv: Vec<String> = ["ciel", "enable", "--pdk-family", family, "--pdk-root"]
        .map(str::to_owned)
        .to_vec();
    argv.push(root);
    for l in libs {
        argv.push("-l".to_owned());
        argv.push((*l).to_owned());
    }
    argv.push(version.to_owned());
    assert_eq!(argv.len(), 7 + 2 * libs.len());
    argv
}

/// `schemify-interop xschem-import <root>/<variant>/libs.tech/xschem -o <dir>/lib --xschemrc …/xschemrc`
/// (spec §3 step 2); `root` is the version's `root/`, or a `local` kit's path.
///
/// # Panics
///
/// If `r` is not a ciel recipe.
pub fn import_argv(r: &Recipe, version: &str, root: &str) -> Vec<String> {
    let Source::Ciel { variant, .. } = r.source else {
        panic!("{} is not a ciel family", r.family)
    };
    let dir = version_dir(r.family, version);
    let xschem = format!("{root}/{variant}/libs.tech/xschem");
    let argv = vec![
        "schemify-interop".to_owned(),
        "xschem-import".to_owned(),
        xschem.clone(),
        "-o".to_owned(),
        format!("{dir}/lib"),
        "--xschemrc".to_owned(),
        format!("{xschem}/xschemrc"),
    ];
    assert_eq!(argv[..2], ["schemify-interop", "xschem-import"]);
    argv
}

/// The simulator dialects `pdk.export` writes a local install's models in. The store is the
/// kit's own files (`ROOT`, or `<dir>/root`) read from [`model_entries`] in order, converted
/// on demand: the source is the only lossless form.
pub const MODEL_DIALECTS: [&str; 5] = ["spice", "hspice", "spectre", "vacask", "xyce"];

/// The kit's model files relative to its root: the sectioned file, or every corner's files.
pub fn model_entries(r: &Recipe) -> Vec<String> {
    match (r.source, r.models) {
        (Source::Ciel { variant, .. }, Models::Sections { file, .. }) => {
            vec![format!("{variant}/{file}")]
        }
        (_, Models::Sections { file, .. }) => vec![file.to_owned()],
        (_, Models::Files(corners)) => {
            let mut v: Vec<String> = Vec::new();
            for f in corners.iter().flat_map(|c| c.1.iter()) {
                if !v.iter().any(|x| x == f) {
                    v.push((*f).to_owned());
                }
            }
            v
        }
    }
}

/// `schemify-interop pdk-models --from <from> --to <to> --root <root> --entry … -o <o>`.
fn models_argv(r: &Recipe, root: &str, from: &str, to: &str, o: String) -> Vec<String> {
    let mut a = install::argv(&["schemify-interop", "pdk-models", "--root", root]);
    for e in model_entries(r) {
        a.extend(["--entry".to_owned(), e]);
    }
    a.extend(install::argv(&["--from", from, "--to", to, "-o"]));
    a.push(o);
    a
}

/// `pdk.export <family> <version> <dialect>`: the kit's model files under `root` (read as
/// ngspice, as Schemify runs them) in [`MODEL_DIALECTS`] `to`, into `<dir>/export/<to>`.
pub fn models_export_argv(r: &Recipe, version: &str, root: &str, to: &str) -> Vec<String> {
    assert!(MODEL_DIALECTS.contains(&to));
    let o = format!("{}/export/{to}", version_dir(r.family, version));
    models_argv(r, root, "spice", to, o)
}

/// `to` is `xschem` or `virtuoso`: `schemify-interop xschem-export|virtuoso-export <dir>/lib .
/// -o ../export/xschem|../export/<family>.sfyx`. The install's library directory is the
/// project (its `schemify.project` names the library; the paths after it are relative to
/// it), and `.` names every cell file under it (Interop expands a directory), so imported
/// families export too: the plugin cannot list their cells itself. Model dialects:
/// [`models_export_argv`].
pub fn export_argv(r: &Recipe, version: &str, to: &str) -> Vec<String> {
    let (cmd, o) = match to {
        "xschem" => ("xschem-export", "../export/xschem".to_owned()),
        "virtuoso" => ("virtuoso-export", format!("../export/{}.sfyx", r.family)),
        _ => panic!("export to xschem or virtuoso, not {to}"),
    };
    vec![
        "schemify-interop".to_owned(),
        cmd.to_owned(),
        format!("{}/lib", version_dir(r.family, version)),
        ".".to_owned(),
        "-o".to_owned(),
        o,
    ]
}

/// Version ids in `ciel ls-remote` output: every 40-digit hex word, first occurrence order.
pub fn parse_versions(out: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for word in out.split(|c: char| !c.is_ascii_hexdigit()) {
        if word.len() == 40 && !v.iter().any(|x| x == word) && v.len() < REMOTE_MAX {
            v.push(word.to_ascii_lowercase());
        }
    }
    assert!(v.len() <= REMOTE_MAX);
    v
}

/// Parses the install index (`<family> <version>` per line; malformed lines are skipped).
pub fn parse_index(text: &str) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    for line in text.lines().take(INSTALLS_MAX * 2) {
        let mut w = line.split_whitespace();
        if let (Some(f), Some(v), None) = (w.next(), w.next(), w.next())
            && recipe(f).is_some()
            && valid_version(v)
            && !rows.iter().any(|r| r.0 == f && r.1 == v)
            && rows.len() < INSTALLS_MAX
        {
            rows.push((f.to_owned(), v.to_owned()));
        }
    }
    rows
}

/// Writes the install index (sorted, so the file is deterministic).
pub fn write_index(rows: &[(String, String)]) -> String {
    let mut sorted = rows.to_vec();
    sorted.sort();
    sorted.dedup();
    let text: String = sorted.iter().map(|(f, v)| format!("{f} {v}\n")).collect();
    assert_eq!(text.lines().count(), sorted.len());
    text
}

/// The `(family, version)` a store path names, if it is under [`STORE`] (`…/.schemify/pdks/F/V/…`;
/// a lockfile row may hold the absolute path).
pub fn store_version(path: &str) -> Option<(String, String)> {
    let at = path.find(".schemify/pdks/")?;
    let mut seg = path[at + ".schemify/pdks/".len()..].split('/');
    let (f, v) = (seg.next()?, seg.next()?);
    (recipe(f).is_some() && valid_version(v)).then(|| (f.to_owned(), v.to_owned()))
}

/// One record of a line-record file as the manifest rewrite needs it (format spec §2): its
/// word, its unescaped fields, and its text lines as written (comments before it, the record
/// line, its attribute and verbatim lines), so a kept record is copied byte for byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    /// The record word.
    pub word: String,
    /// Its positional values, unescaped.
    pub fields: Vec<String>,
    /// Its lines: leading comments, the record line, indented lines.
    pub lines: Vec<String>,
}

/// Most records a manifest or lockfile may hold here.
pub const BLOCKS_MAX: usize = 1 << 12;

/// Splits a line-record file of header kind `kind` into its header lines and its records.
///
/// ponytail: a lexical split (record words and fields, quoting through `abi::text`), not the
/// full format reader: the guest stays small, and the app re-parses the manifest on reload,
/// reporting what this would have missed.
///
/// # Errors
///
/// A wrong header, a record line that is not a word with values, an indented line before
/// any record, or more than [`BLOCKS_MAX`] records.
pub fn blocks(text: &str, kind: &str) -> Result<(Vec<String>, Vec<Block>), String> {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    let header_ok = first.split_whitespace().collect::<Vec<_>>() == ["schemify", "1", kind];
    if !header_ok {
        return Err(format!("not a `{kind}` file"));
    }
    let mut header = vec![first.to_owned()];
    let mut out: Vec<Block> = Vec::new();
    let mut comments: Vec<String> = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            comments.push(trimmed.to_owned());
            continue;
        }
        if line.starts_with([' ', '\t']) {
            match out.last_mut() {
                Some(b) => b.lines.push(line.to_owned()),
                None if comments.is_empty() => header.push(line.to_owned()),
                None => return Err("an indented line after a comment".to_owned()),
            }
            continue;
        }
        let (word, rest) = text::split_word(line).map_err(|_| format!("not a record: `{line}`"))?;
        let mut fields = Vec::new();
        for v in text::values(rest) {
            fields.push(
                v.map_err(|_| format!("bad value in `{line}`"))?
                    .into_owned(),
            );
        }
        if out.len() >= BLOCKS_MAX {
            return Err(format!("more than {BLOCKS_MAX} records"));
        }
        let mut lines = std::mem::take(&mut comments);
        lines.push(line.to_owned());
        out.push(Block {
            word: word.to_owned(),
            fields,
            lines,
        });
    }
    assert!(!header.is_empty());
    Ok((header, out))
}

/// Writes header lines and records in the canonical layout: one blank line after the header
/// and between consecutive records with different words.
pub fn write_blocks(header: &[String], blocks: &[Block]) -> String {
    let mut t = String::new();
    for l in header {
        t.push_str(l);
        t.push('\n');
    }
    for (i, b) in blocks.iter().enumerate() {
        if i == 0 || blocks[i - 1].word != b.word {
            t.push('\n');
        }
        for l in &b.lines {
            t.push_str(l);
            t.push('\n');
        }
    }
    assert!(t.ends_with('\n'));
    t
}

/// A new one-line record `word field…` (values quoted as needed).
pub fn record(word: &str, fields: &[&str]) -> Block {
    let mut line = word.to_owned();
    for f in fields {
        line.push(' ');
        text::push_value(&mut line, f);
    }
    Block {
        word: word.to_owned(),
        fields: fields.iter().map(|f| (*f).to_owned()).collect(),
        lines: vec![line],
    }
}

/// Versions the manifest uses (`active`: a `lib` record whose source path is in the store)
/// and the lockfile pins (`locked`), each sorted and deduplicated.
///
/// # Errors
///
/// A file that does not split into records ([`blocks`]), as text.
pub fn references(manifest: &[u8], lock: Option<&[u8]>) -> Result<(Vec<Fv>, Vec<Fv>), String> {
    let utf8 = |b: &[u8], what: &str| {
        String::from_utf8(b.to_vec()).map_err(|_| format!("{what}: not UTF-8"))
    };
    let (_, m) = blocks(&utf8(manifest, "schemify.project")?, "project")
        .map_err(|e| format!("schemify.project: {e}"))?;
    let mut active: Vec<(String, String)> = m
        .iter()
        .filter(|b| b.word == "lib")
        .filter_map(|b| b.fields.get(2).and_then(|p| store_version(p)))
        .collect();
    let mut locked = Vec::new();
    if let Some(lock) = lock {
        let (_, l) = blocks(&utf8(lock, "schemify.lock")?, "lock")
            .map_err(|e| format!("schemify.lock: {e}"))?;
        locked = l
            .iter()
            .filter(|b| b.word == "locked")
            .filter_map(|b| b.fields.get(3).and_then(|p| store_version(p)))
            .collect();
    }
    for v in [&mut active, &mut locked] {
        v.sort();
        v.dedup();
    }
    Ok((active, locked))
}

/// Why `pdk.remove F V` is refused, or `None` when it may proceed (spec §3).
pub fn remove_refusal(
    fv: &(String, String),
    active: &[(String, String)],
    locked: &[(String, String)],
) -> Option<&'static str> {
    if active.contains(fv) {
        Some("active in schemify.project: use another version first")
    } else if locked.contains(fv) {
        Some("pinned by schemify.lock")
    } else {
        None
    }
}

/// Rows of `installs` that `pdk.prune` removes: every install neither active nor locked
/// (complete or not). An active or locked install is never removed, even when incomplete.
pub fn prune_plan(
    installs: &[(String, String)],
    active: &[(String, String)],
    locked: &[(String, String)],
) -> Vec<usize> {
    let plan: Vec<usize> = (0..installs.len())
        .filter(|&i| remove_refusal(&installs[i], active, locked).is_none())
        .collect();
    assert!(plan.iter().all(|&i| !active.contains(&installs[i])));
    plan
}

/// The manifest after `pdk.use F V` (spec §3.1), canonical. Drops every `lib` whose source is
/// in the store, `lib tech`, and the `set` entries `pdk.use` owns (`PDK_ROOT`, `PDK`,
/// `PDK_MODELS`, and any whose value names `PDK_ROOT` or the store); then appends the import's
/// libraries (paths prefixed with `<dir>/lib/`), `lib tech map <dir>/tech.map`,
/// `set PDK_ROOT <root>` (`<dir>/root`, or a `local` kit's path), `set PDK <variant>` (ciel
/// families), `set PDK_MODELS` (the sectioned models file, or the generated
/// `<dir>/models/corners.lib`) and the import's `set` entries. Everything else, comments
/// included, is kept in order and byte for byte.
///
/// # Errors
///
/// A file that does not split into records or is not a manifest, as text.
pub fn use_manifest(
    manifest: &[u8],
    import: &[u8],
    r: &Recipe,
    version: &str,
    root: &str,
) -> Result<String, String> {
    let dir = version_dir(r.family, version);
    assert!(!root.is_empty());
    let utf8 = |b: &[u8], what: &str| {
        String::from_utf8(b.to_vec()).map_err(|_| format!("{what}: not UTF-8"))
    };
    let (header, old) = blocks(&utf8(manifest, "schemify.project")?, "project")
        .map_err(|e| format!("schemify.project: {e}"))?;
    let (_, imp) = blocks(&utf8(import, "import manifest")?, "project")
        .map_err(|e| format!("{dir}/lib/schemify.project: {e}"))?;
    let mut out: Vec<Block> = old.iter().filter(|b| !pdk_owned(b)).cloned().collect();
    let kept = out.len();
    for b in imp
        .iter()
        .filter(|b| b.word == "lib" && field(b, 1) == "path")
    {
        out.push(record(
            "lib",
            &[field(b, 0), "path", &format!("{dir}/lib/{}", field(b, 2))],
        ));
    }
    out.push(record("lib", &["tech", "map", &format!("{dir}/tech.map")]));
    out.push(record("set", &["PDK_ROOT", root]));
    let models = match (r.source, r.models) {
        (Source::Ciel { variant, .. }, Models::Sections { file, .. }) => {
            out.push(record("set", &["PDK", variant]));
            format!("${{PDK_ROOT}}/{variant}/{file}")
        }
        _ => format!("{dir}/{}", install::CORNERS_LIB),
    };
    out.push(record("set", &["PDK_MODELS", &models]));
    for b in imp
        .iter()
        .filter(|b| b.word == "set" && !matches!(field(b, 0), "PDK_ROOT" | "PDK" | "PDK_MODELS"))
    {
        out.push(record("set", &[field(b, 0), field(b, 1)]));
    }
    assert!(out.len() > kept);
    Ok(write_blocks(&header, &out))
}

fn field(b: &Block, i: usize) -> &str {
    b.fields.get(i).map_or("", String::as_str)
}

/// The records `pdk.use` (and a remote refresh) own: every `lib` in the store or the remote
/// cache, `lib tech`, `set PDK_ROOT|PDK|PDK_MODELS`, and every `set` naming `PDK_ROOT` or the
/// store.
fn pdk_owned(b: &Block) -> bool {
    match b.word.as_str() {
        "lib" => {
            field(b, 0) == "tech"
                || field(b, 2).starts_with(".schemify/pdks/")
                || field(b, 2).starts_with(remote::CACHE)
        }
        "set" => {
            matches!(field(b, 0), "PDK_ROOT" | "PDK" | "PDK_MODELS")
                || field(b, 1).contains("PDK_ROOT")
                || field(b, 1).contains(".schemify/pdks/")
        }
        _ => false,
    }
}

fn manifest_blocks(manifest: &[u8]) -> Result<(Vec<String>, Vec<Block>), String> {
    let text = std::str::from_utf8(manifest).map_err(|_| "schemify.project: not UTF-8")?;
    blocks(text, "project").map_err(|e| format!("schemify.project: {e}"))
}

/// The manifest after `pdk.remote.refresh` of `family` (temporary, until the core serves
/// plugin libraries: docs/remote-pdks.md "Implemented"), canonical: the family becomes the
/// project's PDK as with `pdk.use`. Drops what [`pdk_owned`] names (keeping a `lib tech`
/// outside `.schemify/`: the user's own map for the remote cells), then appends `lib <family>
/// path .schemify/pdk-manager/remote/<family>` and `set PDK_MODELS <models>` (the corner
/// library's absolute path on the PDK host, written verbatim into netlists).
///
/// # Errors
///
/// A file that does not split into records or is not a manifest, as text.
pub fn remote_manifest(manifest: &[u8], family: &str, models: &str) -> Result<String, String> {
    let (header, old) = manifest_blocks(manifest)?;
    let users_map = |b: &Block| {
        b.word == "lib" && field(b, 0) == "tech" && !field(b, 2).starts_with(".schemify/")
    };
    let mut out: Vec<Block> = old
        .into_iter()
        .filter(|b| !pdk_owned(b) || users_map(b))
        .collect();
    out.push(record("lib", &[family, "path", &remote::cache_dir(family)]));
    out.push(record("set", &["PDK_MODELS", models]));
    Ok(write_blocks(&header, &out))
}

/// The manifest after `pdk.remote.forget <family>`: without its `lib` row and, when that
/// row was there, without `set PDK_MODELS`. `None` when the manifest has no such row.
///
/// # Errors
///
/// A file that does not split into records or is not a manifest, as text.
pub fn forget_manifest(manifest: &[u8], family: &str) -> Result<Option<String>, String> {
    let (header, old) = manifest_blocks(manifest)?;
    let dir = remote::cache_dir(family);
    let ours = |b: &Block| b.word == "lib" && field(b, 2) == dir;
    if !old.iter().any(ours) {
        return Ok(None);
    }
    let models = |b: &Block| b.word == "set" && field(b, 0) == "PDK_MODELS";
    let out: Vec<Block> = old.into_iter().filter(|b| !ours(b) && !models(b)).collect();
    Ok(Some(write_blocks(&header, &out)))
}

/// What the panel and the commands know: installs with their state, references, the
/// remote listing, the running job.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Model {
    /// The panel is shown.
    pub open: bool,
    /// Installs from the index.
    pub installs: Vec<(String, String)>,
    /// Per install: `Some(true)` when `STATE` reads `ok`, `Some(false)` incomplete, `None`
    /// not read yet.
    pub complete: Vec<Option<bool>>,
    /// Versions the manifest uses.
    pub active: Vec<(String, String)>,
    /// Versions the lockfile pins.
    pub locked: Vec<(String, String)>,
    /// The family the "Available" section lists.
    pub family: String,
    /// Its remote versions (`ls-remote`).
    pub remote: Vec<String>,
    /// The running install, if any.
    pub job: Option<Job>,
    /// The prune waiting for `pdk.prune confirm`: the installs it removes.
    pub prune: Option<Vec<(String, String)>>,
    /// The last outcome, for the panel's status row.
    pub message: String,
    /// The `pdks.remote` records.
    pub remotes: Vec<remote::Remote>,
    /// The destinations trusted in this checkout.
    pub trusted: Vec<String>,
    /// The remote family being scanned, if any.
    pub scanning: Option<String>,
    /// The last remote operation's one-line outcome.
    pub remote_outcome: String,
}

/// One install in progress (or the last one, when `failed`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// Family.
    pub family: String,
    /// Version.
    pub version: String,
    /// The step running: `fetch`, `import`, `map`, `state`.
    pub step: &'static str,
    /// The last lines of the previous step's output (at most [`TAIL_LINES`]).
    pub tail: Vec<String>,
}

/// Output lines kept per step.
pub const TAIL_LINES: usize = 6;

/// The last [`TAIL_LINES`] non-empty lines of child output.
pub fn tail(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let from = lines.len().saturating_sub(TAIL_LINES);
    let out: Vec<String> = lines[from..]
        .iter()
        .map(|l| l.chars().take(160).collect())
        .collect();
    assert!(out.len() <= TAIL_LINES);
    out
}

#[cfg(target_arch = "wasm32")]
mod guest_glue;

#[cfg(test)]
mod tests {
    use super::*;

    fn fv(f: &str, v: &str) -> (String, String) {
        (f.to_owned(), v.to_owned())
    }

    #[test]
    fn argv_is_exactly_the_spec() {
        let r = recipe("sky130").unwrap();
        assert_eq!(
            fetch_argv(r, "abc123").join(" "),
            "ciel enable --pdk-family sky130 --pdk-root .schemify/pdks/sky130/abc123/root -l sky130_fd_pr abc123"
        );
        assert_eq!(
            import_argv(r, "abc123", ".schemify/pdks/sky130/abc123/root").join(" "),
            "schemify-interop xschem-import .schemify/pdks/sky130/abc123/root/sky130A/libs.tech/xschem -o .schemify/pdks/sky130/abc123/lib --xschemrc .schemify/pdks/sky130/abc123/root/sky130A/libs.tech/xschem/xschemrc"
        );
        let ihp = recipe("ihp-sg13g2").unwrap();
        assert_eq!(
            fetch_argv(ihp, "v1").join(" "),
            "ciel enable --pdk-family ihp-sg13 --pdk-root .schemify/pdks/ihp-sg13g2/v1/root v1"
        );
        assert_eq!(
            ls_remote_argv(ihp).join(" "),
            "ciel ls-remote --pdk-family ihp-sg13"
        );
        assert!(recipe("asap7").is_some() && recipe("nope").is_none());
    }

    #[test]
    fn versions_and_paths_are_checked() {
        for ok in [
            "0fe599b2afb6708d281543108caf8310912f54af",
            "v1.2",
            "2024_01-x",
        ] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in [
            "",
            "latest",
            "..",
            "../x",
            "a/b",
            ".hidden",
            "-l",
            "a b",
            &"x".repeat(65),
        ] {
            assert!(!valid_version(bad), "{bad}");
        }
        let out = "Pre-built sky130 PDK versions\n├── 0FE599B2AFB6708D281543108CAF8310912F54AF (2023.10.05)\n└── 12df12e2e74145e31c5a13de02f9a1e176b56e67 (2023.09.20)\n0fe599b2afb6708d281543108caf8310912f54af again\nshort 1234abc\n";
        assert_eq!(
            parse_versions(out),
            [
                "0fe599b2afb6708d281543108caf8310912f54af",
                "12df12e2e74145e31c5a13de02f9a1e176b56e67"
            ]
        );
        assert_eq!(
            store_version("/home/u/p/.schemify/pdks/sky130/v1/lib/sky130_fd_pr"),
            Some(fv("sky130", "v1"))
        );
        assert_eq!(store_version(".schemify/pdks/nope/v1"), None);
        assert_eq!(store_version("lib/x"), None);
        let idx =
            parse_index("sky130 b\nsky130 a\nbogus x\nsky130 ../x\ngf180mcu c extra\nsky130 a\n");
        assert_eq!(idx, [fv("sky130", "b"), fv("sky130", "a")]);
        assert_eq!(write_index(&idx), "sky130 a\nsky130 b\n");
    }

    const MANIFEST: &str = "schemify 1 project\n\n# my own library\nlib mine path lib\nlib sky130_fd_pr path .schemify/pdks/sky130/old/lib/sky130_fd_pr\nlib tech map .schemify/pdks/sky130/old/tech.map\n\nplugin pdk-manager path plugins/pdk-manager\n  grant write_project\n\nset PDK_ROOT .schemify/pdks/sky130/old/root\nset SKYWATER_MODELS ${PDK_ROOT}/${PDK}/libs.tech/combined\nset MINE 1\n";
    const ROOT2: &str = ".schemify/pdks/gf180mcu/v2/root";
    const IMPORT: &str = "schemify 1 project\nlib gf180mcu_fd_pr path gf180mcu_fd_pr\n\nset GF_MODELS ${PDK_ROOT}/${PDK}/libs.tech/ngspice\n";

    #[test]
    fn use_rewrites_only_what_it_owns() {
        let r = recipe("gf180mcu").unwrap();
        let text = use_manifest(MANIFEST.as_bytes(), IMPORT.as_bytes(), r, "v2", ROOT2).unwrap();
        assert_eq!(
            text,
            "schemify 1 project\n\n# my own library\nlib mine path lib\n\nplugin pdk-manager path plugins/pdk-manager\n  grant write_project\n\nset MINE 1\n\nlib gf180mcu_fd_pr path .schemify/pdks/gf180mcu/v2/lib/gf180mcu_fd_pr\nlib tech map .schemify/pdks/gf180mcu/v2/tech.map\n\nset PDK_ROOT .schemify/pdks/gf180mcu/v2/root\nset PDK gf180mcuD\nset PDK_MODELS ${PDK_ROOT}/gf180mcuD/libs.tech/ngspice/sm141064.ngspice\nset GF_MODELS ${PDK_ROOT}/${PDK}/libs.tech/ngspice\n"
        );
        // The independent oracle: the format writer reproduces it (canonical, parses).
        let d = schemify_format::parse(text.as_bytes()).expect("parses");
        assert_eq!(schemify_format::write(&d), text);
        // Idempotent: using the same version again changes nothing.
        let again = use_manifest(text.as_bytes(), IMPORT.as_bytes(), r, "v2", ROOT2).unwrap();
        assert_eq!(again, text);
        let (active, _) = references(text.as_bytes(), None).unwrap();
        assert_eq!(active, [fv("gf180mcu", "v2")]);
        assert!(use_manifest(b"schemify 1 lock\n", IMPORT.as_bytes(), r, "v2", ROOT2).is_err());
        assert!(use_manifest(b"not a file", IMPORT.as_bytes(), r, "v2", ROOT2).is_err());
    }

    #[test]
    fn references_and_prune_plan() {
        let lock = "schemify 1 lock\nlocked lib sky130_fd_pr path /p/.schemify/pdks/sky130/locked/lib/sky130_fd_pr 00\nlocked lib mine path /p/lib 11\n";
        let (active, locked) = references(MANIFEST.as_bytes(), Some(lock.as_bytes())).unwrap();
        assert_eq!(active, [fv("sky130", "old")]);
        assert_eq!(locked, [fv("sky130", "locked")]);
        let installs = vec![
            fv("sky130", "old"),
            fv("sky130", "locked"),
            fv("sky130", "spare"),
            fv("gf180mcu", "broken"),
        ];
        assert_eq!(prune_plan(&installs, &active, &locked), [2, 3]);
        assert!(
            remove_refusal(&installs[0], &active, &locked)
                .unwrap()
                .contains("active")
        );
        assert!(
            remove_refusal(&installs[1], &active, &locked)
                .unwrap()
                .contains("schemify.lock")
        );
        assert_eq!(remove_refusal(&installs[2], &active, &locked), None);
    }

    #[test]
    fn tails_keep_the_last_lines() {
        let out: String = (0..20).map(|i| format!("line {i}\n\n")).collect();
        assert_eq!(
            tail(out.as_bytes()),
            [
                "line 14", "line 15", "line 16", "line 17", "line 18", "line 19"
            ]
        );
        assert!(tail(b"").is_empty());
    }

    #[test]
    fn shipped_maps_are_tech_maps() {
        for r in RECIPES {
            let d = schemify_format::parse(r.map.as_bytes()).expect(r.family);
            assert_eq!(d.kind(), schemify_format::FileKind::Techmap);
        }
    }

    #[test]
    fn recipes_are_coherent_rows() {
        for r in &RECIPES {
            // Imported symbols come with ciel's sectioned models; generated devices with files.
            let ciel = matches!(r.source, Source::Ciel { .. });
            assert_eq!(ciel, r.devices == Devices::Import, "{}", r.family);
            assert_eq!(
                ciel,
                matches!(r.models, Models::Sections { .. }),
                "{}",
                r.family
            );
            assert_eq!(ciel, pinned_version(r).is_none(), "{}", r.family);
            if let Source::Url { sha256, .. } = r.source {
                assert!(
                    sha256.len() == 64 && sha256.bytes().all(|b| b"0123456789abcdef".contains(&b))
                );
            }
            // Every generated cell a map names exists.
            if let Devices::Mos { cells, .. } | Devices::FinOsdi { cells, .. } = r.devices {
                for line in r.map.lines().filter_map(|l| l.trim().strip_prefix("cell ")) {
                    let (lib, cell) = line.split_once(':').unwrap();
                    assert_eq!(lib, r.family);
                    assert!(cells.iter().any(|d| d.cell == cell), "{line}");
                }
            }
        }
        assert_eq!(
            pinned_version(recipe("freepdk45").unwrap()),
            Some("5b93a0b1e12d")
        );
        assert_eq!(
            pinned_version(recipe("asap7").unwrap()),
            Some("58d72c9d291e186a77468586ab0c43d8a21eda6a")
        );
    }

    #[test]
    fn local_paths_never_reach_the_store() {
        let fp = recipe("freepdk45").unwrap();
        let asap7 = recipe("asap7").unwrap();
        for ok in [
            "/opt/FreePDK45",
            "kits/fp45",
            "C:\\kits\\FreePDK45",
            "../shared/kit",
        ] {
            assert_eq!(local_refusal(fp, ok), None, "{ok}");
        }
        for bad in [
            "",
            ".schemify/pdks/freepdk45/v/root",
            "/home/u/p/.schemify/x",
            "a\nb",
            &"x".repeat(LOCAL_PATH_MAX + 1),
        ] {
            assert!(local_refusal(fp, bad).is_some(), "{bad:?}");
        }
        // ASAP7's cards are read through fs_read: project-relative only.
        assert_eq!(local_refusal(asap7, "kits/asap7"), None);
        for bad in [
            "/opt/asap7",
            "../asap7",
            "kits//asap7",
            "C:\\asap7",
            "./kits",
        ] {
            assert!(local_refusal(asap7, bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn use_points_generated_kits_at_their_corner_library() {
        let r = recipe("freepdk45").unwrap();
        let lib = "schemify 1 project\n\nlib freepdk45 path freepdk45\n";
        let text = use_manifest(
            MANIFEST.as_bytes(),
            lib.as_bytes(),
            r,
            "ncsu",
            "/opt/FreePDK 45",
        )
        .unwrap();
        assert_eq!(
            text,
            "schemify 1 project\n\n# my own library\nlib mine path lib\n\nplugin pdk-manager path plugins/pdk-manager\n  grant write_project\n\nset MINE 1\n\nlib freepdk45 path .schemify/pdks/freepdk45/ncsu/lib/freepdk45\nlib tech map .schemify/pdks/freepdk45/ncsu/tech.map\n\nset PDK_ROOT \"/opt/FreePDK 45\"\nset PDK_MODELS .schemify/pdks/freepdk45/ncsu/models/corners.lib\n"
        );
        let d = schemify_format::parse(text.as_bytes()).expect("parses");
        assert_eq!(schemify_format::write(&d), text);
        // Switching back to a ciel family drops every record the generated kit added.
        let sky = recipe("sky130").unwrap();
        let back = use_manifest(
            text.as_bytes(),
            IMPORT.as_bytes(),
            sky,
            "v1",
            ".schemify/pdks/sky130/v1/root",
        )
        .unwrap();
        assert!(
            !back.contains("freepdk45") && !back.contains("FreePDK"),
            "{back}"
        );
    }

    #[test]
    fn remote_refresh_and_forget_rewrite_only_their_rows() {
        let m = "schemify 1 project\n\nlib mine path lib\nlib tech map maps/mine.map\nlib sky130_fd_pr path .schemify/pdks/sky130/v1/lib/sky130_fd_pr\n\nset PDK_ROOT .schemify/pdks/sky130/v1/root\nset MINE 1\n";
        let text = remote_manifest(m.as_bytes(), "tsmc28", "/proj/pdk/m.scs").unwrap();
        assert_eq!(
            text,
            "schemify 1 project\n\nlib mine path lib\nlib tech map maps/mine.map\n\nset MINE 1\n\nlib tsmc28 path .schemify/pdk-manager/remote/tsmc28\n\nset PDK_MODELS /proj/pdk/m.scs\n"
        );
        let d = schemify_format::parse(text.as_bytes()).expect("parses");
        assert_eq!(schemify_format::write(&d), text);
        // Idempotent: a second refresh rewrites the same rows.
        let again = remote_manifest(text.as_bytes(), "tsmc28", "/proj/pdk/m.scs").unwrap();
        assert_eq!(again, text);
        // Forget drops the family's lib and PDK_MODELS, nothing else; then has nothing to do.
        let gone = forget_manifest(text.as_bytes(), "tsmc28").unwrap().unwrap();
        assert_eq!(
            gone,
            "schemify 1 project\n\nlib mine path lib\nlib tech map maps/mine.map\n\nset MINE 1\n"
        );
        assert_eq!(forget_manifest(gone.as_bytes(), "tsmc28").unwrap(), None);
        assert_eq!(forget_manifest(text.as_bytes(), "other").unwrap(), None);
        assert!(remote_manifest(b"schemify 1 lock\n", "x", "/m").is_err());
        // Switching back to a local family drops the remote rows.
        let r = recipe("gf180mcu").unwrap();
        let back = use_manifest(text.as_bytes(), IMPORT.as_bytes(), r, "v2", ROOT2).unwrap();
        assert!(
            !back.contains("tsmc28") && !back.contains("/proj/pdk"),
            "{back}"
        );
    }

    #[test]
    fn model_exports_read_the_kit_itself_and_name_every_model_file() {
        let sky = recipe("sky130").unwrap();
        let a = models_export_argv(sky, "v", "/pdk", "spectre");
        let want = "schemify-interop pdk-models --root /pdk \
             --entry sky130A/libs.tech/combined/sky130.lib.spice --from spice --to spectre \
             -o .schemify/pdks/sky130/v/export/spectre";
        assert_eq!(
            a.join(" "),
            want.split_whitespace().collect::<Vec<_>>().join(" ")
        );
        // A kit with a file per corner: each file once.
        let f = model_entries(recipe("freepdk45").unwrap());
        assert_eq!(f.len(), 24);
        assert!(f.iter().all(|x| f.iter().filter(|y| *y == x).count() == 1));
    }

    #[test]
    fn exports_name_the_whole_library() {
        let a = export_argv(recipe("freepdk45").unwrap(), "ncsu", "xschem");
        assert_eq!(
            a,
            [
                "schemify-interop",
                "xschem-export",
                ".schemify/pdks/freepdk45/ncsu/lib",
                ".",
                "-o",
                "../export/xschem"
            ]
        );
        let v = export_argv(recipe("asap7").unwrap(), "v", "virtuoso");
        assert_eq!(v[1], "virtuoso-export");
        assert_eq!(v[v.len() - 2..], ["-o", "../export/asap7.sfyx"]);
        // An imported family exports the same way: Interop lists the directory.
        assert_eq!(
            export_argv(recipe("sky130").unwrap(), "v", "xschem")[3],
            "."
        );
    }
}
