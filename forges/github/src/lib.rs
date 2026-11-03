//! GitHub, through the `gh` CLI.
//!
//! Why a subprocess rather than the HTTP API: `gh` already holds the
//! credential, refreshes it, honours enterprise hosts and `GH_TOKEN`, and is
//! what the operator has already authenticated for their own use. Reaching for
//! the API directly would mean reaper growing a second place to keep a GitHub
//! credential, which is the one thing worse than shelling out.
//!
//! Replies are parsed as JSON rather than coaxed into lines with `--jq`. A
//! title can contain a tab or a newline, and a line-oriented parser that is
//! right for every title anyone has tried yet is a defect waiting for a
//! release note.

use std::process::{Command, Stdio};

use reaper_core::forge::{split_remote, Forge, ForgeError, Request};
use serde::Deserialize;

pub struct GitHub {
    /// `owner/name`, as `gh` wants it.
    slug: String,
    /// The binary to drive. Configurable so a test can hand it a stand-in and
    /// so an operator with a wrapper script is not stuck.
    gh: String,
}

#[derive(Deserialize)]
struct Head {
    sha: String,
}

#[derive(Deserialize)]
struct Pull {
    number: u64,
    title: String,
    head: Head,
    /// Absent while the forge is still working it out, which is a state the
    /// caller has to be able to see -- hence Option rather than a default.
    #[serde(default)]
    mergeable: Option<bool>,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct Comment {
    body: String,
    user: User,
}

/// The host this adapter answers for, as it appears in a remote URL.
pub const HOST: &str = "github.com";

impl GitHub {
    /// Build from a git remote URL and this adapter's slice of site config.
    ///
    /// The URL is parsed here rather than in the registry because the shape of
    /// a remote -- which hosts are this forge, how the path maps to a
    /// repository -- is exactly the knowledge the seam exists to contain.
    pub fn from_remote(url: &str, table: Option<&toml::Value>) -> Result<Self, ForgeError> {
        let slug = split_remote(url)
            .ok_or_else(|| {
                ForgeError::Config(format!(
                    "cannot tell which repository {url:?} is; expected a host/owner/name URL"
                ))
            })?
            .slug();
        let gh = table
            .and_then(|t| t.get("command"))
            .and_then(|v| v.as_str())
            .unwrap_or("gh")
            .to_string();
        Ok(GitHub { slug, gh })
    }

    fn run(&self, args: &[&str]) -> Result<String, ForgeError> {
        let out = Command::new(&self.gh)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| {
                ForgeError::Config(format!(
                    "cannot run {:?}: {e}. The GitHub adapter drives that command; \
                     install it and authenticate, or set forge command in site config",
                    self.gh
                ))
            })?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(ForgeError::Refused {
                what: format!("{} {}", self.gh, args.join(" ")),
                why: if why.is_empty() {
                    format!("exit {:?}", out.status.code())
                } else {
                    why
                },
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn api(&self, path: &str) -> Result<String, ForgeError> {
        self.run(&["api", "-H", "Accept: application/vnd.github+json", path])
    }

    fn own_login(&self) -> Result<String, ForgeError> {
        let text = self.api("user")?;
        let u: User = serde_json::from_str(&text).map_err(|e| {
            ForgeError::Transport(format!("cannot read the authenticated identity: {e}"))
        })?;
        Ok(u.login)
    }
}

impl Forge for GitHub {
    fn open_requests(&self) -> Result<Vec<Request>, ForgeError> {
        let text = self.api(&format!(
            "repos/{}/pulls?state=open&sort=created&direction=asc&per_page=100",
            self.slug
        ))?;
        let pulls: Vec<Pull> = serde_json::from_str(&text)
            .map_err(|e| ForgeError::Transport(format!("cannot read the open requests: {e}")))?;
        Ok(pulls
            .into_iter()
            .map(|p| Request {
                number: p.number,
                title: p.title,
                head: p.head.sha,
                // The merge commit, not the head: see Request::fetch_ref.
                fetch_ref: format!("refs/pull/{}/merge", p.number),
            })
            .collect())
    }

    fn head_of(&self, number: u64) -> Result<String, ForgeError> {
        let text = self.api(&format!("repos/{}/pulls/{number}", self.slug))?;
        let p: Pull = serde_json::from_str(&text)
            .map_err(|e| ForgeError::Transport(format!("cannot read request {number}: {e}")))?;
        Ok(p.head.sha)
    }

    fn own_comments(&self, number: u64) -> Result<Vec<String>, ForgeError> {
        let me = self.own_login()?;
        let text = self.api(&format!(
            "repos/{}/issues/{number}/comments?per_page=100",
            self.slug
        ))?;
        let all: Vec<Comment> = serde_json::from_str(&text).map_err(|e| {
            ForgeError::Transport(format!("cannot read comments on request {number}: {e}"))
        })?;
        Ok(all
            .into_iter()
            .filter(|c| c.user.login == me)
            .map(|c| c.body)
            .collect())
    }

    fn mergeable(&self, number: u64) -> Result<Option<bool>, ForgeError> {
        let text = self.api(&format!("repos/{}/pulls/{number}", self.slug))?;
        let p: Pull = serde_json::from_str(&text)
            .map_err(|e| ForgeError::Transport(format!("cannot read request {number}: {e}")))?;
        Ok(p.mergeable)
    }

    fn comment(&self, number: u64, body: &str) -> Result<(), ForgeError> {
        // -F reads the value from a file, and - is stdin; passing a multi-line
        // verdict as an argument would meet the platform's argument limit on a
        // long battery log.
        let mut child = Command::new(&self.gh)
            .args([
                "api",
                "--method",
                "POST",
                &format!("repos/{}/issues/{number}/comments", self.slug),
                "-F",
                "body=@-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ForgeError::Config(format!("cannot run {:?}: {e}", self.gh)))?;
        {
            use std::io::Write;
            let pipe = child.stdin.as_mut().expect("stdin was piped");
            pipe.write_all(body.as_bytes())
                .map_err(|e| ForgeError::Transport(format!("cannot write the comment: {e}")))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| ForgeError::Transport(format!("cannot post the comment: {e}")))?;
        if !out.status.success() {
            return Err(ForgeError::Refused {
                what: format!("commenting on request {number}"),
                why: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(())
    }

    fn rebase_merge(&self, number: u64) -> Result<(), ForgeError> {
        self.run(&[
            "api",
            "--method",
            "PUT",
            &format!("repos/{}/pulls/{number}/merge", self.slug),
            "-f",
            "merge_method=rebase",
        ])
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_this_adapter_cannot_place_is_refused_at_construction() {
        // Refused HERE rather than at the first call, so the operator is told
        // their remote is unreadable instead of being shown a 404 from the
        // forge. The shapes themselves are core's business and tested there.
        let m = match GitHub::from_remote("https://github.com/owner", None) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("half a path is not a repository"),
        };
        assert!(m.contains("cannot tell which repository"), "{m}");
    }

    #[test]
    fn the_slug_is_what_the_command_is_given() {
        let f = GitHub::from_remote("git@github.com:axonibyte/reaper.git", None).unwrap();
        assert_eq!(f.slug, "axonibyte/reaper");
    }

    #[test]
    fn the_command_to_drive_comes_from_configuration_when_given() {
        let table: toml::Value = toml::from_str("command = \"/opt/bin/gh-wrapper\"").unwrap();
        let f = GitHub::from_remote("https://github.com/o/n", Some(&table)).unwrap();
        assert_eq!(f.gh, "/opt/bin/gh-wrapper");
        let d = GitHub::from_remote("https://github.com/o/n", None).unwrap();
        assert_eq!(d.gh, "gh");
    }

    #[test]
    fn a_missing_command_says_what_to_install_rather_than_no_such_file() {
        let f = GitHub::from_remote("https://github.com/o/n", None).unwrap();
        let broken = GitHub {
            slug: f.slug,
            gh: "/nonexistent/reaper-test-no-such-binary".into(),
        };
        let e = broken.open_requests().expect_err("should not succeed");
        let m = e.to_string();
        assert!(m.contains("install it"), "{m}");
    }
}
