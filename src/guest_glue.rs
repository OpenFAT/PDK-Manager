//! Events → I/O chains → [`Model`] → panel. Every completion is matched to its request by
//! handle; at most [`PENDING_MAX`] requests are outstanding. Each step waits for the previous
//! one's `IoDone`, so nothing relies on the host executing requests in issue order.
//!
//! - refresh: read the index, the manifest and the lockfile (then each `STATE`); when the last
//!   read lands, recompute the references and run the action that asked for fresh data;
//! - `pdk.install F V [path]`: the recipe's [`install_steps`] in order (remove `STATE` → add
//!   to the index → fetch or record the local path → import or generate devices → corner
//!   library → `tech.map` → `STATE` = `ok`); a failing step stops the chain, so `STATE` stays
//!   absent (spec §3);
//! - `pdk.use F V`: refresh → refuse unless complete → read `ROOT` (a local kit) → read the
//!   library manifest → write `schemify.project` (the app then runs `project.reload`);
//! - `pdk.remove F V` / `pdk.prune [confirm]`: refresh → refuse active/locked → `fs_remove`
//!   each version directory → rewrite the index → refresh;
//! - `pdk.export F V xschem|virtuoso`: refresh → refuse a remote family or an incomplete
//!   install → spawn the Interop export;
//! - `pdk.remote.add|trust|forget F`: refresh → rewrite `pdks.remote`, the trusted list, or
//!   remove the cache and the manifest rows → refresh;
//! - `pdk.remote.refresh F`: refresh → refuse an untrusted destination → probe the agent →
//!   agent scan, or the fallback (the remote script from the local Interop → framed reads →
//!   local `pdk-scan --stdin`, repeated while it needs files) → clear and write the cache →
//!   write `schemify.project` → refresh. Scan and frame bytes stay in memory; only the
//!   one-line outcome is shown or logged.

use std::cell::RefCell;

use schemify_abi::guest::{self, Host};
use schemify_abi::{Code, Event, Kind, Level, Out, text};

use crate::install::{ARCHIVE_BYTES_MAX, HASH_CHUNK, Sha256, Step, derive_cmg, install_steps};
use crate::remote::{self, REMOTES, Remote, TRUSTED};
use crate::{
    Fv, INDEX, Job, Model, RECIPES, Source, export_argv, forget_manifest, local_refusal,
    parse_index, parse_versions, pinned_version, prune_plan, recipe, references, remote_manifest,
    remove_refusal, tail, use_manifest, valid_version, version_dir, write_index,
};

guest::export!(on_event);

const PENDING_MAX: usize = 64;
/// The timer that resumes hashing a download in the next event (plugin-abi §5 `timer`).
const HASH_TIMER: u32 = 1;
const EMPTY_MANIFEST: &[u8] = b"schemify 1 project\n";
/// Cache writes a refresh keeps outstanding.
const WRITES_AT_ONCE: u32 = 32;

/// What a pending handle is for.
enum Io {
    Index,
    State(Fv),
    Manifest,
    Lock,
    ListRemote(String),
    /// Install step `i` of `State::steps`.
    Step(usize),
    /// The write half of a [`Step::Derive`].
    DeriveWrite(usize),
    UseRoot(Fv),
    UseImport(Fv, String),
    UseWrite(Fv),
    Remove(Fv),
    RemoveIndex,
    Export(Fv),
    Remotes,
    Trusted,
    /// A write of `pdks.remote` or of the trusted list.
    RemoteConfig,
    /// The scan steps of [`State::scan`].
    Probe,
    Agent,
    Script,
    Frames,
    StdinScan,
    CacheClear,
    CacheWrite,
    RemoteUse,
    Forget(String),
    ForgetUse(String),
}

/// What runs once a refresh lands.
enum Action {
    Use(Fv),
    Remove(Fv),
    Prune { confirm: bool },
    Export(Fv, &'static str),
    RemoteAdd(Remote),
    Trust(String),
    Scan(String),
    Forget(String),
}

#[derive(Default)]
struct State {
    m: Model,
    pending: Vec<(u32, Io)>,
    /// Refresh reads still outstanding.
    refreshing: u32,
    manifest: Option<Vec<u8>>,
    lock: Option<Vec<u8>>,
    after: Option<Action>,
    /// Removals of the running batch still outstanding, and the ones done.
    removing: u32,
    removed: Vec<Fv>,
    /// The running install's steps.
    steps: Vec<Step>,
    /// A download being hashed, [`HASH_CHUNK`] bytes per event.
    hashing: Option<Hashing>,
    /// The remote scan in progress.
    scan: Option<Scanning>,
}

/// A `pdk.remote.refresh` in progress (design §4.5). Every byte it holds is the kit's and
/// stays here: never shown, logged or written except the cells into the cache.
struct Scanning {
    r: Remote,
    /// `agent`, or `fallback` once the probe failed.
    mode: &'static str,
    /// The remote script (fallback).
    script: Vec<u8>,
    /// The frame records of every round so far, without their headers and trailers.
    frames: Vec<u8>,
    files: usize,
    rounds: u32,
    /// The cache files still to write, and the writes outstanding.
    writes: Vec<(String, Vec<u8>)>,
    writing: u32,
    cells: usize,
    issues: usize,
}

/// The [`Step::Verify`] in progress: its step, the file, how far it is hashed.
struct Hashing {
    step: usize,
    data: Vec<u8>,
    at: usize,
    sha: Sha256,
}

thread_local! {
    /// Plugin state; the guest is single-threaded and the cell outlives events.
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn on_event(ev: Event<'_>, host: &Host, out: &mut Out) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        match ev {
            Event::Load => refresh(&mut s, host, out, None),
            Event::Command { name, args } => command(&mut s, host, out, name, args),
            Event::Timer { id: HASH_TIMER } => hash_some(&mut s, host, out),
            Event::IoDone {
                handle,
                status,
                bytes,
            } => {
                let Some(at) = s.pending.iter().position(|p| p.0 == handle) else {
                    return;
                };
                let (_, io) = s.pending.swap_remove(at);
                done(&mut s, host, out, io, status, bytes);
            }
            _ => return,
        }
        out.contribute_commands(&crate::panel::commands(&s.m));
        out.contribute(Kind::Ui, &crate::panel::ui(&s.m));
    });
}

fn issue(s: &mut State, out: &mut Out, handle: Result<u32, Code>, io: Io) -> bool {
    match handle {
        Ok(h) if s.pending.len() < PENDING_MAX => {
            s.pending.push((h, io));
            true
        }
        Ok(_) => {
            say(s, out, Level::Error, "pdk: too many pending requests");
            false
        }
        Err(c) => {
            let msg = format!(
                "pdk: I/O refused ({c}): grant read_project, write_project and the spawn:<binary> of the step (ciel, schemify-interop, git, curl, tar, ssh)"
            );
            say(s, out, Level::Error, &msg);
            false
        }
    }
}

/// Logs `text` and shows it on the panel.
fn say(s: &mut State, out: &mut Out, level: Level, text: &str) {
    out.log_at(level, text);
    s.m.message = text.to_owned();
}

/// Logs `text` and shows it as the remote section's outcome.
fn outcome(s: &mut State, out: &mut Out, level: Level, text: &str) {
    out.log_at(level, text);
    s.m.remote_outcome = text.to_owned();
}

fn argv(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

/// `F V` from command args.
fn family_version(args: &str) -> Result<Fv, String> {
    let (fv, local) = family_version_local(args)?;
    match local {
        None => Ok(fv),
        Some(_) => Err("expected <family> <version>".to_owned()),
    }
}

/// `F V [path]` from `pdk.install` args: a path installs a `local` kit (spec §2.1), any
/// version name; without one a `git`/`url` family installs only its pinned version.
fn install_args(args: &str) -> Result<(Fv, Option<String>), String> {
    let (fv, local) = family_version_local(args)?;
    let r = recipe(&fv.0).expect("validated");
    match (&local, pinned_version(r)) {
        (Some(p), _) => {
            if let Some(why) = local_refusal(r, p) {
                return Err(format!("`{p}`: {why}"));
            }
        }
        (None, Some(pin)) if pin != fv.1 => {
            return Err(format!(
                "{} installs version {pin} (or a kit on disk: pdk.install {} <name> <path>)",
                fv.0, fv.0
            ));
        }
        (None, _) => {}
    }
    Ok((fv, local))
}

fn family_version_local(args: &str) -> Result<(Fv, Option<String>), String> {
    let mut v = text::values(args);
    let f = v
        .need()
        .map_err(|_| "expected <family> <version>".to_owned())?
        .into_owned();
    let ver = v
        .need()
        .map_err(|_| "expected <family> <version>".to_owned())?
        .into_owned();
    if recipe(&f).is_none() {
        let known: Vec<&str> = RECIPES.iter().map(|r| r.family).collect();
        return Err(format!("unknown family `{f}` ({})", known.join(", ")));
    }
    if !valid_version(&ver) {
        return Err(format!("`{ver}` is not a version id (never `latest`)"));
    }
    let local = match v.next() {
        None => None,
        Some(Ok(p)) => Some(p.into_owned()),
        Some(Err(_)) => return Err("bad local path".to_owned()),
    };
    if v.next().is_some() {
        return Err("expected <family> <version> [path]".to_owned());
    }
    Ok(((f, ver), local))
}

fn command(s: &mut State, host: &Host, out: &mut Out, name: &str, args: &str) {
    let row = |prefix: &str| {
        name.strip_prefix(prefix)
            .and_then(|i| i.parse::<usize>().ok())
    };
    let busy = s.m.job.as_ref().is_some_and(|j| j.step != "failed");
    match name {
        "pdk.panel" => s.m.open = !s.m.open,
        "pdk.list_remote" => {
            let f = text::values(args)
                .next()
                .and_then(Result::ok)
                .map(|f| f.into_owned());
            list_remote(s, host, out, f.as_deref().unwrap_or(&s.m.family.clone()));
        }
        _ if name.starts_with("pdk.family.") => {
            let f = name["pdk.family.".len()..].to_owned();
            list_remote(s, host, out, &f);
        }
        "pdk.install" => match install_args(args) {
            Ok((fv, local)) if !busy => install(s, host, out, fv, local.as_deref()),
            Ok(_) => say(s, out, Level::Warn, "pdk.install: an install is running"),
            Err(e) => say(s, out, Level::Error, &format!("pdk.install: {e}")),
        },
        _ if row("pdk.remote.install.").is_some() => {
            let k = row("pdk.remote.install.").expect("checked");
            match s.m.remote.get(k).cloned() {
                Some(v) if !busy => {
                    let f = s.m.family.clone();
                    install(s, host, out, (f, v), None);
                }
                Some(_) => say(s, out, Level::Warn, "pdk.install: an install is running"),
                None => {}
            }
        }
        "pdk.use" | "pdk.remove" => match family_version(args) {
            Ok(fv) => {
                let a = if name == "pdk.use" {
                    Action::Use(fv)
                } else {
                    Action::Remove(fv)
                };
                refresh(s, host, out, Some(a));
            }
            Err(e) => say(s, out, Level::Error, &format!("{name}: {e}")),
        },
        _ if row("pdk.row.use.").is_some() || row("pdk.row.remove.").is_some() => {
            let (i, is_use) = match row("pdk.row.use.") {
                Some(i) => (i, true),
                None => (row("pdk.row.remove.").expect("checked"), false),
            };
            if let Some(fv) = s.m.installs.get(i).cloned() {
                let a = if is_use {
                    Action::Use(fv)
                } else {
                    Action::Remove(fv)
                };
                refresh(s, host, out, Some(a));
            }
        }
        "pdk.prune" => {
            let confirm = args.trim() == "confirm";
            refresh(s, host, out, Some(Action::Prune { confirm }));
        }
        "pdk.prune.confirm" => refresh(s, host, out, Some(Action::Prune { confirm: true })),
        "pdk.export" => {
            let w: Vec<String> = text::values(args)
                .filter_map(Result::ok)
                .map(|v| v.into_owned())
                .collect();
            match &w[..] {
                [f, v, to] if to == "xschem" || to == "virtuoso" => {
                    let to = if to == "xschem" { "xschem" } else { "virtuoso" };
                    refresh(
                        s,
                        host,
                        out,
                        Some(Action::Export((f.clone(), v.clone()), to)),
                    );
                }
                _ => say(
                    s,
                    out,
                    Level::Error,
                    "pdk.export: expected <family> <version> xschem|virtuoso",
                ),
            }
        }
        _ if row("pdk.row.xschem.").is_some() || row("pdk.row.virtuoso.").is_some() => {
            let (i, to) = match row("pdk.row.xschem.") {
                Some(i) => (i, "xschem"),
                None => (row("pdk.row.virtuoso.").expect("checked"), "virtuoso"),
            };
            if let Some(fv) = s.m.installs.get(i).cloned() {
                refresh(s, host, out, Some(Action::Export(fv, to)));
            }
        }
        "pdk.remote.add" => match remote_args(args) {
            Ok(r) => refresh(s, host, out, Some(Action::RemoteAdd(r))),
            Err(e) => outcome(s, out, Level::Error, &format!("pdk.remote.add: {e}")),
        },
        "pdk.remote.trust" | "pdk.remote.refresh" | "pdk.remote.forget" => {
            let f = args.trim().to_owned();
            if !remote::valid_family(&f) {
                let msg = format!("{name}: expected <family> ([a-z][a-z0-9_]*)");
                outcome(s, out, Level::Error, &msg);
                return;
            }
            let a = match name {
                "pdk.remote.trust" => Action::Trust(f),
                "pdk.remote.refresh" => Action::Scan(f),
                _ => Action::Forget(f),
            };
            refresh(s, host, out, Some(a));
        }
        _ if name.starts_with("pdk.remote.row.") => {
            let rest = &name["pdk.remote.row.".len()..];
            let Some((what, i)) = rest.split_once('.') else {
                return;
            };
            let Some(f) = i
                .parse::<usize>()
                .ok()
                .and_then(|i| s.m.remotes.get(i))
                .map(|r| r.family.clone())
            else {
                return;
            };
            let a = match what {
                "trust" => Action::Trust(f),
                "refresh" => Action::Scan(f),
                "forget" => Action::Forget(f),
                _ => return,
            };
            refresh(s, host, out, Some(a));
        }
        _ => {}
    }
}

/// `F <dest> <models> <dialect> [section]` from `pdk.remote.add` args.
fn remote_args(args: &str) -> Result<Remote, String> {
    let w: Vec<String> = text::values(args)
        .map(|v| v.map(|v| v.into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|_| "bad value".to_owned())?;
    let (f, d, m, dl, sec) = match &w[..] {
        [f, d, m, dl] => (f, d, m, dl, None),
        [f, d, m, dl, sec] => (f, d, m, dl, Some(sec.clone())),
        _ => {
            return Err(
                "expected <family> <user@host:/path> <models> <dialect> [section]".to_owned(),
            );
        }
    };
    let r = Remote {
        family: f.clone(),
        dest: d.clone(),
        models: m.clone(),
        dialect: dl.clone(),
        section: sec,
    };
    match remote::remote_refusal(&r) {
        Some(why) => Err(why),
        None => Ok(r),
    }
}

fn refresh(s: &mut State, host: &Host, out: &mut Out, after: Option<Action>) {
    s.after = after;
    for (path, io) in [
        (INDEX, Io::Index),
        ("schemify.project", Io::Manifest),
        ("schemify.lock", Io::Lock),
        (REMOTES, Io::Remotes),
        (TRUSTED, Io::Trusted),
    ] {
        if issue(s, out, host.fs_read(path), io) {
            s.refreshing += 1;
        }
    }
}

fn list_remote(s: &mut State, host: &Host, out: &mut Out, family: &str) {
    let Some(r) = recipe(family) else {
        say(
            s,
            out,
            Level::Error,
            &format!("pdk.list_remote: unknown family `{family}`"),
        );
        return;
    };
    s.m.family = family.to_owned();
    s.m.remote.clear();
    if let Some(pin) = pinned_version(r) {
        s.m.remote.push(pin.to_owned());
        let from = match r.source {
            Source::Git { .. } => "git",
            _ => "url",
        };
        say(
            s,
            out,
            Level::Info,
            &format!("{family}: the pinned {from} version"),
        );
        return;
    }
    say(s, out, Level::Info, &format!("listing {family} versions…"));
    let a = crate::ls_remote_argv(r);
    issue(
        s,
        out,
        host.spawn(&argv(&a), b""),
        Io::ListRemote(family.to_owned()),
    );
}

fn install(s: &mut State, host: &Host, out: &mut Out, fv: Fv, local: Option<&str>) {
    let r = recipe(&fv.0).expect("validated");
    s.steps = install_steps(r, &fv.1, local);
    s.hashing = None;
    s.m.job = Some(Job {
        family: fv.0,
        version: fv.1,
        step: "prepare",
        tail: Vec::new(),
    });
    s.m.message.clear();
    // The status line too (an RPC caller sees no panel); the panel shows the step instead.
    if let Some(j) = &s.m.job {
        out.log(&format!("pdk: installing {} {}…", j.family, j.version));
    }
    run_step(s, host, out, 0, b"");
}

/// Issues install step `i` (the previous step's output `bytes` becomes the panel's tail).
fn run_step(s: &mut State, host: &Host, out: &mut Out, i: usize, bytes: &[u8]) {
    assert!(i < s.steps.len());
    let Some(fv) = job_fv(s) else { return };
    let step = s.steps[i].clone();
    if let Some(j) = &mut s.m.job {
        j.step = step.label();
        if !bytes.is_empty() {
            j.tail = tail(bytes);
        }
    }
    let handle = match &step {
        Step::Remove(path) => host.fs_remove(path),
        Step::Index => {
            let mut rows = s.m.installs.clone();
            rows.push(fv);
            host.fs_write(INDEX, write_index(&rows).as_bytes())
        }
        Step::Spawn(_, a) => host.spawn(&argv(a), b""),
        Step::Verify(path, _) | Step::Derive(path, _) => host.fs_read(path),
        Step::Write(_, path, text) => host.fs_write(path, text.as_bytes()),
    };
    issue(s, out, handle, Io::Step(i));
}

/// Step `i` completed: check it, then run the next one (or fail the install).
fn step_done(s: &mut State, host: &Host, out: &mut Out, i: usize, status: i32, bytes: &[u8]) {
    assert!(i < s.steps.len());
    let gone = Code::Gone.neg();
    let failure = match &s.steps[i] {
        Step::Remove(_) => (status < 0 && status != gone).then(|| "cannot remove".to_owned()),
        Step::Spawn(_, a) => (status != 0).then(|| format!("{} exit {status}", a[0])),
        Step::Verify(path, _) if status < 0 || bytes.len() > ARCHIVE_BYTES_MAX => {
            Some(format!("cannot read {path}"))
        }
        Step::Verify(..) => {
            s.hashing = Some(Hashing {
                step: i,
                data: bytes.to_vec(),
                at: 0,
                sha: Sha256::default(),
            });
            hash_some(s, host, out);
            return;
        }
        Step::Derive(from, to) if status >= 0 => {
            match derive_cmg(&String::from_utf8_lossy(bytes)) {
                Ok(text) => {
                    let h = host.fs_write(to, text.as_bytes());
                    issue(s, out, h, Io::DeriveWrite(i));
                    return;
                }
                Err(e) => Some(format!("{from}: {e}")),
            }
        }
        Step::Derive(from, _) => Some(format!("cannot read {from}")),
        Step::Index | Step::Write(..) => (status < 0).then(|| "cannot write".to_owned()),
    };
    // Only a spawn's bytes are output worth showing (a read's are the file).
    let shown: &[u8] = if matches!(s.steps[i], Step::Spawn(..)) {
        bytes
    } else {
        b""
    };
    match failure {
        Some(what) => fail(s, host, out, &what, shown),
        None => next_step(s, host, out, i, shown),
    }
}

/// Hashes the next [`HASH_CHUNK`] of the download; asks for a timer event to go on, or checks
/// the digest and runs the next step.
fn hash_some(s: &mut State, host: &Host, out: &mut Out) {
    let Some(h) = &mut s.hashing else { return };
    let whole = h.data.len() / 64 * 64;
    let end = (h.at + HASH_CHUNK).min(whole);
    assert!(h.at <= end && HASH_CHUNK.is_multiple_of(64));
    h.sha.blocks(&h.data[h.at..end]);
    h.at = end;
    if end < whole {
        out.timer(HASH_TIMER, 0);
        return;
    }
    let h = s.hashing.take().expect("checked");
    let got = h.sha.finish(&h.data[whole..]);
    let Step::Verify(_, want) = &s.steps[h.step] else {
        unreachable!("hashing runs for a verify step")
    };
    if got == *want {
        next_step(s, host, out, h.step, b"");
    } else {
        let what = format!("sha256 mismatch: got {got}, want {want}");
        fail(s, host, out, &what, b"");
    }
}

fn next_step(s: &mut State, host: &Host, out: &mut Out, i: usize, bytes: &[u8]) {
    if i + 1 < s.steps.len() {
        run_step(s, host, out, i + 1, bytes);
        return;
    }
    let Some((f, v)) = job_fv(s) else { return };
    s.m.job = None;
    s.steps.clear();
    say(
        s,
        out,
        Level::Info,
        &format!("pdk: installed {f} {v} (pdk.use {f} {v} to switch to it)"),
    );
    refresh(s, host, out, None);
}

fn fail(s: &mut State, host: &Host, out: &mut Out, what: &str, bytes: &[u8]) {
    let (step, family, version) = match &mut s.m.job {
        Some(j) => {
            let step = j.step;
            j.step = "failed";
            j.tail = tail(bytes);
            (step, j.family.clone(), j.version.clone())
        }
        None => ("?", String::new(), String::new()),
    };
    let last = tail(bytes).pop().unwrap_or_default();
    let msg = format!("pdk.install {family} {version}: {step} failed ({what}) {last}");
    say(s, out, Level::Error, msg.trim_end());
    refresh(s, host, out, None);
}

fn job_fv(s: &State) -> Option<Fv> {
    s.m.job
        .as_ref()
        .map(|j| (j.family.clone(), j.version.clone()))
}

fn done(s: &mut State, host: &Host, out: &mut Out, io: Io, status: i32, bytes: &[u8]) {
    let ok = status >= 0;
    match io {
        Io::Index | Io::Manifest | Io::Lock | Io::State(_) | Io::Remotes | Io::Trusted => {
            match io {
                Io::Index => {
                    s.m.installs = if ok {
                        parse_index(&String::from_utf8_lossy(bytes))
                    } else {
                        Vec::new()
                    };
                    s.m.complete = vec![None; s.m.installs.len()];
                    for fv in s.m.installs.clone() {
                        let path = format!("{}/STATE", version_dir(&fv.0, &fv.1));
                        if issue(s, out, host.fs_read(&path), Io::State(fv)) {
                            s.refreshing += 1;
                        }
                    }
                }
                Io::Manifest => s.manifest = ok.then(|| bytes.to_vec()),
                Io::Lock => s.lock = ok.then(|| bytes.to_vec()),
                Io::Remotes => {
                    let text = if ok {
                        String::from_utf8_lossy(bytes)
                    } else {
                        "".into()
                    };
                    match remote::parse_remotes(&text) {
                        Ok(rows) => s.m.remotes = rows,
                        Err(e) => {
                            s.m.remotes.clear();
                            outcome(s, out, Level::Error, &e);
                        }
                    }
                }
                Io::Trusted => {
                    let text = if ok {
                        String::from_utf8_lossy(bytes)
                    } else {
                        "".into()
                    };
                    s.m.trusted = remote::parse_trusted(&text);
                }
                Io::State(fv) => {
                    if let Some(i) = s.m.installs.iter().position(|x| *x == fv) {
                        s.m.complete[i] = Some(ok && bytes.trim_ascii() == b"ok");
                    }
                }
                _ => unreachable!("matched above"),
            }
            s.refreshing = s.refreshing.saturating_sub(1);
            if s.refreshing == 0 {
                refreshed(s, host, out);
            }
        }
        Io::ListRemote(f) => {
            if f != s.m.family {
                return;
            }
            if status != 0 {
                let last = tail(bytes).pop().unwrap_or_default();
                say(
                    s,
                    out,
                    Level::Error,
                    &format!("ciel ls-remote {f}: exit {status} {last}"),
                );
                return;
            }
            s.m.remote = parse_versions(&String::from_utf8_lossy(bytes));
            let n = s.m.remote.len();
            say(s, out, Level::Info, &format!("{n} {f} versions"));
        }
        // Install chain.
        Io::Step(i) => step_done(s, host, out, i, status, bytes),
        Io::DeriveWrite(_) if !ok => fail(s, host, out, "cannot write the derived cards", b""),
        Io::DeriveWrite(i) => next_step(s, host, out, i, b""),
        // Use chain.
        Io::UseRoot(fv) => {
            let dir = version_dir(&fv.0, &fv.1);
            let root = match String::from_utf8_lossy(bytes).lines().next() {
                Some(p) if ok && !p.trim().is_empty() => p.trim().to_owned(),
                _ => format!("{dir}/root"),
            };
            let path = format!("{dir}/lib/schemify.project");
            issue(s, out, host.fs_read(&path), Io::UseImport(fv, root));
        }
        Io::UseImport(fv, _) if !ok => {
            let msg = format!(
                "pdk.use {} {}: no import manifest (install it again)",
                fv.0, fv.1
            );
            say(s, out, Level::Error, &msg);
        }
        Io::UseImport(fv, root) => {
            let manifest = s
                .manifest
                .clone()
                .unwrap_or_else(|| EMPTY_MANIFEST.to_vec());
            let r = recipe(&fv.0).expect("validated");
            match use_manifest(&manifest, bytes, r, &fv.1, &root) {
                Ok(text) => {
                    issue(
                        s,
                        out,
                        host.fs_write("schemify.project", text.as_bytes()),
                        Io::UseWrite(fv),
                    );
                }
                Err(e) => say(s, out, Level::Error, &format!("pdk.use: {e}")),
            }
        }
        Io::UseWrite(fv) if !ok => say(
            s,
            out,
            Level::Error,
            &format!("pdk.use {} {}: cannot write schemify.project", fv.0, fv.1),
        ),
        Io::UseWrite(fv) => {
            say(
                s,
                out,
                Level::Info,
                &format!("pdk: using {} {} (project reloaded)", fv.0, fv.1),
            );
            refresh(s, host, out, None);
        }
        // Removal batch.
        Io::Remove(fv) => {
            let gone = status == Code::Gone.neg();
            if ok || gone {
                s.removed.push(fv);
            } else {
                let msg = format!(
                    "pdk.remove {} {}: {}",
                    fv.0,
                    fv.1,
                    String::from_utf8_lossy(bytes)
                );
                say(s, out, Level::Error, &msg);
            }
            s.removing = s.removing.saturating_sub(1);
            if s.removing == 0 {
                let removed = std::mem::take(&mut s.removed);
                let rows: Vec<Fv> =
                    s.m.installs
                        .iter()
                        .filter(|x| !removed.contains(x))
                        .cloned()
                        .collect();
                let list: Vec<String> = removed.iter().map(|(f, v)| format!("{f} {v}")).collect();
                s.m.message = format!("removed {}", list.join(", "));
                out.log(&format!("pdk: {}", s.m.message));
                let text = write_index(&rows);
                issue(
                    s,
                    out,
                    host.fs_write(INDEX, text.as_bytes()),
                    Io::RemoveIndex,
                );
            }
        }
        Io::RemoveIndex => refresh(s, host, out, None),
        Io::Export(fv) => {
            let last = tail(bytes).pop().unwrap_or_default();
            if status == 0 {
                let dir = format!("{}/export", version_dir(&fv.0, &fv.1));
                say(
                    s,
                    out,
                    Level::Info,
                    &format!("pdk: exported {} {} into {dir}", fv.0, fv.1),
                );
            } else {
                let msg = format!("pdk.export {} {}: exit {status} {last}", fv.0, fv.1);
                say(s, out, Level::Error, msg.trim_end());
            }
        }
        Io::RemoteConfig if !ok => say(s, out, Level::Error, "pdk.remote: cannot write"),
        Io::RemoteConfig => refresh(s, host, out, None),
        Io::Probe
        | Io::Agent
        | Io::Script
        | Io::Frames
        | Io::StdinScan
        | Io::CacheClear
        | Io::CacheWrite
        | Io::RemoteUse => scan_done(s, host, out, io, status, bytes),
        Io::Forget(f) if !ok && status != Code::Gone.neg() => {
            outcome(
                s,
                out,
                Level::Error,
                &format!("{f}: cannot remove the cache"),
            );
        }
        Io::Forget(f) => {
            let manifest = s
                .manifest
                .clone()
                .unwrap_or_else(|| EMPTY_MANIFEST.to_vec());
            match forget_manifest(&manifest, &f) {
                Ok(Some(text)) => {
                    let h = host.fs_write("schemify.project", text.as_bytes());
                    issue(s, out, h, Io::ForgetUse(f));
                }
                Ok(None) => {
                    outcome(s, out, Level::Info, &format!("{f}: forgotten"));
                    refresh(s, host, out, None);
                }
                Err(e) => outcome(s, out, Level::Error, &format!("{f}: {e}")),
            }
        }
        Io::ForgetUse(f) if !ok => {
            outcome(
                s,
                out,
                Level::Error,
                &format!("{f}: cannot write schemify.project"),
            );
        }
        Io::ForgetUse(f) => {
            outcome(
                s,
                out,
                Level::Info,
                &format!("{f}: forgotten (cache and manifest rows removed)"),
            );
            refresh(s, host, out, None);
        }
    }
}

/// The record of remote family `f`, or the refusal shown.
fn remote_record(s: &mut State, out: &mut Out, f: &str) -> Option<Remote> {
    let r = s.m.remotes.iter().find(|r| r.family == f).cloned();
    if r.is_none() {
        outcome(
            s,
            out,
            Level::Error,
            &format!("{f}: no such record in {REMOTES} (pdk.remote.add)"),
        );
    }
    r
}

/// Starts `pdk.remote.refresh` of `r`: refused unless its exact destination is trusted.
fn scan_start(s: &mut State, host: &Host, out: &mut Out, r: Remote) {
    if !s.m.trusted.contains(&r.dest) {
        let msg = format!(
            "{}: refused: {} is not trusted in this checkout (Trust, or pdk.remote.trust {})",
            r.family, r.dest, r.family
        );
        outcome(s, out, Level::Error, &msg);
        return;
    }
    if let Some(f) = &s.m.scanning {
        let msg = format!("{}: a scan of {f} is running", r.family);
        outcome(s, out, Level::Warn, &msg);
        return;
    }
    outcome(
        s,
        out,
        Level::Info,
        &format!("{}: scanning {}…", r.family, r.host()),
    );
    let a = remote::probe_argv(&r);
    s.m.scanning = Some(r.family.clone());
    s.scan = Some(Scanning {
        r,
        mode: "agent",
        script: Vec::new(),
        frames: Vec::new(),
        files: 0,
        rounds: 0,
        writes: Vec::new(),
        writing: 0,
        cells: 0,
        issues: 0,
    });
    let h = host.spawn(&argv(&a), b"");
    if !issue(s, out, h, Io::Probe) {
        scan_fail(s, out, "cannot spawn ssh (grant spawn:ssh)");
    }
}

/// Ends the scan with `what` (never scan bytes) as its outcome.
fn scan_fail(s: &mut State, out: &mut Out, what: &str) {
    let f = s.m.scanning.take().unwrap_or_default();
    s.scan = None;
    outcome(s, out, Level::Error, &format!("{f}: {what}"));
}

/// One scan step completed: the next step, or the outcome.
fn scan_done(s: &mut State, host: &Host, out: &mut Out, io: Io, status: i32, bytes: &[u8]) {
    let Some(j) = &mut s.scan else { return };
    let r = j.r.clone();
    let next: Result<(Vec<String>, Vec<u8>, Io), String> = match io {
        // A `pdkscan 1` line: the agent. ssh's own failure (255): its last line, never scan
        // data. Anything else (no agent): the fallback, starting with the local script.
        Io::Probe => match remote::protocol(bytes) {
            Some((b"1", _)) => Ok((remote::agent_argv(&r), Vec::new(), Io::Agent)),
            Some(_) => Err(format!("update schemify-interop on {}", r.host())),
            None if status == 255 => {
                let last = tail(bytes).pop().unwrap_or_default();
                Err(format!("ssh to {} failed: {last}", r.host()))
            }
            None => {
                j.mode = "fallback";
                Ok((remote::script_argv(), Vec::new(), Io::Script))
            }
        },
        Io::Script if status != 0 || bytes.is_empty() => Err(format!(
            "schemify-interop pdk-scan --remote-script: exit {status}"
        )),
        Io::Script => {
            j.script = bytes.to_vec();
            let a = remote::frames_argv(&r, &[r.entry()]);
            Ok((a, j.script.clone(), Io::Frames))
        }
        Io::Frames if status != 0 => Err(format!(
            "reading the kit on {}: ssh exit {status}",
            r.host()
        )),
        Io::Frames => match remote::frame_body(bytes) {
            Ok((body, n)) => {
                j.files += n;
                j.rounds += 1;
                j.frames.extend_from_slice(body);
                if j.files > remote::FILES_MAX || j.frames.len() > ARCHIVE_BYTES_MAX {
                    Err("more than 4096 files or 64 MiB of frames".to_owned())
                } else {
                    let mut stdin = b"sfyframe 1\n".to_vec();
                    stdin.extend_from_slice(&j.frames);
                    stdin.extend_from_slice(b"end\n");
                    Ok((remote::stdin_argv(&r), stdin, Io::StdinScan))
                }
            }
            Err(e) => Err(e),
        },
        Io::Agent | Io::StdinScan => match remote::parse_scan(bytes) {
            Ok(scan) if !scan.needs.is_empty() => {
                if matches!(io, Io::Agent) {
                    Err("pdk-scan: the agent asked for files".to_owned())
                } else if j.rounds >= remote::ROUNDS_MAX {
                    Err(format!("more than {} include rounds", remote::ROUNDS_MAX))
                } else {
                    remote::fetch_paths(r.root(), &scan.needs)
                        .map(|p| (remote::frames_argv(&r, &p), j.script.clone(), Io::Frames))
                }
            }
            Ok(scan) if scan.files.is_empty() => Err("the scan found no cells".to_owned()),
            Ok(scan) => {
                j.cells = scan.files.len();
                j.issues = scan.issues;
                let dir = remote::cache_dir(&r.family);
                j.writes = scan
                    .files
                    .into_iter()
                    .map(|(n, b)| (format!("{dir}/{n}"), b))
                    .collect();
                j.frames = Vec::new();
                let h = host.fs_remove(&dir);
                if !issue(s, out, h, Io::CacheClear) {
                    scan_fail(s, out, "cannot clear the cache");
                }
                return;
            }
            Err(e) if status != 0 => Err(format!("{e} (exit {status})")),
            Err(e) => Err(e),
        },
        Io::CacheClear if status < 0 && status != Code::Gone.neg() => {
            Err("cannot clear the cache".to_owned())
        }
        Io::CacheWrite if status < 0 => Err("cannot write the cache".to_owned()),
        Io::CacheClear | Io::CacheWrite => {
            if matches!(io, Io::CacheWrite) {
                j.writing -= 1;
            }
            pump(s, host, out);
            return;
        }
        Io::RemoteUse if status < 0 => Err("cannot write schemify.project".to_owned()),
        Io::RemoteUse => {
            let issues = match j.issues {
                0 => String::new(),
                n => format!(", {n} issues"),
            };
            let msg = format!(
                "{}: {} cells from {} ({}){issues}",
                r.family,
                j.cells,
                r.host(),
                j.mode
            );
            s.scan = None;
            s.m.scanning = None;
            outcome(s, out, Level::Info, &msg);
            refresh(s, host, out, None);
            return;
        }
        _ => unreachable!("not a scan step"),
    };
    match next {
        Ok((a, stdin, io)) => {
            let h = host.spawn(&argv(&a), &stdin);
            if !issue(s, out, h, io) {
                scan_fail(
                    s,
                    out,
                    "cannot spawn (grant spawn:ssh and spawn:schemify-interop)",
                );
            }
        }
        Err(e) => scan_fail(s, out, &e),
    }
}

/// Issues cache writes up to [`WRITES_AT_ONCE`]; once all landed, rewrites the manifest.
fn pump(s: &mut State, host: &Host, out: &mut Out) {
    loop {
        let Some(j) = &mut s.scan else { return };
        if j.writing >= WRITES_AT_ONCE {
            return;
        }
        let Some((path, bytes)) = j.writes.pop() else {
            break;
        };
        j.writing += 1;
        let h = host.fs_write(&path, &bytes);
        if !issue(s, out, h, Io::CacheWrite) {
            scan_fail(s, out, "cannot write the cache");
            return;
        }
    }
    let Some(j) = &s.scan else { return };
    if j.writing > 0 {
        return;
    }
    let manifest = s
        .manifest
        .clone()
        .unwrap_or_else(|| EMPTY_MANIFEST.to_vec());
    let (family, entry) = (j.r.family.clone(), j.r.entry());
    match remote_manifest(&manifest, &family, &entry) {
        Ok(text) => {
            let h = host.fs_write("schemify.project", text.as_bytes());
            if !issue(s, out, h, Io::RemoteUse) {
                scan_fail(s, out, "cannot write schemify.project");
            }
        }
        Err(e) => scan_fail(s, out, &e),
    }
}

/// A refresh landed: recompute references, then run the action waiting for it.
fn refreshed(s: &mut State, host: &Host, out: &mut Out) {
    let manifest = s
        .manifest
        .clone()
        .unwrap_or_else(|| EMPTY_MANIFEST.to_vec());
    match references(&manifest, s.lock.as_deref()) {
        Ok((active, locked)) => {
            s.m.active = active;
            s.m.locked = locked;
        }
        Err(e) => say(s, out, Level::Error, &format!("pdk: {e}")),
    }
    match s.after.take() {
        None => {}
        Some(Action::Use(fv)) => {
            if !complete(s, &fv) {
                let msg = format!(
                    "pdk.use {} {}: refused: not installed (STATE is not ok)",
                    fv.0, fv.1
                );
                say(s, out, Level::Error, &msg);
                return;
            }
            let path = format!("{}/ROOT", version_dir(&fv.0, &fv.1));
            issue(s, out, host.fs_read(&path), Io::UseRoot(fv));
        }
        Some(Action::Remove(fv)) => match remove_refusal(&fv, &s.m.active, &s.m.locked) {
            Some(why) => say(
                s,
                out,
                Level::Error,
                &format!("pdk.remove {} {}: refused: {why}", fv.0, fv.1),
            ),
            None => remove_batch(s, host, out, vec![fv]),
        },
        Some(Action::Export(fv, to)) => {
            let (f, v) = (&fv.0, &fv.1);
            let refusal = if let Some(r) = s.m.remotes.iter().find(|r| r.family == *f) {
                Some(format!(
                    "a remote family is never exported (its kit stays on {})",
                    r.host()
                ))
            } else if recipe(f).is_none() || !valid_version(v) {
                Some("not a local install".to_owned())
            } else if !complete(s, &fv) {
                Some("not installed (STATE is not ok)".to_owned())
            } else {
                None
            };
            if let Some(why) = refusal {
                say(
                    s,
                    out,
                    Level::Error,
                    &format!("pdk.export {f} {v}: refused: {why}"),
                );
                return;
            }
            let a = export_argv(recipe(f).expect("checked"), v, to);
            say(
                s,
                out,
                Level::Info,
                &format!("pdk: exporting {f} {v} to {to}…"),
            );
            issue(s, out, host.spawn(&argv(&a), b""), Io::Export(fv));
        }
        Some(Action::RemoteAdd(r)) => {
            let mut rows = s.m.remotes.clone();
            match rows.iter().position(|x| x.family == r.family) {
                Some(i) => rows[i] = r.clone(),
                None if rows.len() < remote::REMOTES_MAX => rows.push(r.clone()),
                None => {
                    outcome(s, out, Level::Error, "pdk.remote.add: too many records");
                    return;
                }
            }
            outcome(
                s,
                out,
                Level::Info,
                &format!("{}: added {} (Trust it, then Refresh)", r.family, r.dest),
            );
            let h = host.fs_write(REMOTES, remote::write_remotes(&rows).as_bytes());
            issue(s, out, h, Io::RemoteConfig);
        }
        Some(Action::Trust(f)) => {
            let Some(r) = remote_record(s, out, &f) else {
                return;
            };
            let mut t = s.m.trusted.clone();
            t.push(r.dest.clone());
            outcome(s, out, Level::Info, &format!("{f}: trusted {}", r.dest));
            let h = host.fs_write(TRUSTED, remote::write_trusted(&t).as_bytes());
            issue(s, out, h, Io::RemoteConfig);
        }
        Some(Action::Scan(f)) => {
            if let Some(r) = remote_record(s, out, &f) {
                scan_start(s, host, out, r);
            }
        }
        Some(Action::Forget(f)) => {
            if s.m.scanning.is_some() {
                outcome(s, out, Level::Warn, &format!("{f}: a scan is running"));
                return;
            }
            let h = host.fs_remove(&remote::cache_dir(&f));
            issue(s, out, h, Io::Forget(f));
        }
        Some(Action::Prune { confirm }) => {
            let plan: Vec<Fv> = prune_plan(&s.m.installs, &s.m.active, &s.m.locked)
                .into_iter()
                .map(|i| s.m.installs[i].clone())
                .collect();
            match (confirm, s.m.prune.take()) {
                // Remove only what was listed and is still unreferenced.
                (true, Some(listed)) => {
                    let go: Vec<Fv> = plan.into_iter().filter(|x| listed.contains(x)).collect();
                    if go.is_empty() {
                        say(s, out, Level::Info, "nothing to prune");
                    } else {
                        remove_batch(s, host, out, go);
                    }
                }
                _ => {
                    let list: Vec<String> = plan.iter().map(|(f, v)| format!("{f} {v}")).collect();
                    let msg = if list.is_empty() {
                        "pdk.prune: nothing to remove".to_owned()
                    } else {
                        format!(
                            "pdk.prune: would remove {}; run pdk.prune confirm",
                            list.join(", ")
                        )
                    };
                    say(s, out, Level::Info, &msg);
                    s.m.prune = Some(plan);
                }
            }
        }
    }
}

/// Whether `fv` is an install whose `STATE` reads `ok`.
fn complete(s: &State, fv: &Fv) -> bool {
    let i = s.m.installs.iter().position(|x| x == fv);
    i.and_then(|i| s.m.complete[i]) == Some(true)
}

fn remove_batch(s: &mut State, host: &Host, out: &mut Out, list: Vec<Fv>) {
    assert!(s.removing == 0 && !list.is_empty());
    for fv in list {
        let dir = version_dir(&fv.0, &fv.1);
        if issue(s, out, host.fs_remove(&dir), Io::Remove(fv)) {
            s.removing += 1;
        }
    }
}
