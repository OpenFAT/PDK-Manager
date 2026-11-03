//! Remote PDKs over SSH (docs/remote-pdks.md §4.4, §4.5), the pure parts: the `pdks.remote`
//! records and their grammar, the trusted destinations, the exact argv of every scan step,
//! and the readers of `pdk-scan` output and of the fallback script's frames.
//!
//! The kit never leaves its host. Scan output and frames stay in guest memory; only the cells
//! a scan returns (pins, parameter names, formats: no model data) are written, to
//! [`cache_dir`], until the core can serve a plugin library (design C1).

use schemify_abi::text;

/// The committed remote configuration, at the project root: one [`Remote`] per line.
pub const REMOTES: &str = "pdks.remote";
/// The destinations trusted in this checkout, one per line (under the git-ignored
/// `.schemify/`, never committed).
pub const TRUSTED: &str = ".schemify/pdk-manager/trusted";
/// Where the scanned cells of each remote family are cached.
pub const CACHE: &str = ".schemify/pdk-manager/remote";
/// Most records `pdks.remote` holds.
pub const REMOTES_MAX: usize = 64;
/// Most bytes of a destination or a models path.
pub const DEST_MAX: usize = 1024;
/// Most fetch rounds of the fallback (the scanner's include depth).
pub const ROUNDS_MAX: u32 = 16;
/// Most files the fallback fetches, and most cells a scan returns (the scanner's cap).
pub const FILES_MAX: usize = 4096;
/// Bytes of quoted paths one fetch round passes (the host caps an argv at 8 KiB).
pub const ARGV_PATHS_MAX: usize = 6 << 10;
/// The model dialects `pdk-scan` reads.
pub const DIALECTS: [&str; 3] = ["spice", "hspice", "spectre"];

/// One `remote <family> <dest> models <relpath> dialect <d> [section <s>]` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    /// Library name: `[a-z][a-z0-9_]{0,31}`, never a built-in family.
    pub family: String,
    /// `[user@]host:/path` ([`dest_refusal`]).
    pub dest: String,
    /// The corner library, relative to the destination's path.
    pub models: String,
    /// One of [`DIALECTS`].
    pub dialect: String,
    /// The models file's section, if it has sections.
    pub section: Option<String>,
}

impl Remote {
    /// `[user@]host`: the ssh destination.
    pub fn host(&self) -> &str {
        self.dest.split_once(':').expect("validated").0
    }

    /// The kit's absolute path on the host.
    pub fn root(&self) -> &str {
        self.dest.split_once(':').expect("validated").1
    }

    /// The corner library's absolute path on the host.
    pub fn entry(&self) -> String {
        format!("{}/{}", self.root(), self.models)
    }
}

/// `.schemify/pdk-manager/remote/<family>`.
pub fn cache_dir(family: &str) -> String {
    assert!(valid_family(family));
    format!("{CACHE}/{family}")
}

/// A path segment: `[A-Za-z0-9._+-]+`, never `.` or `..`, never starting with `-`.
fn seg_ok(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// Segments joined by `/`, at most [`DEST_MAX`] bytes.
fn rel_ok(p: &str) -> bool {
    p.len() <= DEST_MAX && p.split('/').all(seg_ok)
}

fn abs_ok(p: &str) -> bool {
    p.strip_prefix('/').is_some_and(rel_ok)
}

/// Whether `f` may name a remote family: `[a-z][a-z0-9_]{0,31}`.
pub fn valid_family(f: &str) -> bool {
    let b = f.as_bytes();
    (1..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// Why `dest` is not `[user@]host:/path` (design §4.4), or `None`: user
/// `[a-z_][a-z0-9._-]{0,31}`, host `[A-Za-z0-9][A-Za-z0-9.-]{0,252}` (a name or an ssh alias,
/// never an IPv6 literal), an absolute path of segments, at most [`DEST_MAX`] bytes. Nothing
/// can start with `-`, and no shell metacharacter or space passes.
pub fn dest_refusal(dest: &str) -> Option<&'static str> {
    let (login, path) = dest.split_once(':').unwrap_or((dest, ""));
    let (user, host) = match login.split_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, login),
    };
    let user_ok = |u: &str| {
        let b = u.as_bytes();
        (1..=32).contains(&b.len())
            && (b[0].is_ascii_lowercase() || b[0] == b'_')
            && b.iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
    };
    let h = host.as_bytes();
    let host_ok = (1..=253).contains(&h.len())
        && h[0].is_ascii_alphanumeric()
        && h.iter()
            .all(|c| c.is_ascii_alphanumeric() || b".-".contains(c));
    if dest.len() > DEST_MAX {
        Some("a destination is at most 1024 bytes")
    } else if user.is_some_and(|u| !user_ok(u)) {
        Some("the user is [a-z_][a-z0-9._-]{0,31}")
    } else if !host_ok {
        Some("the host is a name or an ~/.ssh/config alias ([A-Za-z0-9][A-Za-z0-9.-]*)")
    } else if !abs_ok(path) {
        Some("the path is absolute, of [A-Za-z0-9._+-] segments, never . or ..")
    } else {
        None
    }
}

/// Why `r` cannot be a record, or `None` (the grammar of every field).
pub fn remote_refusal(r: &Remote) -> Option<String> {
    let why = if !valid_family(&r.family) {
        "a remote family is [a-z][a-z0-9_]{0,31}".to_owned()
    } else if crate::recipe(&r.family).is_some() {
        format!("`{}` is a built-in family", r.family)
    } else if let Some(why) = dest_refusal(&r.dest) {
        format!("`{}`: {why}", r.dest)
    } else if !rel_ok(&r.models) {
        format!(
            "models `{}`: a relative path of [A-Za-z0-9._+-] segments",
            r.models
        )
    } else if !DIALECTS.contains(&r.dialect.as_str()) {
        format!("dialect `{}`: spice, hspice or spectre", r.dialect)
    } else if r
        .section
        .as_ref()
        .is_some_and(|s| s.len() > 64 || !seg_ok(s))
    {
        "a section is a [A-Za-z0-9._+-] word".to_owned()
    } else {
        return None;
    };
    Some(why)
}

/// Parses `pdks.remote` (blank lines and `#` comments skipped). Strict: the file is
/// committed, so one bad record refuses the whole file.
///
/// # Errors
///
/// A line that is not a valid record, a family listed twice, more than [`REMOTES_MAX`].
pub fn parse_remotes(text: &str) -> Result<Vec<Remote>, String> {
    let mut rows: Vec<Remote> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let w: Vec<&str> = line.split_whitespace().collect();
        if w.is_empty() || w[0].starts_with('#') {
            continue;
        }
        let at = |why: String| format!("{REMOTES} line {}: {why}", n + 1);
        let section = match w[..] {
            ["remote", _, _, "models", _, "dialect", _] => None,
            ["remote", _, _, "models", _, "dialect", _, "section", s] => Some(s.to_owned()),
            _ => {
                return Err(at(
                    "expected remote <family> <dest> models <path> dialect <d> [section <s>]"
                        .to_owned(),
                ));
            }
        };
        let r = Remote {
            family: w[1].to_owned(),
            dest: w[2].to_owned(),
            models: w[4].to_owned(),
            dialect: w[6].to_owned(),
            section,
        };
        if let Some(why) = remote_refusal(&r) {
            return Err(at(why));
        }
        if rows.iter().any(|x| x.family == r.family) {
            return Err(at(format!("`{}` is listed twice", r.family)));
        }
        if rows.len() >= REMOTES_MAX {
            return Err(at(format!("more than {REMOTES_MAX} records")));
        }
        rows.push(r);
    }
    Ok(rows)
}

/// Writes `pdks.remote`: a comment naming the record, then one record per family, in order.
pub fn write_remotes(rows: &[Remote]) -> String {
    let mut t = "# pdk-manager: remote <family> <[user@]host:/path> models <path> dialect <spice|hspice|spectre> [section <s>]\n".to_owned();
    for r in rows {
        assert!(remote_refusal(r).is_none());
        t.push_str(&format!(
            "remote {} {} models {} dialect {}",
            r.family, r.dest, r.models, r.dialect
        ));
        if let Some(s) = &r.section {
            t.push_str(&format!(" section {s}"));
        }
        t.push('\n');
    }
    t
}

/// The trusted destinations, once each (lines that are not destinations are skipped).
pub fn parse_trusted(text: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for d in text.lines().map(str::trim).take(REMOTES_MAX * 4) {
        if dest_refusal(d).is_none() && !v.iter().any(|x| x == d) {
            v.push(d.to_owned());
        }
    }
    v
}

/// Writes the trusted list, once each, in order.
pub fn write_trusted(dests: &[String]) -> String {
    let mut t = String::new();
    for (i, d) in dests.iter().enumerate() {
        if !dests[..i].contains(d) {
            t.push_str(d);
            t.push('\n');
        }
    }
    t
}

/// The constant ssh options (design §4.4); nothing from the project is ever added.
const SSH: [&str; 20] = [
    "ssh",
    "-T",
    "-x",
    "-a",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=2",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "StrictHostKeyChecking=yes",
];

/// `'w'`: a grammar-checked word for the remote shell.
fn quote(w: &str) -> String {
    assert!(!w.contains('\''));
    format!("'{w}'")
}

/// `ssh <constant options> -- <user@host> <words>`; the words reach the remote shell.
fn ssh_argv(r: &Remote, words: Vec<String>) -> Vec<String> {
    assert!(remote_refusal(r).is_none());
    let mut a: Vec<String> = SSH.map(str::to_owned).to_vec();
    a.push("--".to_owned());
    a.push(r.host().to_owned());
    a.extend(words);
    a
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|w| (*w).to_owned()).collect()
}

/// The agent probe: `ssh … -- <host> schemify-interop pdk-scan --protocol`.
pub fn probe_argv(r: &Remote) -> Vec<String> {
    ssh_argv(r, words(&["schemify-interop", "pdk-scan", "--protocol"]))
}

/// Agent mode: `ssh … -- <host> schemify-interop pdk-scan --root '<path>' --entry '<models>'
/// --dialect '<d>' [--section '<s>'] --lib '<family>'`.
pub fn agent_argv(r: &Remote) -> Vec<String> {
    let mut w = words(&["schemify-interop", "pdk-scan", "--root"]);
    w.extend([quote(r.root()), "--entry".into(), quote(&r.models)]);
    w.extend(["--dialect".into(), quote(&r.dialect)]);
    if let Some(s) = &r.section {
        w.extend(["--section".into(), quote(s)]);
    }
    w.extend(["--lib".into(), quote(&r.family)]);
    ssh_argv(r, w)
}

/// A fallback round: `ssh … -- <host> sh -s -- '<path>'…`, the script on stdin.
pub fn frames_argv(r: &Remote, paths: &[String]) -> Vec<String> {
    assert!(!paths.is_empty() && paths.iter().all(|p| abs_ok(p)));
    let mut w = words(&["sh", "-s", "--"]);
    w.extend(paths.iter().map(|p| quote(p)));
    ssh_argv(r, w)
}

/// The local interop's remote script: `schemify-interop pdk-scan --remote-script`.
pub fn script_argv() -> Vec<String> {
    words(&["schemify-interop", "pdk-scan", "--remote-script"])
}

/// The local scan of the frames on stdin: `schemify-interop pdk-scan --stdin --dialect <d>
/// --entry <abs path> [--section <s>] --lib <family>`.
pub fn stdin_argv(r: &Remote) -> Vec<String> {
    let mut a = words(&["schemify-interop", "pdk-scan", "--stdin", "--dialect"]);
    a.extend([r.dialect.clone(), "--entry".into(), r.entry()]);
    if let Some(s) = &r.section {
        a.extend(["--section".into(), s.clone()]);
    }
    a.extend(["--lib".into(), r.family.clone()]);
    a
}

/// Splits the next `\n`-terminated line off `rest`.
fn line<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let at = rest.iter().position(|&b| b == b'\n')?;
    let l = &rest[..at];
    *rest = &rest[at + 1..];
    Some(l)
}

/// The version word of the first `pdkscan <v>` line, and what follows that line. Output
/// before it (a login banner) is skipped; stderr comes after stdout.
pub fn protocol(out: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut rest = out;
    loop {
        let l = line(&mut rest)?;
        if let Some(v) = l.strip_prefix(b"pdkscan ") {
            return Some((v, rest));
        }
    }
}

/// One `pdk-scan` result (protocol 1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The kit's version, 16 hex digits.
    pub version: String,
    /// Cell files: (name `<cell>.shprim`, bytes).
    pub files: Vec<(String, Vec<u8>)>,
    /// How many `issue` lines it printed (their text is scan data, never shown).
    pub issues: usize,
    /// The files a `--stdin` scan still needs (absolute paths on the host).
    pub needs: Vec<String>,
}

/// Reads `pdk-scan` output: `pdkscan 1`, `version <16 hex>`, `file <name> <len>` + exactly
/// `len` bytes, `issue <text>`, `need <path>`, `end`; or `error "<text>"`.
///
/// # Errors
///
/// No or another protocol, the scan's own `error`, a truncated or malformed record, a cell
/// name with `/`, `..` or not `<cell>.shprim`, a repeated name, more than [`FILES_MAX`]
/// files. Messages never quote scan bytes beyond the scan's own error text.
pub fn parse_scan(out: &[u8]) -> Result<Scan, String> {
    let mut rest = match protocol(out) {
        Some((b"1", rest)) => rest,
        Some(_) => return Err("another pdk-scan protocol: update schemify-interop".to_owned()),
        None => return Err("no pdk-scan output".to_owned()),
    };
    let mut s = Scan::default();
    loop {
        let l = line(&mut rest).ok_or("truncated pdk-scan output")?;
        let l = std::str::from_utf8(l).map_err(|_| "pdk-scan: a record is not UTF-8")?;
        let (word, arg) = l.split_once(' ').unwrap_or((l, ""));
        match word {
            "" => {}
            "end" => break,
            "version" if s.version.is_empty() && arg.len() == 16 => {
                if !arg.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("pdk-scan: a bad version".to_owned());
                }
                s.version = arg.to_owned();
            }
            "file" => {
                let (name, len) = arg.rsplit_once(' ').ok_or("pdk-scan: a bad file record")?;
                let len: usize = len.parse().map_err(|_| "pdk-scan: a bad file length")?;
                let cell = name.strip_suffix(".shprim").unwrap_or("");
                if cell.is_empty() || name.contains("..") || !seg_ok(name) {
                    return Err("pdk-scan: a cell name that is not <cell>.shprim".to_owned());
                }
                if len > rest.len() {
                    return Err("truncated pdk-scan output".to_owned());
                }
                if s.files.len() >= FILES_MAX || s.files.iter().any(|f| f.0 == name) {
                    return Err("pdk-scan: too many or repeated cells".to_owned());
                }
                s.files.push((name.to_owned(), rest[..len].to_vec()));
                rest = &rest[len..];
            }
            "issue" => s.issues += 1,
            "need" if s.needs.len() < FILES_MAX => s.needs.push(arg.to_owned()),
            "error" => {
                let t = text::values(arg)
                    .next()
                    .and_then(Result::ok)
                    .unwrap_or_default();
                let t: String = t.chars().take(160).collect();
                return Err(format!("pdk-scan: {t}"));
            }
            _ => return Err("pdk-scan: an unknown record".to_owned()),
        }
    }
    if s.version.is_empty() {
        return Err("pdk-scan: no version".to_owned());
    }
    Ok(s)
}

/// The records between the remote script's `sfyframe 1` header and its `end` trailer
/// (`f <len> <path>` + `len` bytes, `f 0 ! <path>`), and how many files they frame. Rounds
/// concatenate their bodies and wrap them once.
///
/// # Errors
///
/// No header, a malformed or truncated frame, no trailer.
pub fn frame_body(out: &[u8]) -> Result<(&[u8], usize), String> {
    let mut rest = out;
    loop {
        if line(&mut rest).ok_or("the remote script printed no frames")? == b"sfyframe 1" {
            break;
        }
    }
    let body = rest;
    let mut n = 0;
    loop {
        let start = body.len() - rest.len();
        let l = line(&mut rest).ok_or("truncated frames")?;
        if l == b"end" {
            return Ok((&body[..start], n));
        }
        if l.is_empty() {
            continue;
        }
        let f = std::str::from_utf8(l)
            .ok()
            .and_then(|l| l.strip_prefix("f "))
            .and_then(|f| f.split_once(' '))
            .ok_or("a malformed frame")?;
        let len: usize = f.0.parse().map_err(|_| "a malformed frame")?;
        if f.1.starts_with("! ") && len != 0 || len > rest.len() {
            return Err("a malformed frame".to_owned());
        }
        rest = &rest[len..];
        n += 1;
    }
}

/// The paths of `needs` the next fallback round fetches: as many as fit in
/// [`ARGV_PATHS_MAX`] (the rest are asked for again by the next scan).
///
/// # Errors
///
/// A path that is not under `root` or not of grammar segments.
pub fn fetch_paths(root: &str, needs: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let mut bytes = 0;
    for p in needs {
        let under = p.strip_prefix(root).is_some_and(|r| r.starts_with('/'));
        if !under || !abs_ok(p) {
            return Err("pdk-scan asked for a file outside the kit".to_owned());
        }
        if !out.is_empty() && bytes + p.len() + 3 > ARGV_PATHS_MAX {
            break;
        }
        bytes += p.len() + 3;
        out.push(p.clone());
    }
    assert!(out.len() <= needs.len());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tsmc() -> Remote {
        Remote {
            family: "tsmc28".into(),
            dest: "pdk@pdkhost:/proj/pdk/tsmc28/1.3".into(),
            models: "models/spectre/toplevel.scs".into(),
            dialect: "spectre".into(),
            section: Some("tt".into()),
        }
    }

    #[test]
    fn destinations_follow_the_grammar() {
        for ok in [
            "pdk@pdkhost:/proj/pdk/tsmc28/1.3",
            "pdkhost:/p",
            "_svc.x-1@host.example.com:/a/b+c/d_e-f.g",
            "Alias9:/x",
        ] {
            assert_eq!(dest_refusal(ok), None, "{ok}");
        }
        let long_host = format!("{}:/p", "h".repeat(254));
        let long = format!("h:/{}", "a".repeat(DEST_MAX));
        for bad in [
            "",
            "host",
            "host:",
            "host:relative/path",
            "host:/",
            "host:/a//b",
            "host:/a/",
            "host:/a/../b",
            "host:/a/./b",
            "host:/..",
            "-oProxyCommand=evil:/p",
            "host:/-oProxyCommand=evil",
            "-host:/p",
            "@host:/p",
            "-u@host:/p",
            "User@host:/p",
            "u@h@x:/p",
            "[::1]:/p",
            "::1:/p",
            "fe80::1:/p",
            "host:/a b",
            "host :/a",
            "'host':/p",
            "\"host\":/p",
            "host:/a'b",
            "host:/a;rm",
            "host:/a|b",
            "host:/a&b",
            "host:/$HOME",
            "host:/`id`",
            "host:/a\\b",
            "host:/a*",
            "host:/a?",
            "host:/a\nb",
            "host;id:/p",
            "ho$t:/p",
            &long_host,
            &long,
        ] {
            assert!(dest_refusal(bad).is_some(), "{bad:?}");
        }
    }

    #[test]
    fn records_parse_validate_and_round_trip() {
        let text = "# team kits\n\nremote tsmc28 pdk@pdkhost:/proj/pdk/tsmc28/1.3 models models/spectre/toplevel.scs dialect spectre section tt\nremote gf12 lab:/kits/gf12 models m.lib dialect hspice\n";
        let rows = parse_remotes(text).unwrap();
        assert_eq!(rows[0], tsmc());
        assert_eq!(rows[1].section, None);
        assert_eq!(rows[0].host(), "pdk@pdkhost");
        assert_eq!(rows[0].root(), "/proj/pdk/tsmc28/1.3");
        assert_eq!(
            rows[0].entry(),
            "/proj/pdk/tsmc28/1.3/models/spectre/toplevel.scs"
        );
        let written = write_remotes(&rows);
        assert_eq!(parse_remotes(&written).unwrap(), rows);
        assert!(
            written.lines().nth(1)
                == Some(
                    "remote tsmc28 pdk@pdkhost:/proj/pdk/tsmc28/1.3 models models/spectre/toplevel.scs dialect spectre section tt"
                )
        );
        let line = |rest: &str| format!("remote {rest}\n");
        for bad in [
            line("sky130 h:/p models m dialect spice"),
            line("Tsmc h:/p models m dialect spice"),
            line("9x h:/p models m dialect spice"),
            line(&format!("{} h:/p models m dialect spice", "a".repeat(33))),
            line("x -oProxyCommand=y:/p models m dialect spice"),
            line("x h:/p models ../m dialect spice"),
            line("x h:/p models /abs dialect spice"),
            line("x h:/p models m dialect eldo"),
            line("x h:/p models m dialect spice section a;b"),
            line("x h:/p models m dialect spice section"),
            line("x h:/p model m dialect spice"),
            line("x \"h:/p\" models m dialect spice"),
            "lib x path y\n".to_owned(),
            line("x h:/p models m dialect spice") + &line("x h:/q models m dialect spice"),
        ] {
            assert!(parse_remotes(&bad).is_err(), "{bad}");
        }
        let many: String = (0..=REMOTES_MAX)
            .map(|i| line(&format!("f{i} h:/p models m dialect spice")))
            .collect();
        assert!(parse_remotes(&many).is_err());
        assert_eq!(
            parse_trusted("b:/x\njunk line\n a@b:/y \nb:/x\n"),
            ["b:/x", "a@b:/y"]
        );
        assert_eq!(
            write_trusted(&["b:/x".into(), "a:/y".into(), "b:/x".into()]),
            "b:/x\na:/y\n"
        );
    }

    #[test]
    fn ssh_argv_is_exactly_the_design() {
        let r = tsmc();
        let head = "ssh -T -x -a -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=2 -o ForwardAgent=no -o ClearAllForwardings=yes -o PermitLocalCommand=no -o StrictHostKeyChecking=yes -- pdk@pdkhost";
        assert_eq!(
            probe_argv(&r).join(" "),
            format!("{head} schemify-interop pdk-scan --protocol")
        );
        assert_eq!(
            agent_argv(&r).join(" "),
            format!(
                "{head} schemify-interop pdk-scan --root '/proj/pdk/tsmc28/1.3' --entry 'models/spectre/toplevel.scs' --dialect 'spectre' --section 'tt' --lib 'tsmc28'"
            )
        );
        assert_eq!(
            frames_argv(&r, &["/proj/a".into(), "/proj/b c".replace(' ', "_")]).join(" "),
            format!("{head} sh -s -- '/proj/a' '/proj/b_c'")
        );
        assert_eq!(
            stdin_argv(&r),
            [
                "schemify-interop",
                "pdk-scan",
                "--stdin",
                "--dialect",
                "spectre",
                "--entry",
                "/proj/pdk/tsmc28/1.3/models/spectre/toplevel.scs",
                "--section",
                "tt",
                "--lib",
                "tsmc28"
            ]
        );
        assert_eq!(
            script_argv().join(" "),
            "schemify-interop pdk-scan --remote-script"
        );
        // The destination is its own argv word after `--`; no option ever follows it.
        let a = agent_argv(&r);
        let at = a.iter().position(|w| w == "--").unwrap();
        assert_eq!(at, SSH.len());
        assert!(a[at + 2..].iter().all(|w| !w.starts_with("-o")));
    }

    #[test]
    fn scans_parse_and_adversarial_output_is_refused() {
        let cell = b"schemify 1 shprim\n";
        let mut out =
            b"motd banner\npdkscan 1\nversion 0123456789abcdef\nfile nch.shprim 18\n".to_vec();
        out.extend_from_slice(cell);
        out.extend_from_slice(b"file pch.shprim 18\n");
        out.extend_from_slice(cell);
        out.extend_from_slice(b"issue bins skipped\nend\nssh: warning on stderr\n");
        let s = parse_scan(&out).unwrap();
        assert_eq!(s.version, "0123456789abcdef");
        assert_eq!(s.files.len(), 2);
        assert_eq!(s.files[1], ("pch.shprim".into(), cell.to_vec()));
        assert_eq!((s.issues, s.needs.len()), (1, 0));
        let needs =
            parse_scan(b"pdkscan 1\nversion 0123456789abcdef\nneed /k/a.inc\nneed /k/b.inc\nend\n")
                .unwrap();
        assert_eq!(needs.needs, ["/k/a.inc", "/k/b.inc"]);
        assert_eq!(
            parse_scan(b"pdkscan 1\nerror \"cannot read /k/m.lib\"\nend\n").unwrap_err(),
            "pdk-scan: cannot read /k/m.lib"
        );
        let v = "pdkscan 1\nversion 0123456789abcdef\n";
        for bad in [
            "".to_owned(),
            "hello\n".to_owned(),
            "pdkscan 2\nend\n".to_owned(),
            "pdkscan 1\nend\n".to_owned(),
            format!("{v}file nch.shprim 18\nshort"),
            format!("{v}file nch.shprim 5\nabcde"),
            format!("{v}file nch.shprim 99999999999999999999999\nx\nend\n"),
            format!("{v}file nch.shprim -1\nend\n"),
            format!("{v}file ../x.shprim 1\nx\nend\n"),
            format!("{v}file a/x.shprim 1\nx\nend\n"),
            format!("{v}file ...shprim 1\nx\nend\n"),
            format!("{v}file .shprim 1\nx\nend\n"),
            format!("{v}file x.shcomp 1\nx\nend\n"),
            format!("{v}file x\\y.shprim 1\nx\nend\n"),
            format!("{v}file x.shprim\nend\n"),
            format!("{v}file a.shprim 1\nxfile a.shprim 1\nx\nend\n"),
            format!("{v}version 0123456789abcdef\nend\n"),
            "pdkscan 1\nversion 0123\nend\n".to_owned(),
            "pdkscan 1\nversion 0123456789abcdeg\nend\n".to_owned(),
            format!("{v}bogus\nend\n"),
        ] {
            assert!(parse_scan(bad.as_bytes()).is_err(), "{bad:?}");
        }
        let mut huge = v.to_owned();
        for i in 0..=FILES_MAX {
            huge.push_str(&format!("file c{i}.shprim 0\n"));
        }
        huge.push_str("end\n");
        assert!(parse_scan(huge.as_bytes()).is_err());
    }

    #[test]
    fn frames_strip_to_their_body() {
        let out =
            b"banner\nsfyframe 1\nf 3 /k/a\nabcf 0 ! /k/b\nf 4 /k/c\nend\n\nend\nssh: stderr\n";
        let (body, n) = frame_body(out).unwrap();
        assert_eq!(body, b"f 3 /k/a\nabcf 0 ! /k/b\nf 4 /k/c\nend\n\n");
        assert_eq!(n, 3);
        assert_eq!(frame_body(b"sfyframe 1\nend\n").unwrap(), (&b""[..], 0));
        for bad in [
            &b"f 1 /x\nx\nend\n"[..],
            b"sfyframe 1\nf 9 /x\nab",
            b"sfyframe 1\nf 1 /x\nx",
            b"sfyframe 1\nf 2 ! /x\nend\n",
            b"sfyframe 1\ng 1 /x\nx\nend\n",
            b"sfyframe 1\nf x /x\nend\n",
        ] {
            assert!(frame_body(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn fetches_stay_under_the_root_and_fit_the_argv() {
        let root = "/proj/kit";
        let ok = vec!["/proj/kit/a.inc".to_owned(), "/proj/kit/m/b.inc".to_owned()];
        assert_eq!(fetch_paths(root, &ok).unwrap(), ok);
        for bad in [
            "/proj/kitx/a",
            "/proj/kit",
            "/etc/passwd",
            "/proj/kit/../x",
            "/proj/kit/a b",
            "relative",
            "/proj/kit/'x",
        ] {
            assert!(fetch_paths(root, &[bad.to_owned()]).is_err(), "{bad}");
        }
        let many: Vec<String> = (0..400)
            .map(|i| format!("{root}/{}{i}.inc", "d".repeat(40)))
            .collect();
        let first = fetch_paths(root, &many).unwrap();
        assert!(!first.is_empty() && first.len() < many.len());
        let quoted: usize = first.iter().map(|p| p.len() + 3).sum();
        assert!(quoted <= ARGV_PATHS_MAX);
        assert!(frames_argv(&tsmc(), &first).join(" ").len() < 8 << 10);
    }
}
