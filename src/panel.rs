//! The docked "PDKs" panel (pdk spec §4) and the plugin's command set, as immediate-mode
//! records over a [`Model`]: the guest re-contributes both whenever the model changes.
//!
//! Styled with the default UI's cell-panel styles (`cell-pane`, `docs-bar`, `docs-title`,
//! `cell-page`, `cell-section`, `cell-block`, `cell-row`, `cell-key`), so it follows the
//! active theme and the Modern preset without styles of its own. Row buttons run row
//! commands (`pdk.row.use.<i>`, `pdk.row.remove.<i>`, `pdk.row.xschem.<i>`,
//! `pdk.row.virtuoso.<i>`, `pdk.remote.install.<k>`, `pdk.family.<family>`,
//! `pdk.remote.row.trust|refresh|forget.<i>`), because a clicked button sends its command
//! without arguments.

use std::fmt::Write as _;

use schemify_abi::text::push_value;

use crate::{Model, RECIPES, Source, remove_refusal};

/// Commands this plugin owns: the public ones (spec §3) and the panel's row commands.
pub fn commands(m: &Model) -> String {
    let mut t = String::new();
    for (name, label) in [
        ("pdk.panel", "PDKs: show or hide the panel"),
        (
            "pdk.list_remote",
            "PDKs: list a family's versions (ciel ls-remote; git/url families: the pinned one)",
        ),
        (
            "pdk.install",
            "PDKs: install <family> <version> [kit path on disk]",
        ),
        ("pdk.use", "PDKs: use <family> <version> in this project"),
        ("pdk.remove", "PDKs: remove <family> <version>"),
        (
            "pdk.prune",
            "PDKs: remove unreferenced versions (then pdk.prune confirm)",
        ),
        ("pdk.prune.confirm", "PDKs: confirm the prune"),
        (
            "pdk.export",
            "PDKs: export <family> <version> xschem|virtuoso (local installs)",
        ),
        (
            "pdk.remote.add",
            "PDKs: add a remote PDK <family> <user@host:/path> <models> <dialect> [section]",
        ),
        ("pdk.remote.trust", "PDKs: trust a remote PDK's destination"),
        (
            "pdk.remote.refresh",
            "PDKs: scan a remote PDK over ssh into the local interface cache",
        ),
        (
            "pdk.remote.forget",
            "PDKs: delete a remote PDK's cache and manifest rows",
        ),
    ] {
        command(&mut t, name, label);
    }
    for r in RECIPES {
        command(&mut t, &format!("pdk.family.{}", r.family), r.family);
    }
    for i in 0..m.installs.len() {
        command(
            &mut t,
            &format!("pdk.row.use.{i}"),
            "PDKs: use this version",
        );
        command(
            &mut t,
            &format!("pdk.row.remove.{i}"),
            "PDKs: remove this version",
        );
        command(
            &mut t,
            &format!("pdk.row.xschem.{i}"),
            "PDKs: export this version to xschem",
        );
        command(
            &mut t,
            &format!("pdk.row.virtuoso.{i}"),
            "PDKs: export this version to Virtuoso",
        );
    }
    for i in 0..m.remotes.len() {
        for (what, label) in [
            ("trust", "PDKs: trust this remote destination"),
            ("refresh", "PDKs: scan this remote PDK"),
            ("forget", "PDKs: forget this remote PDK's cache"),
        ] {
            command(&mut t, &format!("pdk.remote.row.{what}.{i}"), label);
        }
    }
    for k in 0..m.remote.len() {
        command(
            &mut t,
            &format!("pdk.remote.install.{k}"),
            "PDKs: install this version",
        );
    }
    t
}

fn command(t: &mut String, name: &str, label: &str) {
    t.push_str("command ");
    t.push_str(name);
    t.push(' ');
    push_value(t, label);
    t.push('\n');
}

/// One node record: `node <id> <kind>` with `parent` and the given props (values quoted as
/// needed).
fn node(t: &mut String, id: &str, kind: &str, parent: &str, props: &[(&str, &str)]) {
    writeln!(t, "node {id} {kind}\n  parent {parent}").expect("writing to a String");
    for (k, v) in props {
        t.push_str("  ");
        t.push_str(k);
        t.push(' ');
        push_value(t, v);
        t.push('\n');
    }
}

/// A button, greyed with `why` as its tooltip when refused.
fn button(
    t: &mut String,
    id: &str,
    parent: &str,
    label: &str,
    cmd: &str,
    why: Option<&str>,
    help: &str,
) {
    match why {
        Some(why) => node(
            t,
            id,
            "button",
            parent,
            &[
                ("label", label),
                ("command", cmd),
                ("disabled", "1"),
                ("tooltip", why),
            ],
        ),
        None => node(
            t,
            id,
            "button",
            parent,
            &[("label", label), ("command", cmd), ("tooltip", help)],
        ),
    }
}

/// The 7-character short form of a version id.
pub fn short(v: &str) -> &str {
    &v[..v.len().min(7)]
}

/// The state word of install `i` (spec §4).
pub fn state(m: &Model, i: usize) -> &'static str {
    let fv = &m.installs[i];
    if m.active.contains(fv) {
        "active"
    } else if m.locked.contains(fv) {
        "locked"
    } else {
        match m.complete.get(i).copied().flatten() {
            Some(true) => "installed",
            Some(false) => "incomplete",
            None => "…",
        }
    }
}

/// The `ui` set: the View → PDKs item and, while open, the docked panel.
pub fn ui(m: &Model) -> String {
    let mut t = String::new();
    node(
        &mut t,
        "view.pdks",
        "item",
        "menu.view",
        &[
            ("label", "PDKs"),
            ("command", "pdk.panel"),
            ("checked", if m.open { "1" } else { "0" }),
        ],
    );
    if !m.open {
        return t;
    }
    node(
        &mut t,
        "pdk.pane",
        "panel",
        "work",
        &[("style", "cell-pane")],
    );
    node(
        &mut t,
        "pdk.bar",
        "toolbar",
        "pdk.pane",
        &[("style", "docs-bar")],
    );
    node(
        &mut t,
        "pdk.title",
        "label",
        "pdk.bar",
        &[("label", "PDKs"), ("style", "docs-title")],
    );
    node(
        &mut t,
        "pdk.bar.spacer",
        "panel",
        "pdk.bar",
        &[("style", "spacer")],
    );
    node(
        &mut t,
        "pdk.close",
        "button",
        "pdk.bar",
        &[
            ("label", "x"),
            ("command", "pdk.panel"),
            ("tooltip", "Hide the PDKs panel"),
        ],
    );
    node(
        &mut t,
        "pdk.page",
        "scroll",
        "pdk.pane",
        &[("style", "cell-page")],
    );
    // Installed.
    node(
        &mut t,
        "pdk.installed",
        "label",
        "pdk.page",
        &[
            ("label", "Installed in this project"),
            ("style", "cell-section"),
        ],
    );
    if m.installs.is_empty() {
        node(
            &mut t,
            "pdk.installed.none",
            "label",
            "pdk.page",
            &[
                ("label", "No PDK installed yet: pick a family below."),
                ("style", "cell-key"),
            ],
        );
    }
    let busy = m.job.as_ref().is_some_and(|j| j.step != "failed");
    for (i, fv) in m.installs.iter().enumerate() {
        let row = format!("pdk.row.{i}");
        node(&mut t, &row, "panel", "pdk.page", &[("style", "cell-row")]);
        let name = format!("{} {}", fv.0, short(&fv.1));
        node(
            &mut t,
            &format!("{row}.name"),
            "label",
            &row,
            &[("label", &name), ("style", "cell-key"), ("tooltip", &fv.1)],
        );
        let st = state(m, i);
        node(
            &mut t,
            &format!("{row}.state"),
            "label",
            &row,
            &[("label", st)],
        );
        let use_why = match st {
            "active" => Some("already active in schemify.project"),
            "incomplete" => Some("incomplete install: install it again"),
            "…" => Some("reading its state"),
            _ if busy => Some("an install is running"),
            _ => None,
        };
        button(
            &mut t,
            &format!("{row}.use"),
            &row,
            "Use",
            &format!("pdk.row.use.{i}"),
            use_why,
            "Make this version the project's technology",
        );
        let rm_why =
            remove_refusal(fv, &m.active, &m.locked).or(busy.then_some("an install is running"));
        button(
            &mut t,
            &format!("{row}.remove"),
            &row,
            "Remove",
            &format!("pdk.row.remove.{i}"),
            rm_why,
            "Delete this version from .schemify/pdks",
        );
        let ex_why = if m.complete.get(i).copied().flatten() != Some(true) {
            Some("not installed (STATE is not ok)")
        } else {
            None
        };
        for (to, label) in [("xschem", "→ xschem"), ("virtuoso", "→ Virtuoso")] {
            button(
                &mut t,
                &format!("{row}.{to}"),
                &row,
                label,
                &format!("pdk.row.{to}.{i}"),
                ex_why,
                "Export this version's cells (schemify-interop) into its export/ directory",
            );
        }
    }
    // Available.
    node(
        &mut t,
        "pdk.available",
        "label",
        "pdk.page",
        &[("label", "Available"), ("style", "cell-section")],
    );
    node(
        &mut t,
        "pdk.pick",
        "panel",
        "pdk.page",
        &[("style", "cell-row")],
    );
    let family = if m.family.is_empty() {
        "family…"
    } else {
        m.family.as_str()
    };
    node(
        &mut t,
        "pdk.family",
        "dropdown",
        "pdk.pick",
        &[
            ("label", family),
            ("tooltip", "PDK family: lists its versions"),
        ],
    );
    for r in RECIPES {
        node(
            &mut t,
            &format!("pdk.family.{}", r.family),
            "item",
            "pdk.family",
            &[
                ("label", r.family),
                ("command", &format!("pdk.family.{}", r.family)),
            ],
        );
    }
    for (k, v) in m.remote.iter().enumerate() {
        let row = format!("pdk.remote.{k}");
        node(&mut t, &row, "panel", "pdk.page", &[("style", "cell-row")]);
        node(
            &mut t,
            &format!("{row}.name"),
            "label",
            &row,
            &[("label", short(v)), ("style", "cell-key"), ("tooltip", v)],
        );
        let installed = m.installs.iter().any(|fv| fv.0 == m.family && fv.1 == *v);
        let why = busy.then_some("an install is running");
        let label = if installed { "Reinstall" } else { "Install" };
        let help = match crate::recipe(&m.family).map(|r| r.source) {
            Some(Source::Git { .. }) => "git checkout of the pinned rev, then generate its devices",
            Some(Source::Url { .. }) => "download, check its sha256, unpack, generate its devices",
            _ => "ciel enable, then import its symbols",
        };
        button(
            &mut t,
            &format!("{row}.install"),
            &row,
            label,
            &format!("pdk.remote.install.{k}"),
            why,
            help,
        );
    }
    remote_section(&mut t, m);
    // Progress.
    if let Some(j) = &m.job {
        node(
            &mut t,
            "pdk.job",
            "label",
            "pdk.page",
            &[("label", "Install"), ("style", "cell-section")],
        );
        node(
            &mut t,
            "pdk.job.block",
            "panel",
            "pdk.page",
            &[("style", "cell-block")],
        );
        let what = format!("{} {}: {}", j.family, short(&j.version), j.step);
        node(
            &mut t,
            "pdk.job.step",
            "label",
            "pdk.job.block",
            &[("label", &what)],
        );
        for (n, line) in j.tail.iter().enumerate() {
            node(
                &mut t,
                &format!("pdk.job.tail.{n}"),
                "label",
                "pdk.job.block",
                &[("label", line), ("style", "cell-key")],
            );
        }
    }
    // Prune.
    node(
        &mut t,
        "pdk.prune.title",
        "label",
        "pdk.page",
        &[("label", "Clean up"), ("style", "cell-section")],
    );
    node(
        &mut t,
        "pdk.prune.row",
        "panel",
        "pdk.page",
        &[("style", "cell-row")],
    );
    match &m.prune {
        Some(plan) => {
            let list: Vec<String> = plan
                .iter()
                .map(|(f, v)| format!("{f} {}", short(v)))
                .collect();
            let what = if list.is_empty() {
                "Nothing to remove".to_owned()
            } else {
                format!("Removes {}: {}", list.len(), list.join(", "))
            };
            node(
                &mut t,
                "pdk.prune.what",
                "label",
                "pdk.prune.row",
                &[("label", &what), ("style", "cell-key")],
            );
            let why =
                (plan.is_empty() || busy).then_some("nothing to remove, or an install is running");
            button(
                &mut t,
                "pdk.prune.confirm",
                "pdk.prune.row",
                "Confirm",
                "pdk.prune.confirm",
                why,
                "Delete the listed versions",
            );
        }
        None => button(
            &mut t,
            "pdk.prune",
            "pdk.prune.row",
            "Prune…",
            "pdk.prune",
            busy.then_some("an install is running"),
            "List versions neither active nor locked",
        ),
    }
    if !m.message.is_empty() {
        node(
            &mut t,
            "pdk.message",
            "label",
            "pdk.page",
            &[("label", &m.message), ("style", "cell-key")],
        );
    }
    t
}

/// The "Remote" section: each `pdks.remote` record with its exact destination, its trust,
/// and Trust / Refresh / Forget; then the last remote outcome.
fn remote_section(t: &mut String, m: &Model) {
    node(
        t,
        "pdk.ssh",
        "label",
        "pdk.page",
        &[("label", "Remote"), ("style", "cell-section")],
    );
    if m.remotes.is_empty() {
        node(
            t,
            "pdk.ssh.none",
            "label",
            "pdk.page",
            &[
                (
                    "label",
                    "No remote PDK: pdk.remote.add <family> <user@host:/path> <models> <dialect>",
                ),
                ("style", "cell-key"),
            ],
        );
    }
    for (i, r) in m.remotes.iter().enumerate() {
        let row = format!("pdk.ssh.{i}");
        node(t, &row, "panel", "pdk.page", &[("style", "cell-row")]);
        node(
            t,
            &format!("{row}.name"),
            "label",
            &row,
            &[("label", &r.family), ("style", "cell-key")],
        );
        node(
            t,
            &format!("{row}.dest"),
            "label",
            &row,
            &[("label", &r.dest)],
        );
        let trusted = m.trusted.contains(&r.dest);
        let scanning = m.scanning.is_some().then_some("a scan is running");
        button(
            t,
            &format!("{row}.trust"),
            &row,
            "Trust",
            &format!("pdk.remote.row.trust.{i}"),
            trusted.then_some("trusted in this checkout"),
            "Trust exactly this user@host:/path (stored in .schemify/, never committed)",
        );
        button(
            t,
            &format!("{row}.refresh"),
            &row,
            "Refresh",
            &format!("pdk.remote.row.refresh.{i}"),
            (!trusted)
                .then_some("trust the destination first")
                .or(scanning),
            "Scan the kit over ssh: pins, parameter names, formats; no model data",
        );
        button(
            t,
            &format!("{row}.forget"),
            &row,
            "Forget",
            &format!("pdk.remote.row.forget.{i}"),
            scanning,
            "Delete the local interface cache and the manifest rows",
        );
    }
    if !m.remote_outcome.is_empty() {
        node(
            t,
            "pdk.ssh.outcome",
            "label",
            "pdk.page",
            &[("label", &m.remote_outcome), ("style", "cell-key")],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Job;

    fn model() -> Model {
        let fv = |f: &str, v: &str| (f.to_owned(), v.to_owned());
        Model {
            open: true,
            installs: vec![
                fv("sky130", "aaaaaaaaaa"),
                fv("sky130", "bbbbbbbbbb"),
                fv("gf180mcu", "cccc"),
            ],
            complete: vec![Some(true), Some(false), Some(true)],
            active: vec![fv("sky130", "aaaaaaaaaa")],
            locked: vec![fv("gf180mcu", "cccc")],
            family: "sky130".into(),
            remote: vec!["aaaaaaaaaa".into(), "dddddddddd".into()],
            job: None,
            prune: None,
            message: String::new(),
            remotes: vec![crate::remote::Remote {
                family: "tsmc28".into(),
                dest: "pdk@pdkhost:/proj/pdk/tsmc28".into(),
                models: "m.scs".into(),
                dialect: "spectre".into(),
                section: None,
            }],
            trusted: Vec::new(),
            scanning: None,
            remote_outcome: "tsmc28: 3 cells from pdk@pdkhost (agent)".into(),
        }
    }

    #[test]
    fn rows_grey_refused_actions_with_reasons() {
        let t = ui(&model());
        // The records lex as a UI contribution.
        schemify_format::parse(format!("schemify 1 journal\n{t}").as_bytes()).expect("lexes");
        assert!(t.contains("node pdk.row.0.state label\n  parent pdk.row.0\n  label active\n"));
        assert!(t.contains("node pdk.row.0.use button\n  parent pdk.row.0\n  label Use\n  command pdk.row.use.0\n  disabled 1\n  tooltip \"already active in schemify.project\"\n"));
        assert!(t.contains("node pdk.row.0.remove button\n  parent pdk.row.0\n  label Remove\n  command pdk.row.remove.0\n  disabled 1\n"));
        assert!(t.contains("  label incomplete\n"));
        assert!(t.contains("tooltip \"pinned by schemify.lock\""));
        assert!(t.contains(
            "node pdk.remote.0.install button\n  parent pdk.remote.0\n  label Reinstall\n"
        ));
        assert!(t.contains("label Install\n  command pdk.remote.install.1\n  tooltip"));
        let c = commands(&model());
        assert!(
            c.contains("command pdk.row.remove.2 ") && c.contains("command pdk.remote.install.1 ")
        );
        assert!(!c.contains("pdk.row.use.3"));
    }

    #[test]
    fn closed_panel_is_only_the_menu_item_and_busy_greys_installs() {
        let mut m = model();
        m.open = false;
        assert_eq!(ui(&m).matches("node ").count(), 1);
        m.open = true;
        m.job = Some(Job {
            family: "sky130".into(),
            version: "dddddddddd".into(),
            step: "import",
            tail: vec!["fetched".into()],
        });
        let t = ui(&m);
        assert!(t.contains("label \"sky130 ddddddd: import\""));
        assert!(t.contains("label fetched"));
        assert!(t.contains("command pdk.remote.install.1\n  disabled 1\n"));
        m.prune = Some(vec![("sky130".into(), "bbbbbbbbbb".into())]);
        assert!(ui(&m).contains("Removes 1: sky130 bbbbbbb"));
    }

    #[test]
    fn remote_rows_show_the_exact_destination_and_need_trust() {
        let mut m = model();
        let t = ui(&m);
        schemify_format::parse(format!("schemify 1 journal\n{t}").as_bytes()).expect("lexes");
        assert!(t.contains(
            "node pdk.ssh.0.dest label\n  parent pdk.ssh.0\n  label pdk@pdkhost:/proj/pdk/tsmc28\n"
        ));
        assert!(t.contains("command pdk.remote.row.refresh.0\n  disabled 1\n  tooltip \"trust the destination first\"\n"));
        assert!(t.contains("label Trust\n  command pdk.remote.row.trust.0\n  tooltip"));
        assert!(t.contains("label \"tsmc28: 3 cells from pdk@pdkhost (agent)\""));
        m.trusted = vec!["pdk@pdkhost:/proj/pdk/tsmc28".into()];
        let t = ui(&m);
        assert!(t.contains("command pdk.remote.row.trust.0\n  disabled 1\n"));
        assert!(t.contains("label Refresh\n  command pdk.remote.row.refresh.0\n  tooltip"));
        // Every family exports (Interop lists an imported install's library directory).
        assert!(!t.contains("imported cells"));
        m.installs[2] = ("freepdk45".into(), "x".into());
        let t = ui(&m);
        assert!(t.contains("label \"→ Virtuoso\"\n  command pdk.row.virtuoso.2\n  tooltip"));
        let c = commands(&m);
        assert!(
            c.contains("command pdk.remote.row.forget.0 ") && c.contains("command pdk.export ")
        );
        m.remotes.clear();
        assert!(ui(&m).contains("No remote PDK"));
    }
}
