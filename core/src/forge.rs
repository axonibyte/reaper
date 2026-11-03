//! The forge seam.
//!
//! A forge hosts repositories and the review requests raised against them.
//! Everything below is deliberately free of any particular forge's vocabulary
//! -- no API paths, no vendor names, no authentication scheme, no merge-button
//! semantics. Those are an adapter's business, and a lint guard fails the
//! build if they appear outside one.
//!
//! The seam is narrow on purpose. `reaper auto` needs to answer five
//! questions and no more: what is open, what is at the tip of it, what have I
//! already said about it, how do I say something, and how do I land it. A
//! sixth question is the beginning of reaper knowing what a forge is.

use std::fmt;

/// A review request, as the forge that hosts it describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub number: u64,
    pub title: String,
    /// The commit at the tip of the request's branch, in full.
    ///
    /// What a verdict is recorded against, and what is watched for change --
    /// but NOT what gets tested. See `fetch_ref`.
    pub head: String,
    /// A refspec the local clone can fetch to obtain the tree to test.
    ///
    /// This is the forge's MERGE commit, not the request's head: the question
    /// a battery is being asked is "would landing this break the base
    /// branch", and the head alone cannot answer it. A request can pass
    /// perfectly on its own branch and break the base the moment the two are
    /// combined, with no textual conflict anywhere for the forge to refuse --
    /// and a verb that merges unattended on the strength of that is a verb
    /// that lands breakage.
    ///
    /// It has a second use. Both forges publish this ref only while the
    /// request actually merges, so its absence is a conflict, detectable
    /// before a machine is built rather than after a battery.
    ///
    /// Supplied by the adapter rather than assembled here: the convention is
    /// a forge's, not a fact about git.
    pub fetch_ref: String,
}

#[derive(Debug)]
pub enum ForgeError {
    /// Configuration says something that cannot be acted on, or says nothing
    /// where something was needed.
    Config(String),
    /// The forge could not be reached, or did not answer in a shape we know.
    Transport(String),
    /// The forge was reached, understood the request, and declined it.
    Refused { what: String, why: String },
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeError::Config(m) => write!(f, "{m}"),
            ForgeError::Transport(m) => write!(f, "{m}"),
            ForgeError::Refused { what, why } => write!(f, "{what} refused: {why}"),
        }
    }
}

impl std::error::Error for ForgeError {}

pub trait Forge {
    /// Open requests, oldest first.
    ///
    /// Oldest first because `auto` takes one at a time and a newest-first
    /// order starves an older request that keeps losing the race.
    fn open_requests(&self) -> Result<Vec<Request>, ForgeError>;

    /// The commit at the tip of this request now.
    ///
    /// Separate from `open_requests` because it is polled throughout a battery
    /// to notice a new commit, and that poll should cost one cheap call rather
    /// than a listing.
    fn head_of(&self, number: u64) -> Result<String, ForgeError>;

    /// The bodies of comments this forge's authenticated identity has left on
    /// the request, oldest first.
    ///
    /// Bodies rather than a verdict, because recognising reaper's own verdict
    /// in a comment is reaper's business and identical on every forge -- see
    /// `already_judged`. An adapter that parsed it would be a second place for
    /// the marker format to drift.
    fn own_comments(&self, number: u64) -> Result<Vec<String>, ForgeError>;

    /// Whether the forge believes this request merges, if it has an opinion.
    ///
    /// `None` is a real answer and the important one: both forges compute this
    /// asynchronously and report nothing while they are still thinking, which
    /// is exactly the window just after a push. Used to tell a conflict apart
    /// from a transport failure -- a distinction worth a method, because
    /// recording "this conflicts" is sticky, and doing it to a request that
    /// was merely unreachable for a moment would silence it until someone
    /// noticed by hand.
    fn mergeable(&self, number: u64) -> Result<Option<bool>, ForgeError>;

    fn comment(&self, number: u64, body: &str) -> Result<(), ForgeError>;

    /// Rebase the request onto its base branch and merge it.
    fn rebase_merge(&self, number: u64) -> Result<(), ForgeError>;
}

/// A git remote URL, taken apart.
///
/// How git spells a remote is git's business, not any forge's -- the same five
/// shapes appear whoever is hosting -- so the parsing lives here and every
/// adapter shares it. It was briefly duplicated per adapter, and both copies
/// had the same bug: splitting on the first `/` turns the scp-style
/// `git@host:owner/name` into a host of `host:owner`, which then sends every
/// API call somewhere that does not exist and reports it as a 404 from the
/// forge rather than as a misread remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteParts {
    /// `https` or `http`. An ssh remote is reported as `https`: the API is
    /// HTTP wherever the git transport happens to be ssh, and a base of
    /// `ssh://` fails at connect time complaining about TLS.
    pub scheme: String,
    /// Host only, port stripped.
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RemoteParts {
    /// Scheme and host, no trailing slash.
    pub fn base(&self) -> String {
        format!("{}://{}", self.scheme, self.host)
    }

    /// `owner/name`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

fn prefix_len(lowered: &str, prefix: &str) -> Option<usize> {
    lowered.starts_with(prefix).then_some(prefix.len())
}

/// Take a remote URL apart, or decline to guess.
pub fn split_remote(url: &str) -> Option<RemoteParts> {
    let url = url.trim();
    // A URL scheme is case-insensitive (RFC 3986 section 3.1), and git echoes
    // back whatever was configured. Matching only the lowercase spelling left
    // an upper-case remote unparseable, which then picked an adapter by
    // fallback rather than by its host.
    let lower = url.to_ascii_lowercase();
    let (scheme, rest, had_scheme) = if let Some(n) = prefix_len(&lower, "https://") {
        ("https", &url[n..], true)
    } else if let Some(n) = prefix_len(&lower, "http://") {
        ("http", &url[n..], true)
    } else if let Some(n) = prefix_len(&lower, "ssh://") {
        ("https", &url[n..], true)
    } else if let Some(n) = prefix_len(&lower, "git://") {
        ("https", &url[n..], true)
    } else {
        ("https", url, false)
    };
    // Any userinfo belongs to the transport, not to the address.
    let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);

    // scp-style remotes separate host from path with a colon, and only those
    // do: with a scheme present a colon is a port.
    let colon = rest.find(':');
    let slash = rest.find('/');
    let scp = !had_scheme
        && match (colon, slash) {
            (Some(_), None) => true,
            (Some(c), Some(s)) => c < s,
            _ => false,
        };
    let (host, path) = if scp {
        rest.split_once(':')?
    } else {
        rest.split_once('/')?
    };

    let host = host.split_once(':').map(|(h, _)| h).unwrap_or(host);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.trim_matches('/').split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?;
    let name = parts.next()?;
    if host.is_empty() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(RemoteParts {
        scheme: scheme.to_string(),
        host: host.to_ascii_lowercase(),
        owner: owner.to_string(),
        name: name.to_string(),
    })
}

/// The line every verdict comment carries, naming the commit it judged.
///
/// Why a marker and not a timestamp comparison: the obvious rule is "skip the
/// request if my own comment is newer than its head commit", and it is wrong
/// twice. Forge timestamps are second-granular and a commit pushed in the same
/// second as the comment would be skipped forever; and a *force-push of an
/// older commit* has a commit date earlier than the comment, so a genuinely
/// new head would look judged. Naming the commit is exact, needs no clock, and
/// cannot be confused by either -- and it has the side benefit that a reader
/// of the comment can see which commit the verdict was about.
///
/// It also means an ordinary human comment from the same account does not
/// silence the loop, which a time-based rule would do.
pub const VERDICT_PREFIX: &str = "reaper verdict for ";

/// The commit a verdict comment reports on, if it is one.
pub fn judged_head(body: &str) -> Option<&str> {
    body.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(VERDICT_PREFIX)?;
        let sha = rest
            .split(|c: char| !c.is_ascii_hexdigit())
            .next()
            .unwrap_or("");
        // Seven is the shortest abbreviation git will produce unprompted.
        // Anything shorter is prose that happened to start with the prefix.
        (sha.len() >= 7).then_some(sha)
    })
}

/// Has this exact commit already been given a verdict?
///
/// Abbreviations compare either way round: reaper writes the full hash, but a
/// person editing a comment may well shorten it, and a rule that silently
/// stopped recognising its own marker would retest forever.
pub fn already_judged(head: &str, own_comments: &[String]) -> bool {
    own_comments.iter().any(|body| {
        judged_head(body).is_some_and(|sha| {
            let (long, short) = if sha.len() >= head.len() {
                (sha, head)
            } else {
                (head, sha)
            };
            !short.is_empty() && long.starts_with(short)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_remote_shape_git_accepts_names_the_same_repository() {
        for url in [
            "https://forge.example.com/axonibyte/reaper.git",
            "https://forge.example.com/axonibyte/reaper",
            "http://forge.example.com/axonibyte/reaper/",
            "git@forge.example.com:axonibyte/reaper.git",
            "ssh://git@forge.example.com/axonibyte/reaper.git",
            "ssh://git@forge.example.com:22/axonibyte/reaper.git",
            "https://user:secret@forge.example.com/axonibyte/reaper.git",
            "https://forge.example.com:8443/axonibyte/reaper",
            "git://forge.example.com/axonibyte/reaper.git",
            "HTTPS://Forge.Example.com/axonibyte/reaper.git",
            "SSH://git@Forge.Example.COM/axonibyte/reaper.git",
        ] {
            let p = split_remote(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(p.host, "forge.example.com", "{url}");
            assert_eq!(p.slug(), "axonibyte/reaper", "{url}");
        }
    }

    #[test]
    fn an_scp_style_remote_does_not_fold_the_owner_into_the_host() {
        // The bug this exists for: splitting on the first slash makes the host
        // "forge.example.com:axonibyte", and every call then goes to a host
        // that does not resolve -- reported as the forge being unreachable
        // rather than as the remote being misread.
        let p = split_remote("git@forge.example.com:axonibyte/reaper.git").unwrap();
        assert_eq!(p.host, "forge.example.com");
        assert_eq!(p.owner, "axonibyte");
        assert_eq!(p.name, "reaper");
    }

    #[test]
    fn an_ssh_remote_is_addressed_over_http_not_ssh() {
        assert_eq!(
            split_remote("ssh://git@forge.example.com/o/n.git")
                .unwrap()
                .base(),
            "https://forge.example.com"
        );
        assert_eq!(
            split_remote("git@forge.example.com:o/n.git")
                .unwrap()
                .base(),
            "https://forge.example.com"
        );
    }

    #[test]
    fn a_plain_http_remote_keeps_its_scheme() {
        // A stand-in on loopback is http, and silently promoting it to https
        // would make the whole end-to-end suite unreachable.
        assert_eq!(
            split_remote("http://127.0.0.1:8080/o/n.git")
                .unwrap()
                .base(),
            "http://127.0.0.1"
        );
    }

    #[test]
    fn a_remote_with_no_repository_in_it_is_refused_rather_than_guessed() {
        for url in [
            "https://forge.example.com/",
            "https://forge.example.com/owner",
            "https://forge.example.com",
            "nonsense",
            "",
        ] {
            assert_eq!(split_remote(url), None, "{url}");
        }
    }

    fn verdict(sha: &str) -> String {
        format!("reaper battery is green.\n\n{VERDICT_PREFIX}{sha}\n")
    }

    #[test]
    fn a_verdict_comment_names_the_commit_it_judged() {
        let sha = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c";
        assert_eq!(judged_head(&verdict(sha)), Some(sha));
        assert!(already_judged(sha, &[verdict(sha)]));
    }

    #[test]
    fn a_new_commit_is_not_judged_by_the_previous_verdict() {
        let old = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c";
        let new = "ffffffffffffffffffffffffffffffffffffffff";
        assert!(
            !already_judged(new, &[verdict(old)]),
            "a verdict on the previous head must not silence the new one"
        );
    }

    #[test]
    fn an_ordinary_comment_from_the_same_account_is_not_a_verdict() {
        // The whole reason for a marker rather than a timestamp: a person
        // using the same identity to say "looks good to me" must not stop the
        // loop from ever testing this request again.
        let sha = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c";
        let human = vec![
            "looks good to me".to_string(),
            "rebased onto main, please retry".to_string(),
        ];
        assert_eq!(judged_head(&human[0]), None);
        assert!(!already_judged(sha, &human));
    }

    #[test]
    fn an_abbreviated_marker_still_matches_its_commit() {
        // reaper writes the full hash; a person editing the comment may well
        // shorten it, and the loop must keep recognising its own verdict.
        let sha = "0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c";
        let short = format!("{VERDICT_PREFIX}{}", &sha[..12]);
        assert!(already_judged(sha, &[short]));
    }

    #[test]
    fn prose_that_merely_starts_with_the_prefix_is_not_a_marker() {
        // Anti-vacuity for the length floor: without it, any sentence
        // beginning with the prefix would claim to be a verdict for whatever
        // hex-ish word followed, and every request would look judged.
        assert_eq!(judged_head("reaper verdict for abc was unclear"), None);
        assert!(!already_judged(
            "abcdef0123456789",
            &["reaper verdict for abc was unclear".to_string()]
        ));
    }

    #[test]
    fn a_marker_is_found_wherever_it_sits_in_the_body() {
        let sha = "abcdef0123456789abcdef0123456789abcdef01";
        let body = format!("## failures\n\n- one\n- two\n\n{VERDICT_PREFIX}{sha}\n\nsee the log.");
        assert_eq!(judged_head(&body), Some(sha));
    }

    #[test]
    fn no_comments_at_all_means_not_judged() {
        assert!(!already_judged(
            "abcdef0123456789abcdef0123456789abcdef01",
            &[]
        ));
    }
}
