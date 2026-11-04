//! An install as data (pdk spec §3): [`install_steps`] lists every I/O step of
//! `pdk.install F V [path]` for the recipe's source kind, and the guest runs them in order,
//! each starting when the previous one completed. The files the plugin generates are pure
//! functions here: device cells ([`device_shprim`], spec §2.2), the corner library
//! ([`corners_lib`], §7) and BSIM-CMG cards for ngspice's OSDI ([`derive_cmg`]); plus the
//! sha256 a `url` download is checked with ([`sha256_hex`]).

use crate::{Device, Devices, Models, Recipe, Source, fetch_argv, import_argv, version_dir};

/// Most steps one install has (the longest recipe has about 30).
pub const STEPS_MAX: usize = 64;
/// The corner library a [`Models::Files`] install writes, relative to the version directory.
pub const CORNERS_LIB: &str = "models/corners.lib";
/// The archive a `url` install downloads, relative to the kit root (removed once unpacked).
pub const ARCHIVE: &str = "source.tar.gz";
/// Most bytes of a downloaded archive the guest hashes (the `fs_read` reply limit).
pub const ARCHIVE_BYTES_MAX: usize = 64 << 20;

/// One I/O step of an install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `fs_remove` of a store path; nothing there is fine.
    Remove(String),
    /// Rewrite the install index with this install added.
    Index,
    /// `spawn` of the argv; a non-zero exit fails the install.
    Spawn(&'static str, Vec<String>),
    /// `fs_read` of a downloaded file, refused unless its sha256 is the given hex.
    Verify(String, &'static str),
    /// `fs_read` of a kit's level-72 card file, then `fs_write` of [`derive_cmg`] of it.
    Derive(String, String),
    /// `fs_write` of generated text: (panel label, path, bytes).
    Write(&'static str, String, String),
}

impl Step {
    /// The panel's name for the step.
    pub fn label(&self) -> &'static str {
        match self {
            Step::Remove(_) | Step::Index => "prepare",
            Step::Spawn(label, _) | Step::Write(label, _, _) => label,
            Step::Verify(..) => "verify (sha256)",
            Step::Derive(..) => "models (OSDI cards)",
        }
    }
}

/// The steps of `pdk.install <family> <version> [local]` (spec §3): remove `STATE`, add the
/// index row, fetch (or record the `local` kit's path in `ROOT`), import or generate the
/// devices, write the corner library and `tech.map`, write `STATE` = `ok` last.
///
/// `version` must be the recipe's pinned version for `git` and `url` sources unless `local`
/// is given ([`crate::pinned_version`]); `local` must pass [`crate::local_refusal`].
pub fn install_steps(r: &Recipe, version: &str, local: Option<&str>) -> Vec<Step> {
    assert!(local.is_none_or(|p| crate::local_refusal(r, p).is_none()));
    assert!(local.is_some() || crate::pinned_version(r).is_none_or(|v| v == version));
    let dir = version_dir(r.family, version);
    let root = local.map_or_else(|| format!("{dir}/root"), str::to_owned);
    let mut s = vec![Step::Remove(format!("{dir}/STATE")), Step::Index];
    match (local, r.source) {
        (Some(path), _) => {
            s.push(Step::Remove(format!("{dir}/root")));
            s.push(Step::Write(
                "prepare",
                format!("{dir}/ROOT"),
                format!("{path}\n"),
            ));
        }
        (None, Source::Ciel { .. }) => {
            s.push(Step::Remove(format!("{dir}/ROOT")));
            s.push(Step::Spawn("fetch (ciel)", fetch_argv(r, version)));
        }
        (None, Source::Git { url, rev }) => {
            s.push(Step::Remove(format!("{dir}/ROOT")));
            s.push(Step::Remove(root.clone()));
            s.extend(git_steps(url, rev, &root));
        }
        (None, Source::Url { url, sha256 }) => {
            let archive = format!("{root}/{ARCHIVE}");
            s.push(Step::Remove(format!("{dir}/ROOT")));
            s.push(Step::Remove(root.clone()));
            s.push(Step::Spawn(
                "download (curl)",
                argv(&["curl", "-fsSL", "--create-dirs", "-o", &archive, url]),
            ));
            s.push(Step::Verify(archive.clone(), sha256));
            s.push(Step::Spawn(
                "unpack (tar)",
                argv(&["tar", "-xzf", &archive, "-C", &root, "--strip-components=1"]),
            ));
            s.push(Step::Remove(archive));
        }
    }
    match r.devices {
        Devices::Import => s.push(Step::Spawn(
            "import (schemify-interop)",
            import_argv(r, version, &root),
        )),
        Devices::Mos { cells, .. } | Devices::FinOsdi { cells, .. } => {
            s.push(Step::Remove(format!("{dir}/lib")));
            s.push(Step::Remove(format!("{dir}/models")));
            if let Devices::FinOsdi { va, .. } = r.devices {
                let to = format!("{dir}/{}", va.dir);
                s.push(Step::Remove(to.clone()));
                s.extend(git_steps(va.url, va.rev, &to));
            }
            if let (Models::Files(corners), Devices::FinOsdi { .. }) = (r.models, r.devices) {
                for file in corners.iter().flat_map(|c| c.1.iter()) {
                    let to = format!("{dir}/models/{}", basename(file));
                    s.push(Step::Derive(format!("{root}/{file}"), to));
                }
            }
            let up = models_to_root(local, &root);
            s.push(Step::Write(
                "models",
                format!("{dir}/{CORNERS_LIB}"),
                corners_lib(r, &up),
            ));
            for d in cells {
                let path = format!("{dir}/lib/{}/{}.shprim", r.family, d.cell);
                s.push(Step::Write("devices", path, device_shprim(r, d)));
            }
            let manifest = format!("schemify 1 project\n\nlib {0} path {0}\n", r.family);
            s.push(Step::Write(
                "devices",
                format!("{dir}/lib/schemify.project"),
                manifest,
            ));
        }
    }
    s.push(Step::Write(
        "tech.map",
        format!("{dir}/tech.map"),
        r.map.to_owned(),
    ));
    s.push(Step::Write(
        "state",
        format!("{dir}/STATE"),
        "ok\n".to_owned(),
    ));
    assert!(s.len() <= STEPS_MAX);
    s
}

pub(crate) fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// `git clone --quiet --no-checkout <url> <to>`, then `git -C <to> checkout --quiet <rev>`.
fn git_steps(url: &str, rev: &str, to: &str) -> [Step; 2] {
    assert!(crate::valid_version(rev));
    [
        Step::Spawn(
            "fetch (git)",
            argv(&["git", "clone", "--quiet", "--no-checkout", url, to]),
        ),
        Step::Spawn(
            "checkout (git)",
            argv(&["git", "-C", to, "checkout", "--quiet", rev]),
        ),
    ]
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The kit root as seen from `<dir>/models/` (where [`CORNERS_LIB`] lives): ngspice resolves
/// a relative `.include` against the including file's directory. An absolute `local` path
/// stays absolute; a project-relative one climbs the five store levels.
fn models_to_root(local: Option<&str>, root: &str) -> String {
    let is_abs = |p: &str| {
        let b = p.as_bytes();
        p.starts_with(['/', '\\', '~'])
            || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
    };
    let up = match local {
        None => "../root".to_owned(),
        Some(p) if is_abs(p) => p.trim_end_matches('/').to_owned(),
        Some(p) => format!("../../../../../{}", p.trim_end_matches('/')),
    };
    assert!(!up.is_empty() && !root.is_empty());
    up
}

/// The generated corner library (spec §7): one `.lib <corner>` … `.endl <corner>` section per
/// row of [`Models::Files`], each including that corner's files: under `up` (the kit root seen
/// from `models/`), or for [`Devices::FinOsdi`] the derived cards beside the library. Paths
/// are quoted, so a `local` kit path may hold spaces.
///
/// # Panics
///
/// If the recipe's models are not [`Models::Files`].
pub fn corners_lib(r: &Recipe, up: &str) -> String {
    let Models::Files(corners) = r.models else {
        panic!("{}: models are sections, not files", r.family)
    };
    let derived = matches!(r.devices, Devices::FinOsdi { .. });
    let mut t = format!(
        "* {}: model corners as .lib sections (pdk-manager, pdk spec §7)\n",
        r.family
    );
    for (corner, files) in corners {
        t.push_str(&format!("\n.lib {corner}\n"));
        for f in *files {
            let path = if derived {
                basename(f).to_owned()
            } else {
                format!("{up}/{f}")
            };
            t.push_str(&format!(".include \"{path}\"\n"));
        }
        t.push_str(&format!(".endl {corner}\n"));
    }
    assert_eq!(t.matches(".endl ").count(), corners.len());
    t
}

const NMOS4: &str = include_str!("../devices/nmos4.shprim");
const PMOS4: &str = include_str!("../devices/pmos4.shprim");
/// The size label of the core MOS drawing, replaced for cells without `w`/`l`.
const WL_LABEL: &str = "{eng(w)}/{eng(l)}";

/// The `.shprim` of a generated device cell (spec §2.2): the pins and drawing of `core:nmos4`
/// or `core:pmos4` exactly (so a `tech:` instance and a direct one look and connect alike),
/// the kit's model name and size params, and its instance line: `M{name} … {model} w={w}
/// l={l} nf={nf} m={m}` for [`Devices::Mos`]; for [`Devices::FinOsdi`] `N{name} … {model} nfin={nfin}
/// nf={nf} m={m}` in a `veriloga` impl whose `file` is the checked-out BSIM-CMG source, which the
/// runner compiles to `<stem>.osdi` (run spec §5), plus a `format espice` line for ESPice, which
/// compiles that source itself and needs every port connected (the 5th, thermal `t`, grounded)
/// and `mfactor=` for the multiplier (netlist spec §7.2a).
///
/// # Panics
///
/// If the recipe imports its devices ([`Devices::Import`]).
pub fn device_shprim(r: &Recipe, d: &Device) -> String {
    let core = if d.pmos { PMOS4 } else { NMOS4 };
    let pins: String = core
        .lines()
        .filter(|l| l.starts_with("pin "))
        .map(|l| format!("{l}\n"))
        .collect();
    // The drawing: everything after the impl block.
    let impl_at = core.find("\nimpl ").expect("core MOS has an impl");
    let draw_at = impl_at + core[impl_at..].find("\n\n").expect("a drawing follows") + 2;
    let drawing = &core[draw_at..];
    let channel = if d.pmos { "P" } else { "N" };
    let mut t = format!(
        "schemify 1 shprim\n  doc \"{} {} ({channel}-channel), generated by pdk-manager (pdk spec §2.2)\"\n  prefix M\n  category semiconductor\n\n{pins}\nparam model {}\n  kind string\n  doc \"the kit's .model name\"\n",
        r.family, d.model, d.model
    );
    match r.devices {
        Devices::Mos { w, l, .. } => {
            t.push_str(&format!(
                "param w {w}\n  unit m\n  doc \"total channel width\"\nparam l {l}\n  unit m\n  doc \"channel length\"\nparam nf 1\n  doc \"number of fingers\"\nparam m 1\n  doc \"parallel multiplier\"\n\nimpl spice spice\n  format spice \"M{{name}} {{d}} {{g}} {{s}} {{b}} {{model}} w={{w}} l={{l}} nf={{nf}} m={{m}}\"\n  format spectre \"{{name}} ({{d}} {{g}} {{s}} {{b}}) {{model}} w={{w}} l={{l}} nf={{nf}} m={{m}}\"\n\n"
            ));
            t.push_str(drawing);
        }
        Devices::FinOsdi { va, .. } => {
            t.push_str(&format!(
                "param nfin 1\n  doc \"fins per finger\"\nparam nf 1\n  doc \"number of fingers\"\nparam m 1\n  doc \"parallel multiplier\"\n\nimpl osdi veriloga\n  format spice \"N{{name}} {{d}} {{g}} {{s}} {{b}} {{model}} nfin={{nfin}} nf={{nf}} m={{m}}\"\n  format espice \"N{{name}} {{d}} {{g}} {{s}} {{b}} 0 {{model}} nfin={{nfin}} nf={{nf}} mfactor={{m}}\"\n  file ../../{}/{}\n\n",
                va.dir, va.source
            ));
            t.push_str(&drawing.replace(WL_LABEL, "nfin={nfin}"));
        }
        Devices::Import => panic!("{} imports its devices", r.family),
    }
    assert!(t.ends_with('\n') && t.matches("\npin ").count() == 4);
    t
}

/// A kit's BSIM-CMG cards made loadable by ngspice's OSDI (spec §2.2): ngspice has no built-in
/// level 72, so each `.model <name> nmos|pmos level = 72` line becomes `.model <name>
/// bsimcmg_va type=1|-1` (the module the compiled Verilog-A defines; `type` 1 = n, -1 = p).
/// Every other line is kept byte for byte, the kit's licence header included. Parameters the
/// newer model does not know (`version`, `coremod`, …) are ignored by ngspice with a warning.
///
/// # Errors
///
/// No level-72 `.model` line, or a MOS `.model` line of another level, as text.
pub fn derive_cmg(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len() + 128);
    out.push_str(
        "* derived by pdk-manager for ngspice OSDI (bsimcmg_va); the kit's card follows\n",
    );
    let mut models = 0usize;
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let mos = w.len() >= 3
            && w[0].eq_ignore_ascii_case(".model")
            && matches!(w[2].to_ascii_lowercase().as_str(), "nmos" | "pmos");
        if !mos {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if !w[3..].concat().eq_ignore_ascii_case("level=72") {
            return Err(format!("not a level-72 card: `{}`", line.trim()));
        }
        let ty = if w[2].eq_ignore_ascii_case("nmos") {
            "1"
        } else {
            "-1"
        };
        out.push_str(&format!(".model {} bsimcmg_va type={ty}\n", w[1]));
        models += 1;
    }
    if models == 0 {
        return Err("no level-72 .model card".to_owned());
    }
    assert!(out.lines().count() == text.lines().count() + 1);
    Ok(out)
}

/// Bytes the guest hashes per event, so a download is hashed over several events within the
/// default fuel of 10^8 per event (plugin-abi §2). Measured on the FreePDK45 archive: 256 KiB
/// in one event exhausts it, 64 KiB does not; 32 KiB keeps a 2x margin (a 64 MiB archive is
/// 2048 events, one per frame).
pub const HASH_CHUNK: usize = 32 << 10;

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// An incremental sha256 (FIPS 180-4): whole 64-byte blocks in, then the tail. The guest
/// checks a `url` download with it, [`HASH_CHUNK`] bytes per event, before unpacking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sha256 {
    h: [u32; 8],
    len: u64,
}

impl Default for Sha256 {
    fn default() -> Sha256 {
        let h = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        Sha256 { h, len: 0 }
    }
}

impl Sha256 {
    /// Hashes `data`, whole blocks only. O(len).
    ///
    /// # Panics
    ///
    /// If `data.len()` is not a multiple of 64.
    pub fn blocks(&mut self, data: &[u8]) {
        assert!(data.len().is_multiple_of(64), "whole blocks only");
        for block in data.as_chunks::<64>().0 {
            let mut w = [0u32; 64];
            for (i, c) in block.as_chunks::<4>().0.iter().enumerate() {
                w[i] = u32::from_be_bytes(*c);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let mut v = self.h;
            for i in 0..64 {
                let s1 = v[4].rotate_right(6) ^ v[4].rotate_right(11) ^ v[4].rotate_right(25);
                let ch = (v[4] & v[5]) ^ (!v[4] & v[6]);
                let t1 = v[7]
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = v[0].rotate_right(2) ^ v[0].rotate_right(13) ^ v[0].rotate_right(22);
                let maj = (v[0] & v[1]) ^ (v[0] & v[2]) ^ (v[1] & v[2]);
                v = [
                    t1.wrapping_add(s0).wrapping_add(maj),
                    v[0],
                    v[1],
                    v[2],
                    v[3].wrapping_add(t1),
                    v[4],
                    v[5],
                    v[6],
                ];
            }
            for (a, b) in self.h.iter_mut().zip(v) {
                *a = a.wrapping_add(b);
            }
        }
        self.len += data.len() as u64;
    }

    /// Hashes the last `rest` bytes (fewer than 64) and returns the lowercase hex digest.
    ///
    /// # Panics
    ///
    /// If `rest` is 64 bytes or longer.
    pub fn finish(mut self, rest: &[u8]) -> String {
        assert!(rest.len() < 64);
        let bits = (self.len + rest.len() as u64) * 8;
        let mut tail = rest.to_vec();
        tail.push(0x80);
        while tail.len() % 64 != 56 {
            tail.push(0);
        }
        tail.extend_from_slice(&bits.to_be_bytes());
        self.blocks(&tail);
        let hex: String = self.h.iter().map(|x| format!("{x:08x}")).collect();
        assert_eq!(hex.len(), 64);
        hex
    }
}

/// Lowercase hex sha256 of `bytes` in one call. O(n).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let whole = bytes.len() / 64 * 64;
    let mut s = Sha256::default();
    s.blocks(&bytes[..whole]);
    s.finish(&bytes[whole..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe;

    /// One line per step: what it does, without generated bodies.
    fn render(steps: &[Step]) -> String {
        let mut t = String::new();
        for s in steps {
            let line = match s {
                Step::Remove(p) => format!("remove {p}"),
                Step::Index => "index".to_owned(),
                Step::Spawn(l, a) => format!("{l}: {}", a.join(" ")),
                Step::Verify(p, h) => format!("verify {p} {}", &h[..8]),
                Step::Derive(f, to) => format!("derive {f} -> {to}"),
                Step::Write(l, p, _) => format!("{l}: write {p}"),
            };
            t.push_str(&line);
            t.push('\n');
        }
        t
    }

    const FP45: &str = "5b93a0b1e12d";
    const ASAP7: &str = "58d72c9d291e186a77468586ab0c43d8a21eda6a";

    #[test]
    fn url_install_downloads_verifies_unpacks_then_generates() {
        let r = recipe("freepdk45").unwrap();
        let d = format!(".schemify/pdks/freepdk45/{FP45}");
        let want = format!(
            "remove {d}/STATE\nindex\nremove {d}/ROOT\nremove {d}/root\n\
             download (curl): curl -fsSL --create-dirs -o {d}/root/source.tar.gz https://codeload.github.com/baichen318/FreePDK45/tar.gz/a75cb116a3efd1d3ae05b695285452e6ffbc7ca5\n\
             verify {d}/root/source.tar.gz 5b93a0b1\n\
             unpack (tar): tar -xzf {d}/root/source.tar.gz -C {d}/root --strip-components=1\n\
             remove {d}/root/source.tar.gz\nremove {d}/lib\nremove {d}/models\n\
             models: write {d}/models/corners.lib\n"
        );
        let got = render(&install_steps(r, FP45, None));
        assert!(got.starts_with(&want), "{got}");
        let cells: Vec<&str> = got
            .lines()
            .filter(|l| l.starts_with("devices: write"))
            .collect();
        assert_eq!(cells.len(), 9, "8 cells and the library manifest");
        assert!(got.ends_with(&format!(
            "devices: write {d}/lib/schemify.project
tech.map: write {d}/tech.map
state: write {d}/STATE
"
        )));
    }

    #[test]
    fn git_install_checks_out_both_pinned_revs_and_derives_the_cards() {
        let r = recipe("asap7").unwrap();
        let d = format!(".schemify/pdks/asap7/{ASAP7}");
        let got = render(&install_steps(r, ASAP7, None));
        let want = format!(
            "remove {d}/STATE\nindex\nremove {d}/ROOT\nremove {d}/root\n\
             fetch (git): git clone --quiet --no-checkout https://github.com/The-OpenROAD-Project/asap7_pdk_r1p7 {d}/root\n\
             checkout (git): git -C {d}/root checkout --quiet {ASAP7}\n\
             remove {d}/lib\nremove {d}/models\nremove {d}/va\n\
             fetch (git): git clone --quiet --no-checkout https://github.com/dwarning/VA-Models {d}/va\n\
             checkout (git): git -C {d}/va checkout --quiet 6099b4e12e6f66ff530f59b8d01fb909094be85a\n\
             derive {d}/root/models/hspice/7nm_TT_160803.pm -> {d}/models/7nm_TT_160803.pm\n\
             derive {d}/root/models/hspice/7nm_FF_160803.pm -> {d}/models/7nm_FF_160803.pm\n\
             derive {d}/root/models/hspice/7nm_SS_160803.pm -> {d}/models/7nm_SS_160803.pm\n\
             models: write {d}/models/corners.lib\n"
        );
        assert!(got.starts_with(&want), "{got}");
        assert!(got.ends_with(&format!("state: write {d}/STATE\n")));
    }

    #[test]
    fn local_install_fetches_nothing_and_records_the_path() {
        let r = recipe("freepdk45").unwrap();
        let d = ".schemify/pdks/freepdk45/ncsu";
        let steps = install_steps(r, "ncsu", Some("/opt/FreePDK 45"));
        let got = render(&steps);
        assert!(got.starts_with(&format!(
            "remove {d}/STATE\nindex\nremove {d}/root\nprepare: write {d}/ROOT\nremove {d}/lib\n"
        )));
        // Nothing is downloaded, unpacked or removed outside the version directory.
        assert!(!got.contains("curl") && !got.contains("tar ") && !got.contains("git "));
        for s in &steps {
            if let Step::Remove(p) = s {
                assert!(p.starts_with(&format!("{d}/")), "{p}");
            }
        }
        assert!(steps.contains(&Step::Write(
            "prepare",
            format!("{d}/ROOT"),
            "/opt/FreePDK 45\n".into()
        )));
        let lib = steps.iter().find_map(|s| match s {
            Step::Write(_, p, b) if p.ends_with("corners.lib") => Some(b.clone()),
            _ => None,
        });
        assert!(lib.unwrap().contains(".include \"/opt/FreePDK 45/FreePDK45/ncsu_basekit/models/hspice/tran_models/models_ss/PMOS_THKOX.inc\"\n"));
        // ASAP7 from a project-relative kit: its cards are read where they are.
        let a = render(&install_steps(
            recipe("asap7").unwrap(),
            "mine",
            Some("kits/asap7"),
        ));
        assert!(a.contains("derive kits/asap7/models/hspice/7nm_TT_160803.pm -> .schemify/pdks/asap7/mine/models/7nm_TT_160803.pm\n"));
        assert!(!a.contains("asap7_pdk_r1p7"), "the kit is not cloned");
        // A ciel family from disk: imported in place.
        let c = render(&install_steps(
            recipe("sky130").unwrap(),
            "mine",
            Some("/pdk"),
        ));
        assert!(c.contains("import (schemify-interop): schemify-interop xschem-import /pdk/sky130A/libs.tech/xschem -o .schemify/pdks/sky130/mine/lib"));
        assert!(!c.contains("ciel"));
    }

    #[test]
    fn ciel_install_is_the_mvp_chain() {
        let r = recipe("sky130").unwrap();
        let d = ".schemify/pdks/sky130/v1";
        assert_eq!(
            render(&install_steps(r, "v1", None)),
            format!(
                "remove {d}/STATE\nindex\nremove {d}/ROOT\n\
                 fetch (ciel): ciel enable --pdk-family sky130 --pdk-root {d}/root -l sky130_fd_pr v1\n\
                 import (schemify-interop): schemify-interop xschem-import {d}/root/sky130A/libs.tech/xschem -o {d}/lib --xschemrc {d}/root/sky130A/libs.tech/xschem/xschemrc\n\
                 tech.map: write {d}/tech.map\nstate: write {d}/STATE\n"
            )
        );
    }

    #[test]
    fn corner_library_sections_by_root() {
        let r = recipe("freepdk45").unwrap();
        let t = corners_lib(r, "../../../../../kits/fp45");
        assert!(t.starts_with("* freepdk45: model corners as .lib sections (pdk-manager, pdk spec §7)\n\n.lib tt\n.include \"../../../../../kits/fp45/FreePDK45/ncsu_basekit/models/hspice/tran_models/models_nom/NMOS_VTL.inc\"\n"));
        assert_eq!(t.matches(".include ").count(), 24);
        assert!(t.ends_with("models_ss/PMOS_THKOX.inc\"\n.endl ss\n"));
        assert_eq!(models_to_root(None, "x"), "../root");
        assert_eq!(
            models_to_root(Some("kits/fp45/"), "x"),
            "../../../../../kits/fp45"
        );
        assert_eq!(models_to_root(Some("C:\\kits"), "x"), "C:\\kits");
        assert_eq!(
            corners_lib(recipe("asap7").unwrap(), "unused"),
            "* asap7: model corners as .lib sections (pdk-manager, pdk spec §7)\n\n.lib tt\n.include \"7nm_TT_160803.pm\"\n.endl tt\n\n.lib ff\n.include \"7nm_FF_160803.pm\"\n.endl ff\n\n.lib ss\n.include \"7nm_SS_160803.pm\"\n.endl ss\n"
        );
    }

    /// The generated cells of the `tech:` inverter equal the CLI's netlist fixtures
    /// (Schemify's `crates/cli/tests/fixtures/tech/pdk`, copied to `tests/fixtures/schemify/`):
    /// a re-bless here must be copied back there, or the golden netlists drift.
    #[test]
    fn generated_cells_are_canonical_core_style_symbols() {
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/schemify/tech/pdk"
        );
        for (family, cells) in [
            ("freepdk45", ["nmos_vtl", "pmos_vtl"]),
            ("asap7", ["nmos_rvt", "pmos_rvt"]),
        ] {
            let r = recipe(family).unwrap();
            let (Devices::Mos { cells: all, .. } | Devices::FinOsdi { cells: all, .. }) = r.devices
            else {
                panic!("{family} generates its devices")
            };
            for d in all {
                let t = device_shprim(r, d);
                // Canonical: the format writer reproduces it byte for byte.
                let doc = schemify_format::parse(t.as_bytes()).expect(d.cell);
                assert_eq!(schemify_format::write(&doc), t, "{family}:{}", d.cell);
                // The core symbol's pins and drawing, exactly.
                let core = if d.pmos { PMOS4 } else { NMOS4 };
                let pins = |x: &str| {
                    x.lines()
                        .filter(|l| l.starts_with("pin "))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                assert_eq!(pins(&t), pins(core));
                let draw =
                    |x: &str| x[x.find("\nline ").unwrap()..].replace("nfin={nfin}", WL_LABEL);
                assert_eq!(draw(&t), draw(core));
                assert!(t.contains(&format!("param model {}\n", d.model)));
                if cells.contains(&d.cell) {
                    let path = format!("{fixtures}/{family}/{family}/{}.shprim", d.cell);
                    if std::env::var_os("SCHEMIFY_BLESS").is_some() {
                        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap())
                            .unwrap();
                        std::fs::write(&path, &t).unwrap();
                    }
                    assert_eq!(
                        std::fs::read_to_string(&path).expect("blessed"),
                        t,
                        "{path}"
                    );
                }
            }
        }
        let asap7 = device_shprim(
            recipe("asap7").unwrap(),
            &crate::Device {
                cell: "x",
                model: "nmos_rvt",
                pmos: false,
            },
        );
        assert!(asap7.contains("impl osdi veriloga\n  format spice \"N{name} {d} {g} {s} {b} {model} nfin={nfin} nf={nf} m={m}\"\n  format espice \"N{name} {d} {g} {s} {b} 0 {model} nfin={nfin} nf={nf} mfactor={m}\"\n  file ../../va/code/bsimcmg/vacode/bsimcmg.va\n"));
        assert!(!asap7.contains("param w ") && !asap7.contains("param l "));
        // FinFET OSDI cells have no Spectre form (Verilog-A in Spectre is `ahdl_include`).
        assert!(!asap7.contains("format spectre"));
        let fp45 = device_shprim(
            recipe("freepdk45").unwrap(),
            &crate::Device {
                cell: "x",
                model: "NMOS_VTL",
                pmos: false,
            },
        );
        assert!(fp45.contains(
            "\n  format spectre \"{name} ({d} {g} {s} {b}) {model} w={w} l={l} nf={nf} m={m}\"\n"
        ));
    }

    #[test]
    fn level_72_cards_become_osdi_models() {
        let card = "* BSD 3-Clause License\n.model nmos_rvt nmos level = 72\n+version = 107 l = 2.1e-008\n.MODEL pmos_rvt PMOS level=72\n+tfin = 6.5e-009\n";
        assert_eq!(
            derive_cmg(card).unwrap(),
            "* derived by pdk-manager for ngspice OSDI (bsimcmg_va); the kit's card follows\n* BSD 3-Clause License\n.model nmos_rvt bsimcmg_va type=1\n+version = 107 l = 2.1e-008\n.model pmos_rvt bsimcmg_va type=-1\n+tfin = 6.5e-009\n"
        );
        // Negative space: a BSIM4 card, or no card at all, is refused.
        assert!(
            derive_cmg(".model NMOS_VTL nmos level = 54\n")
                .unwrap_err()
                .contains("not a level-72")
        );
        assert!(derive_cmg("* nothing\n.model r1 r r=1\n").is_err());
    }

    #[test]
    fn sha256_matches_the_fips_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let two = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            sha256_hex(two),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let million = vec![b'a'; 1_000_000];
        let want = "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0";
        assert_eq!(sha256_hex(&million), want);
        // In chunks, as the guest hashes a download.
        let mut s = Sha256::default();
        let whole = million.len() / 64 * 64;
        for c in million[..whole].chunks(HASH_CHUNK) {
            s.blocks(c);
        }
        assert_eq!(s.finish(&million[whole..]), want);
    }
}
