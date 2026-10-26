//! Getting a working tree in, and results back out.
//!
//! Both directions are rsync over the same SSH transport the rest of reaper
//! uses. The two are deliberately not symmetrical, and the asymmetry is the
//! only interesting thing in this module.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::transport::Ssh;

/// The directory results come back on, relative to the synced tree.
///
/// Anchored with a leading slash in the exclude below, so it means the one at
/// the top of the tree and not every `out` anywhere in it. Without that anchor
/// a forward sync would carry `--delete` into the guest's results directory and
/// destroy the run's output on the way past -- the sharpest edge in this
/// design, and the reason this is a constant rather than a literal.
pub const RESULTS: &str = "out";

#[derive(Debug)]
pub enum SyncError {
    Spawn {
        program: String,
        source: io::Error,
    },
    Failed {
        what: String,
        status: i32,
        stderr: String,
    },
    /// The transport wrapper's path is unusable. See [`rsh_wrapper`].
    UnusablePath {
        path: PathBuf,
    },
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Spawn { program, source } => write!(f, "could not run {program}: {source}"),
            SyncError::Failed {
                what,
                status,
                stderr,
            } => {
                write!(f, "{what} failed ({status})")?;
                let trimmed = stderr.trim();
                if !trimmed.is_empty() {
                    write!(f, ": {trimmed}")?;
                }
                Ok(())
            }
            SyncError::UnusablePath { path } => write!(
                f,
                "the transport wrapper would live at {}, and rsync splits its \
                 --rsh option on whitespace with no quoting, so a path containing \
                 any would be torn into pieces. Point XDG_STATE_HOME at a \
                 directory whose path has no spaces in it",
                path.display()
            ),
        }
    }
}

impl std::error::Error for SyncError {}

type Result<T> = std::result::Result<T, SyncError>;

/// An rsync invocation, built but not yet run.
///
/// Separating construction from execution is what lets the suite assert on the
/// arguments -- the flags are the whole substance of this module, and a test
/// that ran rsync to find out what it did would be testing rsync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub program: String,
    pub args: Vec<String>,
}

impl Plan {
    pub fn run(&self) -> Result<String> {
        let out = Command::new(&self.program)
            .args(&self.args)
            .output()
            .map_err(|e| SyncError::Spawn {
                program: self.program.clone(),
                source: e,
            })?;

        if !out.status.success() {
            return Err(SyncError::Failed {
                what: format!("{} {}", self.program, self.args.join(" ")),
                status: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

/// Write the script rsync will use in place of `ssh`, and return its path.
///
/// rsync's `--rsh` takes a command *string*, which it splits on whitespace with
/// no quote handling of its own. Anything with a path in it -- a key, a
/// per-session known-hosts file -- is therefore one space away from being torn
/// into pieces. A script sidesteps the whole question: the options live inside
/// it, quoted by us, and rsync sees a single word.
///
/// It is built from [`Ssh::transport_options`], so ssh and rsync cannot drift
/// into connecting two different ways.
pub fn rsh_wrapper(ssh: &Ssh, at: &Path) -> Result<PathBuf> {
    let text = at.to_string_lossy();
    if text.chars().any(char::is_whitespace) {
        return Err(SyncError::UnusablePath {
            path: at.to_path_buf(),
        });
    }

    let mut script = String::from("#!/bin/sh\n");
    script.push_str("# Written by reaper. rsync splits --rsh on whitespace and does no\n");
    script.push_str("# quoting, so the transport's options live in a script instead.\n");
    script.push_str("exec ");
    script.push_str(&crate::job::quote(ssh.program()));
    for opt in ssh.transport_options() {
        script.push(' ');
        script.push_str(&crate::job::quote(&opt));
    }
    script.push_str(" \"$@\"\n");

    if let Some(dir) = at.parent() {
        std::fs::create_dir_all(dir).map_err(|e| SyncError::Spawn {
            program: text.to_string(),
            source: e,
        })?;
    }
    std::fs::write(at, script).map_err(|e| SyncError::Spawn {
        program: text.to_string(),
        source: e,
    })?;
    make_executable(at)?;
    Ok(at.to_path_buf())
}

fn make_executable(at: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(at)
        .map_err(|e| SyncError::Spawn {
            program: at.to_string_lossy().to_string(),
            source: e,
        })?
        .permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(at, perms).map_err(|e| SyncError::Spawn {
        program: at.to_string_lossy().to_string(),
        source: e,
    })
}

/// The working tree, into the session.
///
/// `--delete` mirrors deletions, because a tree that still holds a file the
/// operator removed is not the tree they are testing. The results directory is
/// excluded, and excluded rather than `--delete-excluded`, so the receiver's
/// copy is protected rather than destroyed.
///
/// `.git` is not excluded by default, deliberately: never needing a commit is
/// the point of this whole channel, version-stamping builds read git metadata,
/// and rsync's deltas make every sync after the first one cheap. A tenant who
/// disagrees says so in `sync.exclude`.
///
/// No `-z`. This is a local network, and compressing an already-compressed
/// artifact costs more than it saves.
pub fn push(
    rsync: &str,
    rsh: &Path,
    ssh: &Ssh,
    local: &Path,
    remote: &str,
    exclude: &[String],
) -> Plan {
    let mut args = vec![
        "-a".to_string(),
        "--delete".to_string(),
        // Itemized so that [`deletions`] can count what went. A removal that
        // silently fails to propagate leaves the guest compiling a file the
        // operator deleted, and a baseline built that way is green for the
        // wrong reason -- the one failure this channel has that looks like
        // success. `-i` rather than `--info=DEL`, which is newer than some of
        // the rsync builds this runs against; the extra lines are captured and
        // counted here, never shown.
        "--itemize-changes".to_string(),
        // rsync's own inactivity timeout, and the only thing that bounds the
        // failure this channel actually produces. Twice now a transfer has
        // wedged with both ends idle in select and nothing moving, while the
        // connection stayed ESTABLISHED and a second ssh to the same guest
        // answered instantly -- so nothing at the connection layer, keepalives
        // included, was ever going to notice. This does: no data for this long
        // and rsync gives up and says so.
        format!("--timeout={}", ssh.io_timeout().as_secs().max(1)),
    ];
    args.extend(ownership());
    args.push(format!("--exclude=/{RESULTS}/"));
    for pattern in exclude {
        args.push(format!("--exclude={pattern}"));
    }
    args.push("-e".into());
    args.push(rsh.to_string_lossy().to_string());
    args.push(dir(local));
    args.push(format!("{}:{}", ssh.rsync_host(), with_slash(remote)));
    Plan {
        program: rsync.to_string(),
        args,
    }
}

/// A linked working tree, as `git worktree add` makes one.
///
/// In such a tree `.git` is not a directory but a one-line file pointing at a
/// directory inside the main checkout's repository -- which holds what makes
/// this checkout *this* checkout (HEAD, the index, its reflogs) -- and that
/// directory in turn names the repository the two checkouts share (objects,
/// refs, config). Shipped as a file, the pointer names a path that does not
/// exist on the guest, and every git invocation there fails with "not a git
/// repository" -- one failing oracle at a time, in a battery that otherwise
/// runs normally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// This checkout's own metadata: HEAD, index, logs.
    pub private: PathBuf,
    /// The repository it shares: objects, refs, config.
    pub common: PathBuf,
}

/// Whether `tree` is a linked working tree, and of what.
///
/// `Ok(None)` for an ordinary checkout or no repository at all; both are
/// synced as they are. An error only when `.git` is a pointer that cannot be
/// followed -- a worktree whose main checkout has been moved or removed --
/// because shipping it would cost a full battery to diagnose, and nothing
/// here can make such a tree whole.
pub fn worktree(tree: &Path) -> io::Result<Option<Worktree>> {
    let dotgit = tree.join(".git");
    match std::fs::symlink_metadata(&dotgit) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    let text = std::fs::read_to_string(&dotgit)?;
    let Some(target) = text.lines().find_map(|l| l.strip_prefix("gitdir:")) else {
        return Err(io::Error::other(format!(
            "{} is a file, but not the gitdir pointer a linked working tree \
             carries; git itself will not know what to make of it",
            dotgit.display()
        )));
    };
    let private = resolve(tree, target.trim());
    if !private.is_dir() {
        return Err(io::Error::other(format!(
            "{} points at {}, which is not here. This is a linked working tree \
             (git worktree) whose main checkout has been moved or removed, and \
             the guest could do nothing with it either; `git worktree repair` \
             from the main checkout is the cure",
            dotgit.display(),
            private.display()
        )));
    }
    let commondir = private.join("commondir");
    let common_text = std::fs::read_to_string(&commondir).map_err(|e| {
        io::Error::other(format!(
            "{} names no shared repository ({}): {e}",
            private.display(),
            commondir.display()
        ))
    })?;
    let common = resolve(&private, common_text.trim());
    if !common.is_dir() {
        return Err(io::Error::other(format!(
            "{} says the shared repository is at {}, which is not here",
            commondir.display(),
            common.display()
        )));
    }
    Ok(Some(Worktree { private, common }))
}

/// A path from a git pointer file, against the directory the file lives in.
fn resolve(base: &Path, target: &str) -> PathBuf {
    let raw = Path::new(target);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    // Cleaned up where that is possible, so the message a person reads and
    // the source rsync is handed are the same path; left as joined when it
    // does not exist, which is exactly when the message matters.
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

/// Whether the tenant has asked for `.git` to stay home.
///
/// Matched on the three spellings that mean the top-level directory and
/// nothing else; a pattern like `*.git` is some other file's business.
fn git_excluded(exclude: &[String]) -> bool {
    exclude
        .iter()
        .any(|e| matches!(e.as_str(), ".git" | "/.git" | "/.git/" | ".git/"))
}

/// The forward syncs a tree needs: one for an ordinary checkout, three for a
/// linked working tree.
///
/// For a worktree the pointer file stays home and a real repository is built
/// at `<remote>/.git` from the two halves it points at: the shared repository
/// first, mirrored (its `worktrees/` registry left out and removed if an
/// earlier sync put it there, since it names paths on this machine), then this checkout's own metadata laid over the top
/// without `--delete`, so that HEAD and the index are this branch's while the
/// objects and refs are the repository's. The two files in it that point back
/// here, `gitdir` and `commondir`, stay home too: on the guest the directory
/// IS the repository, and git reads them as a claim that it is not.
///
/// Both repository halves go by `--checksum`, and the real-git arm in the
/// suite is why. rsync's quick check takes equal size and equal mtime as
/// "same file", and a repository is full of files that defeat it: a branch
/// ref is 41 bytes whatever it names, and the shared half has just put the
/// main checkout's index where this checkout's index is about to go -- same
/// tracked set, same size. Written within a second of each other, both were
/// skipped, and the guest was left on a stale commit with the other
/// checkout's index, `git status` reporting every file modified twice over.
/// A guest quietly building the wrong commit is the failure this channel
/// exists to prevent, so the repository is compared by content; it costs a
/// read of the repository on both sides per sync, and only for worktrees.
///
/// A tenant who excludes `.git` gets exactly the one sync they asked for,
/// worktree or not.
pub fn push_plans(
    rsync: &str,
    rsh: &Path,
    ssh: &Ssh,
    local: &Path,
    remote: &str,
    exclude: &[String],
) -> io::Result<(Vec<Plan>, Option<Worktree>)> {
    if git_excluded(exclude) {
        return Ok((vec![push(rsync, rsh, ssh, local, remote, exclude)], None));
    }
    let Some(wt) = worktree(local)? else {
        return Ok((vec![push(rsync, rsh, ssh, local, remote, exclude)], None));
    };

    let mut tree_exclude = vec!["/.git".to_string()];
    tree_exclude.extend(exclude.iter().cloned());
    let git_remote = format!("{}/.git", remote.trim_end_matches('/'));
    let mut common = push(
        rsync,
        rsh,
        ssh,
        &wt.common,
        &git_remote,
        &["/worktrees/".to_string()],
    );
    common.args.insert(1, "--checksum".to_string());
    // Excluded means protected on the receiver everywhere else in this
    // module, and here that is the wrong rule: a guest that was synced from
    // the main checkout before this tree became the source still holds that
    // checkout's `worktrees/` registry, naming paths on this machine, and a
    // protected exclude would leave it there for ever. Found on a live guest.
    common.args.insert(2, "--delete-excluded".to_string());
    let plans = vec![
        push(rsync, rsh, ssh, local, remote, &tree_exclude),
        common,
        overlay(
            rsync,
            rsh,
            ssh,
            &wt.private,
            &git_remote,
            &["/gitdir".to_string(), "/commondir".to_string()],
        ),
    ];
    Ok((plans, Some(wt)))
}

/// Into the session, adding to what is there: the shape of [`pull`] pointed
/// the other way, and used only for the second half of a worktree's
/// repository.
fn overlay(
    rsync: &str,
    rsh: &Path,
    ssh: &Ssh,
    local: &Path,
    remote: &str,
    exclude: &[String],
) -> Plan {
    let mut args = vec![
        "-a".to_string(),
        // Content decides, not size and mtime; see `push_plans`.
        "--checksum".to_string(),
        format!("--timeout={}", ssh.io_timeout().as_secs().max(1)),
    ];
    args.extend(ownership());
    for pattern in exclude {
        args.push(format!("--exclude={pattern}"));
    }
    args.push("-e".into());
    args.push(rsh.to_string_lossy().to_string());
    args.push(dir(local));
    args.push(format!("{}:{}", ssh.rsync_host(), with_slash(remote)));
    Plan {
        program: rsync.to_string(),
        args,
    }
}

/// How many paths a forward sync removed from the receiver.
///
/// Read from rsync's own itemized output rather than inferred here, because
/// the question this answers is precisely "did `--delete` reach that path?" --
/// and a count reaper computed from what it *believed* it had deleted would
/// answer a different question, the one that is never in doubt.
///
/// rsync writes one `*deleting   <path>` line per removal. Directories and
/// files both count: a removed directory is a removed path.
pub fn deletions(output: &str) -> usize {
    output
        .lines()
        .filter(|l| l.trim_start().starts_with("*deleting"))
        .count()
}

/// Results, back out.
///
/// **No `--delete`**, and this is a decision rather than an omission. The guest
/// is authoritative for what it produced; it is not authoritative for what was
/// in the operator's results directory beforehand. Destroying a local artifact
/// to mirror a guest that never had it is the wrong trade for a channel whose
/// entire purpose is that a failure trace must never exist only on a machine
/// scheduled for destruction.
pub fn pull(rsync: &str, rsh: &Path, ssh: &Ssh, remote: &str, local: &Path) -> Plan {
    // Bounded for the same reason as the forward direction, and more urgently:
    // this is the one that runs on a timer while a tenant's command is going,
    // so a pull that never returns holds the whole verb open after the run it
    // was reporting on has already passed.
    let mut args = vec![
        "-a".to_string(),
        format!("--timeout={}", ssh.io_timeout().as_secs().max(1)),
    ];
    args.extend(ownership());
    args.extend([
        "-e".to_string(),
        rsh.to_string_lossy().to_string(),
        format!("{}:{}", ssh.rsync_host(), with_slash(remote)),
        dir(local),
    ]);
    Plan {
        program: rsync.to_string(),
        args,
    }
}

/// Empty the workstation's results directory, and say how many entries went.
///
/// The counterpart to [`pull`]'s refusal to delete, and the reason that refusal
/// is safe to keep. Because the backward sync merges, a run that fails *before
/// the guest produces anything* leaves the previous run's results sitting in
/// `out/` -- and a collector globbing `out/*.xml`, or a person, then reads a
/// complete green battery for a tier that never executed. The exit status is
/// right and the artifacts contradict it, which is the worst shape a result can
/// have.
///
/// So this runs once at the top of a whole loop, before anything is synced: the
/// operator asking for another run is what supersedes the last one's output.
/// Nothing is destroyed to mirror the guest, which is the trade `pull`
/// documents and declines; the failure trace from the run just finished
/// survives until the next run is deliberately started.
///
/// The directory itself is kept, because it is the rsync destination and
/// recreating it is one more thing to get wrong. A directory that is not there
/// is not an error: there is nothing to clear.
pub fn clear_results(tree: &Path) -> io::Result<usize> {
    let at = tree.join(RESULTS);
    let entries = match std::fs::read_dir(&at) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };

    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        // Never followed. A symlink in here is removed as a link; whatever it
        // points at is somebody else's file and is none of this function's
        // business.
        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(entry.path())?;
        } else {
            std::fs::remove_file(entry.path())?;
        }
        removed += 1;
    }
    Ok(removed)
}

/// Numeric owners are not carried across.
///
/// `-a` preserves them, and the two machines share no user database, so a tree
/// synced from a workstation lands owned by a uid the guest has never heard of.
/// Everything in a session runs as root, and the practical result of the
/// default is that tools which check who owns what -- git's own repository
/// ownership check is the one that bit -- refuse to work on the synced tree.
fn ownership() -> [String; 2] {
    ["--no-owner".to_string(), "--no-group".to_string()]
}

/// A trailing slash on a source means "the contents of", and on a destination
/// it costs nothing. Getting it wrong on the source nests the tree inside
/// itself one directory deeper on every sync.
fn dir(p: &Path) -> String {
    with_slash(&p.to_string_lossy())
}

fn with_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}/")
    }
}
