//! `reaper auto`: watch a forge, and judge what shows up.
//!
//! One review request at a time, oldest first. Fetch its head into the clone,
//! make sure a session exists, run the battery, and then either land the
//! request or say on it why not. Then do it again.
//!
//! Three things in here are not obvious and are the reason this is a verb
//! rather than a shell loop.
//!
//! **The battery is a child process.** `reaper test` could be called as a
//! function -- it is in the same binary -- but then a commit landing mid-run
//! could not interrupt it without a cancellation token threaded through the
//! transport, the sync and the runner. A child can simply be killed. The cost
//! is one process spawn per battery, which is nothing next to the battery.
//!
//! **An interrupted battery leaves the session unusable.** A sync cut off part
//! way through leaves a half-written tree in the guest, and the next run fails
//! for reasons that have nothing to do with the change under test. So a killed
//! battery is always followed by destroying the session and making a new one,
//! never by retrying on the same machine.
//!
//! **The verdict is re-checked against the head before it is acted on.** A
//! commit that lands after the battery finishes but before the merge would
//! otherwise mean landing a commit that was never tested. The head is read
//! again immediately before merging, and a change means the verdict is thrown
//! away rather than applied.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reaper_core::forge::{already_judged, Forge, Request, VERDICT_PREFIX};

use crate::commands::{Error, Result};

/// How much of a battery's output a verdict comment carries.
///
/// Forges cap a comment's length, and the cap is not generous next to a
/// battery log. The tail is what a reader wants anyway -- a failure announces
/// itself at the end -- and the whole log stays in the session's results.
const COMMENT_LINES: usize = 120;
const COMMENT_BYTES: usize = 48 * 1024;

/// How much of a battery's output is kept in memory.
///
/// A long battery can print a great deal; keeping all of it to quote 120 lines
/// of it would be a leak that only shows up on the longest runs.
const KEEP_LINES: usize = 4000;

pub struct Options {
    /// Which adapter, overriding what the remote implies.
    pub forge: Option<String>,
    /// Which git remote names the repository to watch.
    pub remote: String,
    /// How often to look for work, and to re-read the head during a battery.
    pub poll: Duration,
    /// One pass, then stop. For a cron entry, and for tests.
    pub once: bool,
    /// Say what would happen; change nothing on the forge.
    pub dry_run: bool,
    pub profile: Option<String>,
}

pub fn auto(path: Option<PathBuf>, opts: Options) -> Result<()> {
    let dir = match path {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let dir = dir.canonicalize().map_err(|e| -> Error {
        format!("{}: {e}", dir.display()).into()
    })?;
    if !dir.join(".git").exists() {
        return Err(format!("{} is not a git clone", dir.display()).into());
    }

    let exe = std::env::current_exe()
        .map_err(|e| -> Error { format!("cannot find my own binary: {e}").into() })?;
    let cfg = reaper_core::config::load()?;

    let remote_url = git(&dir, &["remote", "get-url", &opts.remote])?;
    let remote_url = remote_url.trim().to_string();
    let name = match &opts.forge {
        Some(n) => n.clone(),
        None => reaper_forges::detect(&remote_url).to_string(),
    };
    let forge = reaper_forges::build(&name, &remote_url, cfg.forge_table(&name))?;

    say!(
        "auto: watching {remote_url} as {name}, every {}",
        reaper_core::duration::format_rough(opts.poll)
    );
    if opts.dry_run {
        say!("auto: dry run -- nothing will be merged and no comment will be posted");
    }

    loop {
        match pass(&dir, &exe, forge.as_ref(), &opts)? {
            Pass::Acted | Pass::Superseded => {
                // Straight back round: a request that was superseded has a new
                // head to fetch, and one that was acted on may have a sibling
                // waiting. Sleeping here would be sleeping on known work.
            }
            Pass::Idle => {
                if opts.once {
                    return Ok(());
                }
                std::thread::sleep(opts.poll);
            }
        }
        if opts.once {
            return Ok(());
        }
    }
}

enum Pass {
    /// A request was judged: merged, or commented on.
    Acted,
    /// A commit landed while we were working. Nothing was decided.
    Superseded,
    /// Nothing to do.
    Idle,
}

fn pass(dir: &Path, exe: &Path, forge: &dyn Forge, opts: &Options) -> Result<Pass> {
    let requests = forge.open_requests()?;
    if requests.is_empty() {
        say!("auto: nothing open");
        return Ok(Pass::Idle);
    }

    let mut chosen = None;
    for r in &requests {
        let said = forge.own_comments(r.number)?;
        if already_judged(&r.head, &said) {
            continue;
        }
        chosen = Some(r.clone());
        break;
    }
    let Some(req) = chosen else {
        say!(
            "auto: {} open, all of them already judged at their current head",
            requests.len()
        );
        return Ok(Pass::Idle);
    };

    say!(
        "auto: request {} {:?} at {}",
        req.number,
        req.title,
        short(&req.head)
    );

    // The tree is the tenant's, and sync copies it wholesale. Checking out
    // over uncommitted work would destroy it, so refuse rather than stash:
    // a stash this process made is a stash nobody is expecting to find.
    let dirty = git(dir, &["status", "--porcelain"])?;
    if !dirty.trim().is_empty() {
        return Err(format!(
            "{} has uncommitted changes, and auto has to check out request {} over them.\n\
             Commit, discard or stash them first:\n{}",
            dir.display(),
            req.number,
            dirty.trim()
        )
        .into());
    }

    git(dir, &["fetch", "--quiet", "--force", &opts.remote, &req.fetch_ref])?;
    git(dir, &["checkout", "--quiet", "--detach", "FETCH_HEAD"])?;
    let at = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    if at != req.head {
        // The forge said one thing and the ref gave another: a commit landed
        // between the listing and the fetch. Go round rather than test a
        // commit we are about to mis-attribute.
        say!("auto: head moved while fetching ({} -> {})", short(&req.head), short(&at));
        return Ok(Pass::Superseded);
    }
    say!("auto: checked out {} detached", short(&at));

    ensure_session(exe, dir, opts)?;

    match battery(exe, dir, opts, forge, &req)? {
        Battery::Superseded => {
            say!("auto: a commit landed mid-battery; stopping and starting again");
            // An interrupted battery leaves a half-synced tree in the guest.
            // The next run on that machine would fail for reasons unrelated to
            // the change, so the machine goes.
            let _ = reaper(exe, dir, &["down"]);
            Ok(Pass::Superseded)
        }
        Battery::Green { log } => {
            let now = forge.head_of(req.number)?;
            if now != req.head {
                say!(
                    "auto: green, but the head moved to {} after the battery -- not merging",
                    short(&now)
                );
                return Ok(Pass::Superseded);
            }
            if opts.dry_run {
                say!("auto: would rebase-and-merge request {}", req.number);
                return Ok(Pass::Acted);
            }
            match forge.rebase_merge(req.number) {
                Ok(()) => {
                    say!("auto: request {} merged", req.number);
                    Ok(Pass::Acted)
                }
                Err(e) => {
                    // Say so on the request, not just here. Without a comment
                    // the loop would pick this request up again next pass and
                    // keep failing the same way for as long as it is open.
                    say!("auto: request {} is green but would not merge: {e}", req.number);
                    let body = refused_comment(&req, &e.to_string(), &log);
                    forge.comment(req.number, &body)?;
                    Ok(Pass::Acted)
                }
            }
        }
        Battery::Red { log, status } => {
            if opts.dry_run {
                say!(
                    "auto: would comment on request {} ({})",
                    req.number,
                    describe(status)
                );
                return Ok(Pass::Acted);
            }
            let body = red_comment(&req, status, &log);
            forge.comment(req.number, &body)?;
            say!("auto: request {} is red; said so", req.number);
            Ok(Pass::Acted)
        }
    }
}

/// Make sure a session exists, allowing for one that has gone stale.
///
/// `up` is idempotent against a live session and says so. It fails when the
/// record names a machine that is no longer there, which is the ordinary state
/// after a sweep or a node restart -- and the fix is always the same, so it is
/// done here rather than left for a person to do by hand at the point the
/// overnight loop stops.
fn ensure_session(exe: &Path, dir: &Path, opts: &Options) -> Result<()> {
    // The profile carries the time-to-live and the environment, so an `up`
    // without it would make a session the battery then runs differently in.
    let mut args: Vec<&str> = vec!["up"];
    if let Some(p) = &opts.profile {
        args.push("--profile");
        args.push(p);
    }
    if reaper(exe, dir, &args)?.success() {
        return Ok(());
    }
    say!("auto: up failed; clearing whatever is left and trying once more");
    // Deliberately not --all: another project's session is not ours to take,
    // and reaper refuses a record whose machine the pool has reassigned.
    let _ = reaper(exe, dir, &["down"]);
    if reaper(exe, dir, &args)?.success() {
        return Ok(());
    }
    Err(format!(
        "cannot bring up a session in {} even after clearing the old one",
        dir.display()
    )
    .into())
}

enum Battery {
    Green { log: Vec<String> },
    Red { log: Vec<String>, status: ExitStatus },
    Superseded,
}

/// Run the battery, watching the request's head while it runs.
fn battery(
    exe: &Path,
    dir: &Path,
    opts: &Options,
    forge: &dyn Forge,
    req: &Request,
) -> Result<Battery> {
    let mut args: Vec<&str> = vec!["test"];
    if let Some(p) = &opts.profile {
        args.push("--profile");
        args.push(p);
    }
    say!("auto: battery starting");

    let mut child = Command::new(exe)
        .args(&args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| -> Error { format!("cannot start the battery: {e}").into() })?;

    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut readers = Vec::new();
    for pipe in [
        child.stdout.take().map(Pipe::Out),
        child.stderr.take().map(Pipe::Err),
    ]
    .into_iter()
    .flatten()
    {
        let sink = Arc::clone(&log);
        readers.push(std::thread::spawn(move || drain(pipe, &sink)));
    }

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(opts.poll.min(Duration::from_secs(10)));
        if let Some(status) = child.try_wait()? {
            break status;
        }
        match forge.head_of(req.number) {
            Ok(now) if now != req.head => {
                kill(&mut child);
                for r in readers {
                    let _ = r.join();
                }
                return Ok(Battery::Superseded);
            }
            Ok(_) => {}
            // A forge that cannot be reached mid-battery is not a reason to
            // throw away a battery that is still running. Say it once and keep
            // watching: if the forge is still unreachable when the verdict is
            // due, that is where it will fail, with the verdict in hand.
            Err(e) => say!("auto: cannot re-read the head just now: {e}"),
        }
    };

    for r in readers {
        let _ = r.join();
    }
    let log = log.lock().expect("log lock").clone();
    if status.success() {
        say!("auto: battery green");
        Ok(Battery::Green { log })
    } else {
        say!("auto: battery red ({})", describe(status));
        Ok(Battery::Red { log, status })
    }
}

enum Pipe {
    Out(std::process::ChildStdout),
    Err(std::process::ChildStderr),
}

/// Echo the battery's output as it arrives, and keep a bounded tail of it.
///
/// Echoed because an operator watching `auto` wants to see the battery, not a
/// silent gap of forty minutes; bounded because the tail is all a comment can
/// carry anyway.
fn drain(pipe: Pipe, sink: &Mutex<Vec<String>>) {
    let reader: Box<dyn Read + Send> = match pipe {
        Pipe::Out(p) => Box::new(p),
        Pipe::Err(p) => Box::new(p),
    };
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        say!("{line}");
        let mut held = sink.lock().expect("log lock");
        if held.len() == KEEP_LINES {
            held.remove(0);
        }
        held.push(line);
    }
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn describe(status: ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("exit {c}"),
        None => "killed by a signal".to_string(),
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// The tail of a log, fenced, and under the forge's size limit.
fn tail(log: &[String]) -> String {
    let start = log.len().saturating_sub(COMMENT_LINES);
    let dropped = start;
    let mut body = String::new();
    if dropped > 0 {
        body.push_str(&format!(
            "_The first {dropped} line(s) are omitted; the whole log is in the session's results._\n\n"
        ));
    }
    body.push_str("```\n");
    for line in &log[start..] {
        if body.len() + line.len() + 8 > COMMENT_BYTES {
            body.push_str("... truncated\n");
            break;
        }
        body.push_str(line);
        body.push('\n');
    }
    body.push_str("```\n");
    body
}

fn red_comment(req: &Request, status: ExitStatus, log: &[String]) -> String {
    format!(
        "reaper ran its battery against this request and it failed ({}).\n\n{}\n{VERDICT_PREFIX}{}\n",
        describe(status),
        tail(log),
        req.head
    )
}

fn refused_comment(req: &Request, why: &str, log: &[String]) -> String {
    format!(
        "reaper's battery passed, but the forge would not rebase-and-merge this \
         request:\n\n```\n{why}\n```\n\nThe battery's output, for the record:\n\n{}\n\
         {VERDICT_PREFIX}{}\n",
        tail(log),
        req.head
    )
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    run("git", dir, args)
}

fn run(program: &str, dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| -> Error { format!("cannot run {program}: {e}").into() })?;
    if !out.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run one of our own verbs, inheriting the terminal.
fn reaper(exe: &Path, dir: &Path, args: &[&str]) -> Result<ExitStatus> {
    Command::new(exe)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| -> Error { format!("cannot run my own {}: {e}", args[0]).into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Request {
        Request {
            number: 7,
            title: "a change".into(),
            head: "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c".into(),
            fetch_ref: "refs/pull/7/head".into(),
        }
    }

    fn lines(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn a_red_comment_carries_the_marker_for_the_commit_it_judged() {
        // Without the marker the loop cannot tell this request has been
        // answered, and will retest the same commit for as long as it is open.
        let body = red_comment(&req(), fake_status(), &lines(3));
        assert!(
            reaper_core::forge::already_judged(&req().head, &[body.clone()]),
            "{body}"
        );
    }

    #[test]
    fn a_refusal_comment_also_carries_the_marker() {
        // The green-but-unmergeable path is the one most easily left out, and
        // leaving it out is an infinite loop rather than a missing nicety.
        let body = refused_comment(&req(), "merge conflict", &lines(3));
        assert!(reaper_core::forge::already_judged(&req().head, &[body.clone()]), "{body}");
        assert!(body.contains("merge conflict"), "{body}");
    }

    #[test]
    fn a_long_log_is_cut_to_its_tail_and_says_so() {
        let body = tail(&lines(500));
        assert!(body.contains("line 499"), "the tail must be the end");
        assert!(!body.contains("line 0\n"), "the head must be dropped");
        assert!(body.contains("are omitted"), "and must say that it was: {body}");
    }

    #[test]
    fn a_short_log_is_quoted_whole_with_no_omission_notice() {
        let body = tail(&lines(3));
        assert!(body.contains("line 0") && body.contains("line 2"), "{body}");
        assert!(!body.contains("omitted"), "{body}");
    }

    #[test]
    fn a_comment_stays_under_the_size_a_forge_will_accept() {
        // One enormous line, which the line cap alone does not bound.
        let huge = vec!["x".repeat(200_000)];
        let body = red_comment(&req(), fake_status(), &huge);
        assert!(body.len() <= COMMENT_BYTES + 2048, "{} bytes", body.len());
        assert!(body.contains("truncated"), "and must say so");
    }

    #[test]
    fn an_abbreviated_sha_is_twelve_characters_and_never_panics() {
        assert_eq!(short("0f1e2d3c4b5a69788796"), "0f1e2d3c4b5a");
        // A short or empty head comes from a forge we did not write; slicing
        // blindly would panic inside the loop rather than report anything.
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
    }

    fn fake_status() -> ExitStatus {
        // A real failing status, so `describe` is exercised rather than mocked.
        Command::new("sh")
            .args(["-c", "exit 3"])
            .status()
            .expect("sh should run")
    }

    #[test]
    fn a_failing_status_is_described_by_its_code() {
        assert_eq!(describe(fake_status()), "exit 3");
    }
}
