//! The built guest (`pdk_manager.wasm`, from `build.sh`) through the
//! real host (`schemify_plugin`: grants and path checks included), with fake `ciel`,
//! `schemify-interop`, `git`, `curl`, `tar` and `ssh` first on `PATH` (shell scripts that write a
//! tiny tree and log their argv; the fake `ssh` runs the remote command locally, against a
//! fixture kit outside the project).
//! Filesystem and canonical-text oracles: the argv of every step, the store layout, `STATE`,
//! `tech.map`, the manifest after `pdk.use`, that a failing step leaves no `STATE ok`, that
//! `pdk.remove` refuses the active and the locked version, and that `pdk.prune` removes exactly
//! the unreferenced ones; that a remote kit needs trust, that its scan (agent and fallback)
//! writes only the cells and the manifest rows and that no kit byte reaches the project tree
//! or the panel, that forget removes them, and that exports refuse it. Skips when the module
//! has not been built. `real_interop_scans_a_remote_kit` runs the real `schemify-interop` (the
//! dev shell's) behind the fake `ssh`, and skips when it does not speak `pdk-scan`.
//!
//! Ignored, nightly-style, against the network: `real_ciel_installs_sky130` (real `ciel` and
//! `schemify-interop` on `PATH`) and `real_kits_simulate_an_inverter` (FreePDK45 and ASAP7 installed
//! for real, then the inverter's DC sweep in ngspice): `cargo test -- --ignored`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use schemify_plugin::abi::{Code, Event, FUEL_DEFAULT, Kind};
use schemify_plugin::{IoOp, PluginEvents, PluginId, Plugins, RequestQueues, Snapshots, deliver};

const GRANTS: [&str; 7] = [
    "read_project",
    "write_project",
    "spawn:schemify-interop",
    "spawn:git",
    "spawn:curl",
    "spawn:tar",
    "spawn:ssh",
];

/// The fake `ciel`: logs its argv; `ls-remote` prints two versions; `enable` builds
/// `<root>/<variant>/libs.tech/xschem/xschemrc`, or fails for `$FAIL_CIEL`.
const FAKE_CIEL: &str = r#"#!/bin/sh
echo "ciel $*" >> "$FAKE_LOG"
case "$1" in
ls-remote)
  echo "Pre-built $3 PDK versions"
  echo "├── 1111111111111111111111111111111111111111 (2024.01.01)"
  echo "└── 2222222222222222222222222222222222222222 (2023.12.01)"
  exit 0 ;;
enable)
  root="$5"; eval "ver=\${$#}"
  if [ "$ver" = "$FAIL_CIEL" ]; then echo "downloading $ver"; echo "error: network unreachable" >&2; exit 3; fi
  case "$3" in sky130) v=sky130A ;; gf180mcu) v=gf180mcuD ;; *) v="$3" ;; esac
  mkdir -p "$root/$v/libs.tech/xschem" && echo "set PDK_ROOT x" > "$root/$v/libs.tech/xschem/xschemrc"
  echo "Version $ver enabled for the $3 PDK."
  exit 0 ;;
esac
exit 64
"#;

/// The fake `schemify-interop`: `ciel …` hands over to the fake ciel beside it (which logs);
/// `xschem-import` logs its argv and writes `<out>/schemify.project` and one cell, or fails
/// when the output directory names `$FAIL_IMPORT`.
const FAKE_INTEROP: &str = r#"#!/bin/sh
if [ "$1" = ciel ]; then shift; exec "$(dirname "$0")/ciel" "$@"; fi
echo "schemify-interop $*" >> "$FAKE_LOG"
case "$1" in
xschem-export|virtuoso-export) echo "exported"; exit 0 ;;
pdk-scan) exec sh "$(dirname "$0")/pdk-scan" "$@" ;;
esac
out="$4"
case "$out" in *"/$FAIL_IMPORT/"*) echo "import: no symbols found" >&2; exit 2 ;; esac
mkdir -p "$out/sky130_fd_pr"
printf 'schemify 1 project\n\nlib sky130_fd_pr path sky130_fd_pr\n\nset SKYWATER_MODELS ${PDK_ROOT}/${PDK}/libs.tech/combined\n' > "$out/schemify.project"
printf 'schemify 1 shprim\n' > "$out/sky130_fd_pr/nfet_01v8.shprim"
echo "imported 1 cell"
"#;

/// The fake `pdk-scan` (protocol 1): the agent scan prints two cells (or the scan's error when
/// the entry is missing); `--remote-script` prints a framing script; `--stdin` needs the
/// entry's `inc.spice` until its frames hold it, then prints the cells. Never echoes its input.
const FAKE_PDK_SCAN: &str = r#"#!/bin/sh
cells() { printf 'pdkscan 1\nversion 0123456789abcdef\nfile nch.shprim 18\nschemify 1 shprim\nfile pch.shprim 18\nschemify 1 shprim\nissue bins skipped\nend\n'; }
shift
case "$1" in
--protocol) echo "pdkscan 1"; exit 0 ;;
--remote-script) cat <<'EOS'
printf 'sfyframe 1\n'
for p in "$@"; do
  if [ -f "$p" ]; then printf 'f %s %s\n' "$(wc -c < "$p" | tr -d ' ')" "$p"; cat "$p"
  else printf 'f 0 ! %s\n' "$p"; fi
done
printf 'end\n'
EOS
  exit 0 ;;
--root) [ -f "$2/$4" ] || { printf 'pdkscan 1\nerror "cannot read %s"\nend\n' "$4"; exit 1; }
  cells; exit 0 ;;
--stdin) entry="$5"; inc="$(dirname "$entry")/inc.spice"; input="$(cat)"
  case "$input" in
  *"$inc"*) cells ;;
  *) printf 'pdkscan 1\nversion 0123456789abcdef\nneed %s\nend\n' "$inc" ;;
  esac
  exit 0 ;;
esac
exit 64
"#;

/// The fake `ssh`: logs its argv and runs the remote command (the words after the
/// destination) locally with `sh -c`, as sshd's login shell would; host `down` refuses the
/// connection, and with `$NO_AGENT` set the remote side has no `schemify-interop`.
const FAKE_SSH: &str = r#"#!/bin/sh
echo "ssh $*" >> "$FAKE_LOG"
while [ "$1" != "--" ]; do shift; done
shift; host="$1"; shift
case "$host" in *down) echo "ssh: connect to host down port 22: Connection refused" >&2; exit 255 ;; esac
case "$1" in schemify-interop) if [ -n "$NO_AGENT" ]; then echo "sh: schemify-interop: not found" >&2; exit 127; fi ;; esac
exec sh -c "$*"
"#;

/// The fake `git`: logs its argv; `clone` of the ASAP7 kit writes three level-72 corner cards,
/// of VA-Models the BSIM-CMG source; `checkout` of `$FAIL_GIT` fails.
const FAKE_GIT: &str = r#"#!/bin/sh
echo "git $*" >> "$FAKE_LOG"
case "$1" in
clone)
  url="$4"; to="$5"; mkdir -p "$to"
  case "$url" in
  *asap7_pdk_r1p7) mkdir -p "$to/models/hspice"
    for c in TT FF SS; do printf '* BSD 3-Clause\n.model nmos_rvt nmos level = 72\n+l = 2.1e-008\n.model pmos_rvt pmos level = 72\n+l = 2.1e-008\n' > "$to/models/hspice/7nm_${c}_160803.pm"; done ;;
  *VA-Models) mkdir -p "$to/code/bsimcmg/vacode"; echo "module bsimcmg_va(d, g, s, e); endmodule" > "$to/code/bsimcmg/vacode/bsimcmg.va" ;;
  esac
  exit 0 ;;
-C)
  if [ "$5" = "$FAIL_GIT" ]; then echo "fatal: reference is not a tree: $5" >&2; exit 128; fi
  exit 0 ;;
esac
exit 64
"#;

/// The fake `curl`: logs its argv and downloads something that is not the kit.
const FAKE_CURL: &str = r#"#!/bin/sh
echo "curl $*" >> "$FAKE_LOG"
mkdir -p "$(dirname "$4")" && printf 'not the kit' > "$4"
"#;

/// The fake `tar`: logs its argv (the download it would unpack never verifies).
const FAKE_TAR: &str = r#"#!/bin/sh
echo "tar $*" >> "$FAKE_LOG"
"#;

struct World {
    plugins: Plugins,
    events: PluginEvents,
    snap: Snapshots,
    queues: RequestQueues,
    id: PluginId,
    dir: PathBuf,
    path: String,
    log: PathBuf,
    env: Vec<(&'static str, String)>,
    /// Writes of `schemify.project` (each one makes the app run `project.reload`).
    reloads: usize,
}

impl World {
    fn new(tag: &str) -> Option<World> {
        let wasm =
            std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk_manager.wasm")).ok()?;
        assert!(wasm.len() < 200 * 1024, "size limit: {}", wasm.len());
        let dir = std::env::temp_dir().join(format!("schemify-pdk-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let bin = dir.join(".fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in [
            ("ciel", FAKE_CIEL),
            ("schemify-interop", FAKE_INTEROP),
            ("git", FAKE_GIT),
            ("curl", FAKE_CURL),
            ("tar", FAKE_TAR),
            ("ssh", FAKE_SSH),
            ("pdk-scan", FAKE_PDK_SCAN),
        ] {
            std::fs::write(bin.join(name), body).unwrap();
            let ok = Command::new("chmod")
                .arg("+x")
                .arg(bin.join(name))
                .status()
                .unwrap();
            assert!(ok.success());
        }
        std::fs::write(
            dir.join("schemify.project"),
            "schemify 1 project\n\nlib mine path lib\n",
        )
        .unwrap();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Some(World::with(wasm, dir.clone(), path, dir.join(".fakelog")))
    }

    fn with(wasm: Vec<u8>, dir: PathBuf, path: String, log: PathBuf) -> World {
        let mut plugins = Plugins::new();
        let id = plugins
            .load("pdk-manager", &wasm, &GRANTS, FUEL_DEFAULT)
            .expect("loads");
        let mut w = World {
            plugins,
            events: PluginEvents::new(),
            snap: Snapshots::new(),
            queues: RequestQueues::default(),
            id,
            dir,
            path,
            log,
            env: Vec::new(),
            reloads: 0,
        };
        w.run();
        w
    }

    /// Delivers, then answers I/O until the plugin has nothing outstanding.
    fn run(&mut self) {
        for _ in 0..1000 {
            deliver(
                &mut self.plugins,
                &self.events,
                &mut self.snap,
                &mut self.queues,
            );
            self.events.clear();
            let q = std::mem::take(&mut self.queues);
            if q.io.is_empty() && q.timers.is_empty() {
                return;
            }
            // Timers fire at once here (the app waits `ms` first).
            for t in &q.timers {
                self.events
                    .push(Some(self.id), &Event::Timer { id: t.id })
                    .expect("queued");
            }
            for r in &q.io {
                let (status, bytes) = self.io(r.op, &r.args, &r.data);
                let ev = Event::IoDone {
                    handle: r.handle,
                    status,
                    bytes: &bytes,
                };
                self.events.push(Some(self.id), &ev).expect("queued");
            }
        }
        panic!("the plugin never settled");
    }

    /// The app's execution of an authorized request (`crates/app` `run_io`, spawn in the
    /// project root with the fake binaries first on `PATH`).
    fn io(&mut self, op: IoOp, args: &[String], data: &[u8]) -> (i32, Vec<u8>) {
        let gone = Code::Gone.neg();
        let path = self.dir.join(args.first().map_or("", String::as_str));
        match op {
            IoOp::FsRead => std::fs::read(&path).map_or((gone, Vec::new()), |b| (0, b)),
            IoOp::FsWrite => {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, data).unwrap();
                self.reloads += usize::from(args[0] == "schemify.project");
                (0, Vec::new())
            }
            IoOp::FsRemove if path.is_dir() => (
                std::fs::remove_dir_all(&path).map_or(-1, |()| 0),
                Vec::new(),
            ),
            IoOp::FsRemove if path.exists() => {
                (std::fs::remove_file(&path).map_or(-1, |()| 0), Vec::new())
            }
            IoOp::FsRemove => (gone, Vec::new()),
            IoOp::Spawn => {
                let mut c = Command::new(&args[0]);
                c.args(&args[1..])
                    .current_dir(&self.dir)
                    .env("PATH", &self.path)
                    .env("FAKE_LOG", &self.log)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                for (k, v) in &self.env {
                    c.env(k, v);
                }
                let mut child = c.spawn().expect("spawns");
                let mut stdin = child.stdin.take().expect("piped");
                let data = data.to_vec();
                let feed = std::thread::spawn(move || {
                    let _ = stdin.write_all(&data);
                });
                let o = child.wait_with_output().expect("runs");
                feed.join().expect("fed");
                let mut bytes = o.stdout;
                bytes.extend_from_slice(&o.stderr);
                (o.status.code().unwrap_or(-1), bytes)
            }
            op => panic!("unexpected {op:?}"),
        }
    }

    fn cmd(&mut self, name: &str, args: &str) {
        self.events
            .push(Some(self.id), &Event::Command { name, args })
            .expect("queued");
        self.run();
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(rel)).ok()
    }

    fn take_log(&self) -> Vec<String> {
        let t = std::fs::read_to_string(&self.log).unwrap_or_default();
        let _ = std::fs::remove_file(&self.log);
        t.lines().map(str::to_owned).collect()
    }

    fn ui(&self) -> String {
        let c = self.plugins.composed();
        c.of(Kind::Ui)
            .iter()
            .map(|r| r.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn faults(&self) -> Vec<String> {
        self.plugins
            .health()
            .faults()
            .iter()
            .map(|f| format!("{f:?}"))
            .collect()
    }
}

const V1: &str = "1111111111111111111111111111111111111111";
const V2: &str = "2222222222222222222222222222222222222222";

#[test]
fn install_use_remove_prune() {
    let Some(mut w) = World::new("flow") else {
        eprintln!("pdk_manager.wasm not built (./build.sh); skipped");
        return;
    };
    w.env = vec![
        ("FAIL_CIEL", "v-bad-fetch".into()),
        ("FAIL_IMPORT", "v-bad-import".into()),
    ];
    assert!(w.ui().contains("node view.pdks item"), "the View menu item");
    // The panel and the remote listing.
    w.cmd("pdk.panel", "");
    w.cmd("pdk.family.sky130", "");
    assert_eq!(w.take_log(), ["ciel ls-remote --pdk-family sky130"]);
    assert!(
        w.ui()
            .contains("node pdk.remote.1.name label\n  parent pdk.remote.1\n  label 2222222"),
        "{}",
        w.ui()
    );

    // Install V1 from the panel row: exact argv, store layout, STATE, tech.map.
    w.cmd("pdk.remote.install.0", "");
    let d = format!(".schemify/pdks/sky130/{V1}");
    assert_eq!(
        w.take_log(),
        [
            format!("ciel enable --pdk-family sky130 --pdk-root {d}/root -l sky130_fd_pr {V1}"),
            format!(
                "schemify-interop xschem-import {d}/root/sky130A/libs.tech/xschem -o {d}/lib --xschemrc {d}/root/sky130A/libs.tech/xschem/xschemrc"
            ),
        ]
    );
    assert_eq!(w.read(&format!("{d}/STATE")).as_deref(), Some("ok\n"));
    assert_eq!(
        w.read(&format!("{d}/tech.map")).as_deref(),
        Some(include_str!("../maps/sky130.map"))
    );
    assert!(
        w.dir
            .join(format!("{d}/root/sky130A/libs.tech/xschem/xschemrc"))
            .exists()
    );
    assert!(
        w.dir
            .join(format!("{d}/lib/sky130_fd_pr/nfet_01v8.shprim"))
            .exists()
    );
    assert_eq!(
        w.read(".schemify/pdks/INSTALLS").unwrap(),
        format!("sky130 {V1}\n")
    );

    // Failing steps leave no `STATE ok` (and stop the chain).
    w.cmd("pdk.install", "sky130 v-bad-fetch");
    assert_eq!(w.take_log().len(), 1, "import never ran after ciel failed");
    assert!(w.read(".schemify/pdks/sky130/v-bad-fetch/STATE").is_none());
    w.cmd("pdk.install", "sky130 v-bad-import");
    assert_eq!(w.take_log().len(), 2);
    assert!(w.read(".schemify/pdks/sky130/v-bad-import/STATE").is_none());
    assert!(
        w.read(".schemify/pdks/sky130/v-bad-import/tech.map")
            .is_none()
    );
    assert!(w.ui().contains("label incomplete"));
    assert!(
        w.ui().contains("no symbols found"),
        "the failing step's output tail: {}",
        w.ui()
    );
    // A version that was ok and is re-installed with a failure loses its `ok`.
    w.cmd("pdk.install", &format!("sky130 {V2}"));
    assert_eq!(
        w.read(&format!(".schemify/pdks/sky130/{V2}/STATE"))
            .as_deref(),
        Some("ok\n")
    );
    w.env[0].1 = V2.into();
    w.cmd("pdk.install", &format!("sky130 {V2}"));
    assert!(
        w.read(&format!(".schemify/pdks/sky130/{V2}/STATE"))
            .is_none()
    );
    w.env[0].1 = "v-bad-fetch".into();
    w.cmd("pdk.install", &format!("sky130 {V2}"));
    w.take_log();

    // `pdk.use` refuses an incomplete install, then rewrites the manifest (canonical text).
    w.cmd("pdk.use", "sky130 v-bad-import");
    assert_eq!(w.reloads, 0);
    w.cmd("pdk.use", &format!("sky130 {V1}"));
    assert_eq!(w.reloads, 1, "one manifest write → one project.reload");
    assert_eq!(
        w.read("schemify.project").unwrap(),
        format!(
            "schemify 1 project\n\nlib mine path lib\nlib sky130_fd_pr path {d}/lib/sky130_fd_pr\nlib tech map {d}/tech.map\n\nset PDK_ROOT {d}/root\nset PDK sky130A\nset PDK_MODELS ${{PDK_ROOT}}/sky130A/libs.tech/combined/sky130.lib.spice\nset SKYWATER_MODELS ${{PDK_ROOT}}/${{PDK}}/libs.tech/combined\n"
        )
    );
    assert!(w.ui().contains("label active"));

    // `pdk.remove` refuses the active and the locked version; removes a free one.
    std::fs::write(
        w.dir.join("schemify.lock"),
        format!("schemify 1 lock\nlocked lib sky130_fd_pr path {}/.schemify/pdks/sky130/{V2}/lib/sky130_fd_pr 00\n", w.dir.display()),
    )
    .unwrap();
    w.cmd("pdk.remove", &format!("sky130 {V1}"));
    assert!(w.dir.join(&d).exists(), "active: refused");
    w.cmd("pdk.remove", &format!("sky130 {V2}"));
    assert!(
        w.dir.join(format!(".schemify/pdks/sky130/{V2}")).exists(),
        "locked: refused"
    );
    assert!(w.ui().contains("pinned by schemify.lock"));

    // `pdk.prune` lists, then removes exactly the unreferenced installs (complete or not).
    w.cmd("pdk.install", "gf180mcu spare");
    let before = w.read(".schemify/pdks/INSTALLS").unwrap();
    w.cmd("pdk.prune", "");
    assert_eq!(
        w.read(".schemify/pdks/INSTALLS").unwrap(),
        before,
        "listing only"
    );
    assert!(
        w.ui()
            .contains("Removes 3: gf180mcu spare, sky130 v-bad-f, sky130 v-bad-i"),
        "{}",
        w.ui()
    );
    w.cmd("pdk.prune", "confirm");
    for gone in [
        "gf180mcu/spare",
        "sky130/v-bad-fetch",
        "sky130/v-bad-import",
    ] {
        assert!(
            !w.dir.join(format!(".schemify/pdks/{gone}")).exists(),
            "{gone}"
        );
    }
    assert!(w.dir.join(&d).exists() && w.dir.join(format!(".schemify/pdks/sky130/{V2}")).exists());
    assert_eq!(
        w.read(".schemify/pdks/INSTALLS").unwrap(),
        format!("sky130 {V1}\nsky130 {V2}\n")
    );
    // Switching technology: the previous family's records go.
    std::fs::remove_file(w.dir.join("schemify.lock")).unwrap();
    w.cmd("pdk.remove", &format!("sky130 {V2}"));
    assert!(!w.dir.join(format!(".schemify/pdks/sky130/{V2}")).exists());
    assert!(w.faults().is_empty(), "{:?}", w.faults());
}

#[test]
fn bad_arguments_change_nothing() {
    let Some(mut w) = World::new("args") else {
        return;
    };
    let manifest = w.read("schemify.project").unwrap();
    for (cmd, args) in [
        ("pdk.install", "sky130 latest"),
        ("pdk.install", "sky130 ../../x"),
        ("pdk.install", "asap7 v1"),
        ("pdk.install", "sky130"),
        ("pdk.use", "sky130 never-installed"),
        ("pdk.remove", "sky130 .."),
    ] {
        w.cmd(cmd, args);
        assert!(w.take_log().is_empty(), "{cmd} {args}: nothing spawned");
    }
    assert_eq!(w.read("schemify.project").unwrap(), manifest);
    assert!(!w.dir.join(".schemify/pdks/sky130").exists());
    assert_eq!(w.reloads, 0);
}

const ASAP7: &str = "58d72c9d291e186a77468586ab0c43d8a21eda6a";
const VA_REV: &str = "6099b4e12e6f66ff530f59b8d01fb909094be85a";

/// Writes `kits/<name>/models/hspice/7nm_{TT,FF,SS}_160803.pm` holding `card`.
fn kit(w: &World, name: &str, card: &str) {
    let d = w.dir.join(format!("kits/{name}/models/hspice"));
    std::fs::create_dir_all(&d).unwrap();
    for c in ["TT", "FF", "SS"] {
        std::fs::write(d.join(format!("7nm_{c}_160803.pm")), card).unwrap();
    }
}

#[test]
fn git_url_and_local_kits() {
    let Some(mut w) = World::new("academic") else {
        return;
    };
    w.env = vec![("FAIL_GIT", "none".into())];
    // A git family lists its pinned rev without spawning anything.
    w.cmd("pdk.panel", "");
    w.cmd("pdk.family.asap7", "");
    assert!(w.take_log().is_empty());
    assert!(w.ui().contains("label 58d72c9"), "{}", w.ui());

    // Install from the panel: exact argv, generated store, STATE last.
    w.cmd("pdk.remote.install.0", "");
    let d = format!(".schemify/pdks/asap7/{ASAP7}");
    assert_eq!(
        w.take_log(),
        [
            format!(
                "git clone --quiet --no-checkout https://github.com/The-OpenROAD-Project/asap7_pdk_r1p7 {d}/root"
            ),
            format!("git -C {d}/root checkout --quiet {ASAP7}"),
            format!("git clone --quiet --no-checkout https://github.com/dwarning/VA-Models {d}/va"),
            format!("git -C {d}/va checkout --quiet {VA_REV}"),
        ]
    );
    assert_eq!(
        w.read(&format!("{d}/STATE")).as_deref(),
        Some("ok\n"),
        "{}",
        w.ui()
    );
    assert_eq!(
        w.read(&format!("{d}/models/7nm_SS_160803.pm")).unwrap(),
        "* derived by pdk-manager for ngspice OSDI (bsimcmg_va); the kit's card follows\n* BSD 3-Clause\n.model nmos_rvt bsimcmg_va type=1\n+l = 2.1e-008\n.model pmos_rvt bsimcmg_va type=-1\n+l = 2.1e-008\n"
    );
    assert!(
        w.read(&format!("{d}/models/corners.lib"))
            .unwrap()
            .contains(".lib ss\n.include \"7nm_SS_160803.pm\"\n.endl ss\n")
    );
    assert_eq!(
        w.read(&format!("{d}/lib/asap7/nmos_rvt.shprim")).as_deref(),
        Some(include_str!(
            "fixtures/schemify/tech/pdk/asap7/asap7/nmos_rvt.shprim"
        ))
    );
    assert_eq!(
        w.read(&format!("{d}/lib/schemify.project")).as_deref(),
        Some("schemify 1 project\n\nlib asap7 path asap7\n")
    );
    assert_eq!(
        w.read(&format!("{d}/tech.map")).as_deref(),
        Some(include_str!("../maps/asap7.map"))
    );
    assert!(w.read(&format!("{d}/ROOT")).is_none());

    // `pdk.use`: the generated library, the map, the corner library.
    w.cmd("pdk.use", &format!("asap7 {ASAP7}"));
    assert_eq!(
        w.read("schemify.project").unwrap(),
        format!(
            "schemify 1 project\n\nlib mine path lib\nlib asap7 path {d}/lib/asap7\nlib tech map {d}/tech.map\n\nset PDK_ROOT {d}/root\nset PDK_MODELS {d}/models/corners.lib\n"
        )
    );

    // A failing checkout (here: of the model source) leaves no `STATE ok`.
    w.env[0].1 = VA_REV.into();
    w.cmd("pdk.install", &format!("asap7 {ASAP7}"));
    assert_eq!(w.take_log().len(), 4);
    assert!(w.read(&format!("{d}/STATE")).is_none());
    assert!(w.ui().contains("reference is not a tree"), "{}", w.ui());
    w.env[0].1 = "none".into();
    w.cmd("pdk.install", &format!("asap7 {ASAP7}"));
    assert_eq!(w.read(&format!("{d}/STATE")).as_deref(), Some("ok\n"));
    w.take_log();

    // Refused arguments spawn nothing: an unpinned version, an absolute path for a kit whose
    // cards are read, a path into the store.
    for args in [
        "asap7 v1",
        "asap7 mine /opt/asap7",
        "freepdk45 x .schemify/pdks/freepdk45/x/root",
        "freepdk45 x kits/fp45 extra",
    ] {
        w.cmd("pdk.install", args);
        assert!(w.take_log().is_empty(), "{args}");
    }
    assert!(!w.dir.join(".schemify/pdks/asap7/mine").exists());
    assert!(!w.dir.join(".schemify/pdks/freepdk45").exists());

    // A local kit: only the model source is fetched; the kit is read where it is.
    kit(&w, "asap7", "* local\n.model nmos_rvt nmos level=72\n");
    w.cmd("pdk.install", "asap7 mine kits/asap7");
    let m = ".schemify/pdks/asap7/mine";
    assert_eq!(
        w.take_log(),
        [
            format!("git clone --quiet --no-checkout https://github.com/dwarning/VA-Models {m}/va"),
            format!("git -C {m}/va checkout --quiet {VA_REV}"),
        ]
    );
    assert_eq!(
        w.read(&format!("{m}/ROOT")).as_deref(),
        Some("kits/asap7\n")
    );
    assert_eq!(w.read(&format!("{m}/STATE")).as_deref(), Some("ok\n"));
    assert!(!w.dir.join(format!("{m}/root")).exists(), "nothing copied");
    w.cmd("pdk.use", "asap7 mine");
    assert!(
        w.read("schemify.project")
            .unwrap()
            .contains("set PDK_ROOT kits/asap7\n")
    );
    // Removing a local install never touches the kit (negative space).
    w.cmd("pdk.use", &format!("asap7 {ASAP7}"));
    w.cmd("pdk.remove", "asap7 mine");
    assert!(!w.dir.join(m).exists());
    assert!(
        w.dir
            .join("kits/asap7/models/hspice/7nm_TT_160803.pm")
            .exists()
    );

    // Cards of another level are refused at install: no `STATE ok`.
    kit(&w, "bsim4", ".model nmos_rvt nmos level = 54\n");
    w.cmd("pdk.install", "asap7 bad kits/bsim4");
    assert!(w.read(".schemify/pdks/asap7/bad/STATE").is_none());
    assert!(w.ui().contains("not a level-72 card"), "{}", w.ui());
    w.take_log();

    // A `url` download whose sha256 differs is refused before anything is unpacked.
    w.cmd("pdk.family.freepdk45", "");
    w.cmd("pdk.remote.install.0", "");
    let f = ".schemify/pdks/freepdk45/5b93a0b1e12d";
    assert_eq!(
        w.take_log(),
        [format!(
            "curl -fsSL --create-dirs -o {f}/root/source.tar.gz https://codeload.github.com/baichen318/FreePDK45/tar.gz/a75cb116a3efd1d3ae05b695285452e6ffbc7ca5"
        )]
    );
    assert!(w.read(&format!("{f}/STATE")).is_none());
    assert!(w.read(&format!("{f}/tech.map")).is_none());
    assert!(w.ui().contains("sha256 mismatch"), "{}", w.ui());

    // A kit outside the project (absolute path): referenced in place, never removed.
    let ext = w.dir.with_extension("ext");
    std::fs::create_dir_all(ext.join("FreePDK45")).unwrap();
    let ext_s = ext.display().to_string();
    w.cmd("pdk.install", &format!("freepdk45 ncsu {ext_s}"));
    assert!(w.take_log().is_empty(), "nothing spawned");
    let n = ".schemify/pdks/freepdk45/ncsu";
    assert_eq!(w.read(&format!("{n}/STATE")).as_deref(), Some("ok\n"));
    assert!(w.read(&format!("{n}/models/corners.lib")).unwrap().contains(&format!(".include \"{ext_s}/FreePDK45/ncsu_basekit/models/hspice/tran_models/models_nom/NMOS_VTL.inc\"\n")));
    assert_eq!(
        w.read(&format!("{n}/lib/freepdk45/pmos_vtl.shprim"))
            .as_deref(),
        Some(include_str!(
            "fixtures/schemify/tech/pdk/freepdk45/freepdk45/pmos_vtl.shprim"
        ))
    );
    w.cmd("pdk.use", "freepdk45 ncsu");
    assert!(
        w.read("schemify.project")
            .unwrap()
            .contains(&format!("set PDK_ROOT {ext_s}\n"))
    );
    w.cmd("pdk.use", &format!("asap7 {ASAP7}"));
    w.cmd("pdk.prune", "");
    w.cmd("pdk.prune", "confirm");
    assert!(!w.dir.join(n).exists(), "pruned");
    assert!(ext.join("FreePDK45").is_dir(), "the kit itself stays");
    assert!(w.dir.join(&d).exists(), "the active install stays");
    let _ = std::fs::remove_dir_all(&ext);
    assert!(w.faults().is_empty(), "{:?}", w.faults());
}

const SSH: &str = "ssh -T -x -a -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=2 -o ForwardAgent=no -o ClearAllForwardings=yes -o PermitLocalCommand=no -o StrictHostKeyChecking=yes --";

/// A fixture kit outside the project (`<project>.kit`, an absolute path the grammar accepts):
/// `models/top.lib` includes `inc.spice`; both hold the marker `SECRET`, which must never
/// reach the project tree or the panel.
fn remote_kit(w: &World) -> String {
    let kit = w.dir.with_extension("kit");
    let _ = std::fs::remove_dir_all(&kit);
    std::fs::create_dir_all(kit.join("models")).unwrap();
    std::fs::write(
        kit.join("models/top.lib"),
        "* SECRET corner library\n.lib tt\n.include inc.spice\n.endl tt\n",
    )
    .unwrap();
    std::fs::write(
        kit.join("models/inc.spice"),
        ".subckt nch d g s b w=1u l=1u\n.model nmos_SECRET nmos level=54 vth0=0.42\n.ends nch\n",
    )
    .unwrap();
    let kit = kit.display().to_string();
    let ok = kit.strip_prefix('/').unwrap().split('/').all(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    });
    assert!(ok, "the temp dir {kit} is not a grammar path");
    kit
}

/// Whether `needle` occurs in any file under `dir`.
fn tree_contains(dir: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(dir).unwrap().flatten().any(|e| {
        let p = e.path();
        if p.is_dir() {
            tree_contains(&p, needle)
        } else {
            std::fs::read(&p).is_ok_and(|b| b.windows(needle.len()).any(|x| x == needle))
        }
    })
}

#[test]
fn remote_kits_need_trust_and_never_leave_the_host() {
    let Some(mut w) = World::new("remote") else {
        return;
    };
    let kit = remote_kit(&w);
    let dest = format!("pdk@pdkhost:{kit}");
    w.cmd("pdk.panel", "");
    // Refused records write nothing.
    for args in [
        format!("sky130 {dest} models/top.lib spice"),
        format!("tsmc28 -oProxyCommand=x:{kit} models/top.lib spice"),
        format!("tsmc28 {dest} ../top.lib spice"),
        format!("tsmc28 {dest} models/top.lib eldo"),
        format!("tsmc28 \"pdk@pdkhost:{kit};id\" models/top.lib spice"),
    ] {
        w.cmd("pdk.remote.add", &args);
        assert!(w.read("pdks.remote").is_none(), "{args}");
    }
    w.cmd(
        "pdk.remote.add",
        &format!("tsmc28 {dest} models/top.lib spice"),
    );
    assert_eq!(
        w.read("pdks.remote").unwrap().lines().nth(1).unwrap(),
        format!("remote tsmc28 {dest} models models/top.lib dialect spice")
    );
    // Untrusted: no ssh, the panel names the exact destination with a Trust button.
    w.cmd("pdk.remote.refresh", "tsmc28");
    assert!(w.take_log().is_empty(), "nothing spawned");
    let ui = w.ui();
    assert!(ui.contains(&format!("label {dest}\n")), "{ui}");
    assert!(
        ui.contains("command pdk.remote.row.trust.0\n  tooltip"),
        "{ui}"
    );
    assert!(ui.contains("is not trusted in this checkout"), "{ui}");
    w.cmd("pdk.remote.row.trust.0", "");
    assert_eq!(
        w.read(".schemify/pdk-manager/trusted").unwrap(),
        format!("{dest}\n")
    );

    // Agent mode: the probe, then the scan; the cache and the manifest rows.
    w.cmd("pdk.remote.refresh", "tsmc28");
    assert_eq!(
        w.take_log(),
        [
            format!("{SSH} pdk@pdkhost schemify-interop pdk-scan --protocol"),
            "schemify-interop pdk-scan --protocol".to_owned(),
            format!(
                "{SSH} pdk@pdkhost schemify-interop pdk-scan --root '{kit}' --entry 'models/top.lib' --dialect 'spice' --lib 'tsmc28'"
            ),
            format!(
                "schemify-interop pdk-scan --root {kit} --entry models/top.lib --dialect spice --lib tsmc28"
            ),
        ]
    );
    let cache = ".schemify/pdk-manager/remote/tsmc28";
    assert_eq!(
        w.read(&format!("{cache}/nch.shprim")).as_deref(),
        Some("schemify 1 shprim\n")
    );
    assert!(w.read(&format!("{cache}/pch.shprim")).is_some());
    let manifest = format!(
        "schemify 1 project\n\nlib mine path lib\nlib tsmc28 path {cache}\n\nset PDK_MODELS {kit}/models/top.lib\n"
    );
    assert_eq!(w.read("schemify.project").unwrap(), manifest);
    assert!(
        w.ui()
            .contains("label \"tsmc28: 2 cells from pdk@pdkhost (agent), 1 issues\""),
        "{}",
        w.ui()
    );

    // Fallback: no agent on the host. The script from the local interop, framed reads of the
    // entry and of what the scan needs, the local `--stdin` scan; the cache is rewritten.
    std::fs::write(w.dir.join(format!("{cache}/stale.shprim")), "old").unwrap();
    w.env = vec![("NO_AGENT", "1".into())];
    w.cmd("pdk.remote.refresh", "tsmc28");
    let stdin_scan = format!(
        "schemify-interop pdk-scan --stdin --dialect spice --entry {kit}/models/top.lib --lib tsmc28"
    );
    assert_eq!(
        w.take_log(),
        [
            format!("{SSH} pdk@pdkhost schemify-interop pdk-scan --protocol"),
            "schemify-interop pdk-scan --remote-script".to_owned(),
            format!("{SSH} pdk@pdkhost sh -s -- '{kit}/models/top.lib'"),
            stdin_scan.clone(),
            format!("{SSH} pdk@pdkhost sh -s -- '{kit}/models/inc.spice'"),
            stdin_scan,
        ]
    );
    assert!(
        w.read(&format!("{cache}/stale.shprim")).is_none(),
        "rewritten"
    );
    assert!(w.read(&format!("{cache}/nch.shprim")).is_some());
    assert_eq!(w.read("schemify.project").unwrap(), manifest);
    assert!(w.ui().contains("from pdk@pdkhost (fallback)"), "{}", w.ui());
    assert!(
        !tree_contains(&w.dir, b"SECRET"),
        "kit bytes reached the project"
    );
    assert!(!w.ui().contains("SECRET"));
    w.env.clear();

    // Exports refuse a remote family.
    w.cmd("pdk.export", "tsmc28 v1 xschem");
    assert!(w.take_log().is_empty());
    assert!(
        w.ui().contains("a remote family is never exported"),
        "{}",
        w.ui()
    );

    // A changed destination needs trust again.
    w.cmd(
        "pdk.remote.add",
        &format!("tsmc28 pdk@otherhost:{kit} models/top.lib spice"),
    );
    w.cmd("pdk.remote.refresh", "tsmc28");
    assert!(
        w.take_log().is_empty(),
        "the new destination is not trusted"
    );
    assert!(
        w.ui()
            .contains(&format!("pdk@otherhost:{kit} is not trusted")),
        "{}",
        w.ui()
    );

    // Forget: the cache and the manifest rows go; the record and the trust stay.
    let reloads = w.reloads;
    w.cmd("pdk.remote.forget", "tsmc28");
    assert!(!w.dir.join(cache).exists());
    assert_eq!(
        w.read("schemify.project").unwrap(),
        "schemify 1 project\n\nlib mine path lib\n"
    );
    assert_eq!(w.reloads, reloads + 1);
    assert!(w.read("pdks.remote").unwrap().contains("tsmc28"));

    // ssh's own failure: its message, no fallback.
    w.cmd(
        "pdk.remote.add",
        &format!("lab pdk@down:{kit} models/top.lib spice"),
    );
    w.cmd("pdk.remote.trust", "lab");
    w.cmd("pdk.remote.refresh", "lab");
    assert_eq!(w.take_log().len(), 1, "the probe only");
    assert!(
        w.ui().contains(
            "lab: ssh to pdk@down failed: ssh: connect to host down port 22: Connection refused"
        ),
        "{}",
        w.ui()
    );
    // The scan's own error is the outcome.
    w.cmd(
        "pdk.remote.add",
        &format!("lab pdk@pdkhost:{kit} models/missing.lib spice"),
    );
    w.cmd("pdk.remote.trust", "lab");
    w.cmd("pdk.remote.refresh", "lab");
    assert!(
        w.ui()
            .contains("lab: pdk-scan: cannot read models/missing.lib"),
        "{}",
        w.ui()
    );
    assert!(!w.dir.join(".schemify/pdk-manager/remote/lab").exists());
    let _ = std::fs::remove_dir_all(&kit);
    assert!(w.faults().is_empty(), "{:?}", w.faults());
}

#[test]
fn exports_spawn_interop_on_local_installs() {
    let Some(mut w) = World::new("export") else {
        return;
    };
    let ext = w.dir.with_extension("fp45");
    std::fs::create_dir_all(ext.join("FreePDK45")).unwrap();
    w.cmd("pdk.panel", "");
    w.cmd("pdk.export", "freepdk45 ncsu xschem");
    assert!(w.take_log().is_empty(), "not installed: refused");
    w.cmd("pdk.install", &format!("freepdk45 ncsu {}", ext.display()));
    w.cmd("pdk.export", "freepdk45 ncsu xschem");
    let d = ".schemify/pdks/freepdk45/ncsu";
    assert_eq!(
        w.take_log(),
        [format!(
            "schemify-interop xschem-export {d}/lib . -o ../export/xschem"
        )]
    );
    assert!(
        w.ui()
            .contains(&format!("exported freepdk45 ncsu into {d}/export")),
        "{}",
        w.ui()
    );
    w.cmd("pdk.row.virtuoso.0", "");
    assert_eq!(
        w.take_log(),
        [format!(
            "schemify-interop virtuoso-export {d}/lib . -o ../export/freepdk45.sfyx"
        )]
    );
    // An imported family exports the same way: Interop lists its library directory.
    w.cmd("pdk.install", &format!("sky130 {V1}"));
    w.take_log();
    w.cmd("pdk.export", &format!("sky130 {V1} virtuoso"));
    assert_eq!(
        w.take_log(),
        [format!(
            "schemify-interop virtuoso-export .schemify/pdks/sky130/{V1}/lib . -o ../export/sky130.sfyx"
        )]
    );
    // A model dialect is converted from the kit itself: the local path `ROOT` records.
    w.cmd("pdk.export", "freepdk45 ncsu spectre");
    let log = w.take_log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(
        log[0].starts_with(&format!(
            "schemify-interop pdk-models --root {} --entry ",
            ext.display()
        )) && log[0].ends_with(&format!("--from spice --to spectre -o {d}/export/spectre")),
        "{log:?}"
    );
    for bad in ["freepdk45 ncsu eldo", "freepdk45 ncsu", "nope v xschem"] {
        w.cmd("pdk.export", bad);
        assert!(w.take_log().is_empty(), "{bad}");
    }
    let _ = std::fs::remove_dir_all(&ext);
    assert!(w.faults().is_empty(), "{:?}", w.faults());
}

/// The real `schemify-interop` (the dev shell's, from the `interop` flake input) behind the
/// fake `ssh`: agent mode, then the fallback with its real remote script. Skips when that
/// interop does not speak `pdk-scan` protocol 1 yet.
#[test]
fn real_interop_scans_a_remote_kit() {
    let Some(mut w) = World::new("real-remote") else {
        return;
    };
    let real = Command::new("schemify-interop")
        .args(["pdk-scan", "--protocol"])
        .output();
    if !real.is_ok_and(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l == "pdkscan 1")
    }) {
        eprintln!("schemify-interop on PATH does not speak pdk-scan 1; skipped");
        return;
    }
    std::fs::remove_file(w.dir.join(".fakebin/schemify-interop")).unwrap();
    w.cmd("pdk.panel", "");
    let kit = remote_kit(&w);
    let dest = format!("pdk@pdkhost:{kit}");
    w.cmd(
        "pdk.remote.add",
        &format!("tsmc28 {dest} models/top.lib spice tt"),
    );
    w.cmd("pdk.remote.trust", "tsmc28");
    for (mode, env) in [("agent", ""), ("fallback", "1")] {
        w.env = vec![("NO_AGENT", env.into())];
        w.cmd("pdk.remote.refresh", "tsmc28");
        let ui = w.ui();
        assert!(
            ui.contains(&format!("from pdk@pdkhost ({mode})")),
            "{mode}: {ui}"
        );
        let cache = w.dir.join(".schemify/pdk-manager/remote/tsmc28");
        assert!(std::fs::read_dir(&cache).unwrap().count() > 0);
        assert!(
            w.read("schemify.project")
                .unwrap()
                .contains(&format!("set PDK_MODELS {kit}/models/top.lib\n"))
        );
        assert!(
            !tree_contains(&w.dir.join(".schemify"), b"vth0"),
            "model data reached the cache"
        );
    }
    let _ = std::fs::remove_dir_all(&kit);
}

/// Nightly-style: the real `ciel` (network) and `schemify-interop` on `PATH` install the newest sky130
/// into a scratch project, and the store holds an imported `sky130_fd_pr` library.
#[test]
#[ignore = "network and a real ciel: run with --ignored"]
fn real_ciel_installs_sky130() {
    let wasm = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk_manager.wasm"))
        .expect("built");
    let dir = std::env::temp_dir().join(format!("schemify-pdk-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("schemify.project"), "schemify 1 project\n").unwrap();
    let path = std::env::var("PATH").unwrap_or_default();
    let mut w = World::with(wasm, dir.clone(), path, dir.join(".log"));
    w.cmd("pdk.panel", "");
    w.cmd("pdk.family.sky130", "");
    let ui = w.ui();
    let v = ui
        .split("tooltip ")
        .filter_map(|t| t.split_whitespace().next())
        .find(|t| t.len() == 40)
        .expect("ciel ls-remote listed a version")
        .to_owned();
    w.cmd("pdk.install", &format!("sky130 {v}"));
    let d = format!(".schemify/pdks/sky130/{v}");
    assert_eq!(
        w.read(&format!("{d}/STATE")).as_deref(),
        Some("ok\n"),
        "{}",
        w.ui()
    );
    assert!(dir.join(format!("{d}/lib/sky130_fd_pr")).is_dir());
    w.cmd("pdk.use", &format!("sky130 {v}"));
    assert!(w.read("schemify.project").unwrap().contains("lib tech map"));
}

/// The VTC bench of the `tech:` inverter (`tests/fixtures/schemify/tech/inv.shcomp`): a DC
/// sweep of the input from 0 to `vdd` at the PDK's `tt` corner, through `PDK_MODELS`.
fn vtc_bench(vdd: &str) -> String {
    format!(
        "schemify 1 shbench\n  doc \"inverter VTC\"\n  corner tt\n\ninst 0x0001 ./inv\n  at 0 0 R0\n  name X1\n\nlabel 0n0001 -40 0 R0 a\nlabel 0n0002 40 0 R0 y\nlabel 0n0003 0 -30 R0 vdd\nlabel 0n0004 0 30 R0 0\n\nvar vdd {vdd}\n  unit V\n\ncorner tt\n  lib ${{PDK_MODELS}} tt\n\noutput voh \"at(v(y),0)\"\noutput vol \"at(v(y),0.999*vdd)\"\noutput vm \"cross(v(y)-v(a),0)\"\n\ncode dc spice\n  | VDD vdd 0 {{vdd}}\n  | VIN a 0 0\n  | .dc VIN 0 {{vdd}} {{vdd/200}}\n"
    )
}

/// Nightly-style, network: installs each academic kit for real (`git`, `curl`, `tar` and the
/// `schemify` CLI on `PATH`; for asap7 also OpenVAF, named by `$OPENVAF`, default
/// `openvaf-r`), uses it, and runs the `tech:` inverter's DC sweep in ngspice. The VTC must
/// switch: output within 10 % of the rails at both ends of the sweep and the crossing
/// `vout = vin` within 0.3 vdd to 0.7 vdd. `SCHEMIFY_KITS=freepdk45` (or `asap7`) runs one.
/// Run: `cargo test --test guest real_kits -- --ignored --nocapture`.
#[test]
#[ignore = "network, git/curl/tar, ngspice and the schemify CLI: run with --ignored"]
fn real_kits_simulate_an_inverter() {
    let wasm = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("pdk_manager.wasm"))
        .expect("built");
    let kits = std::env::var("SCHEMIFY_KITS").unwrap_or_else(|_| "freepdk45 asap7".into());
    for (family, version, vdd) in [
        ("freepdk45", "5b93a0b1e12d", 1.1),
        ("asap7", "58d72c9d291e186a77468586ab0c43d8a21eda6a", 0.7),
    ] {
        if !kits.split_whitespace().any(|k| k == family) {
            continue;
        }
        let dir =
            std::env::temp_dir().join(format!("schemify-pdk-{family}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".xdg/config/schemify")).unwrap();
        std::fs::write(dir.join("schemify.project"), "schemify 1 project\n").unwrap();
        let openvaf = std::env::var("OPENVAF").unwrap_or_else(|_| "openvaf-r".into());
        std::fs::write(
            dir.join(".xdg/config/schemify/init.lua"),
            format!("schemify.sim(\"openvaf\", {{ path = \"{openvaf}\" }})\n"),
        )
        .unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        let mut w = World::with(wasm.clone(), dir.clone(), path, dir.join(".log"));
        w.cmd("pdk.panel", "");
        w.cmd("pdk.install", &format!("{family} {version}"));
        let d = format!(".schemify/pdks/{family}/{version}");
        assert_eq!(
            w.read(&format!("{d}/STATE")).as_deref(),
            Some("ok\n"),
            "{}\n{:?}",
            w.ui(),
            w.faults()
        );
        // The kit's own files are the store: a dialect is written from them on demand.
        assert!(!dir.join(&d).join("rack").exists(), "no IR copy of the kit");
        w.cmd("pdk.export", &format!("{family} {version} spectre"));
        assert!(
            dir.join(&d).join("export/spectre").is_dir(),
            "{}\n{:?}",
            w.ui(),
            w.faults()
        );
        w.cmd("pdk.use", &format!("{family} {version}"));
        assert!(
            w.read("schemify.project")
                .unwrap()
                .contains("set PDK_MODELS")
        );
        std::fs::write(
            dir.join("inv.shcomp"),
            include_str!("fixtures/schemify/tech/inv.shcomp"),
        )
        .unwrap();
        std::fs::write(dir.join("inv_tb.shbench"), vtc_bench(&vdd.to_string())).unwrap();
        let out = Command::new("schemify")
            .args(["run", "inv_tb.shbench"])
            .current_dir(&dir)
            .env("XDG_CONFIG_HOME", dir.join(".xdg/config"))
            .env("XDG_DATA_HOME", dir.join(".xdg/data"))
            .output()
            .expect("schemify on PATH");
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        let value = |name: &str| -> f64 {
            let key = format!("value {name} ");
            let line = text.lines().find_map(|l| l.trim().strip_prefix(&key));
            line.and_then(|v| v.trim().parse().ok())
                .unwrap_or_else(|| panic!("{family}: no {name} in\n{text}"))
        };
        let (voh, vol, vm) = (value("voh"), value("vol"), value("vm"));
        eprintln!("{family} {version}: voh {voh} vol {vol} vm {vm} (vdd {vdd})");
        assert!(
            voh > 0.9 * vdd && vol < 0.1 * vdd,
            "{family}: does not switch\n{text}"
        );
        assert!(
            vm > 0.3 * vdd && vm < 0.7 * vdd,
            "{family}: switching point {vm}\n{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The package manifest (`schemify.project`, read by `schemify pack`) is canonical, names the
/// committed module, and requests exactly the grants these tests run the guest with.
#[test]
fn package_manifest_requests_the_tested_grants() {
    let text = include_str!("../schemify.project");
    let doc = schemify_format::parse(text.as_bytes()).expect("parses");
    assert_eq!(schemify_format::write(&doc), text, "canonical");
    let mut lines = text.lines().skip(2);
    assert_eq!(
        lines.next(),
        Some("plugin pdk-manager path pdk_manager.wasm")
    );
    let grants: Vec<&str> = lines.filter_map(|l| l.strip_prefix("  grant ")).collect();
    assert_eq!(grants, GRANTS);
}
