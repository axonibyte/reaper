//! Forgejo, over its HTTP API.
//!
//! Unlike the other adapter this one talks to the forge directly, because
//! there is no first-party command-line client to delegate to. That means
//! reaper holds a credential, which is a cost: see `docs/auto.md` for where it
//! comes from and why a command is offered as an alternative to a literal.
//!
//! Only five routes are used, and the adapter deliberately knows nothing about
//! what reaper does with the answers.

#[cfg(feature = "mock")]
pub mod mock;

use std::process::{Command, Stdio};

use reaper_core::forge::{split_remote, Forge, ForgeError, Request};
use serde::Deserialize;
use serde_json::{json, Value};
use ureq::Agent;

pub struct Forgejo {
    agent: Agent,
    /// Scheme and host, no trailing slash.
    base: String,
    owner: String,
    name: String,
    auth: String,
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

impl Forgejo {
    /// Build from a git remote URL and this adapter's slice of site config.
    ///
    /// Credentials, in the order tried: `token`, then `user` with `password`,
    /// then `user` with `password_command`. A command is offered because a
    /// password in a world-readable config file is the kind of convenience
    /// that is only noticed once.
    pub fn from_remote(url: &str, table: Option<&toml::Value>) -> Result<Self, ForgeError> {
        // The remote is the default source for both the API host and the
        // repository, and configuration may override either. That is not only
        // for tests: a self-hosted forge routinely answers its API on a
        // different hostname from the one git pushes to, and a remote that is
        // a filesystem path has no host in it at all.
        let parts = split_remote(url);
        let over = |k: &str| {
            table
                .and_then(|t: &toml::Value| t.get(k))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let base = match over("url") {
            Some(u) => u.trim_end_matches('/').to_string(),
            None => parts
                .as_ref()
                .map(|p| p.base())
                .ok_or_else(|| ForgeError::Config(format!(
                    "cannot tell which host serves {url:?}; set url in [forges.forgejo]"
                )))?,
        };
        let (owner, name) = match over("repo") {
            Some(slug) => match slug.split_once('/') {
                Some((o, n)) if !o.is_empty() && !n.is_empty() => {
                    (o.to_string(), n.to_string())
                }
                _ => {
                    return Err(ForgeError::Config(format!(
                        "[forges.forgejo] repo is {slug:?}; it should be owner/name"
                    )))
                }
            },
            None => parts
                .as_ref()
                .map(|p| (p.owner.clone(), p.name.clone()))
                .ok_or_else(|| ForgeError::Config(format!(
                    "cannot tell which repository {url:?} is; set repo in [forges.forgejo]"
                )))?,
        };
        let t = table.ok_or_else(|| {
            ForgeError::Config(format!(
                "no credential for {base}. Add a [forges.forgejo] table to site \
                 configuration with a token, or a user and password_command"
            ))
        })?;
        let get = |k: &str| t.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let auth = if let Some(token) = get("token") {
            format!("token {token}")
        } else {
            let user = get("user").ok_or_else(|| {
                ForgeError::Config(format!(
                    "[forges.forgejo] for {base} has neither token nor user"
                ))
            })?;
            let password = match (get("password"), get("password_command")) {
                (Some(p), _) => p,
                (None, Some(cmd)) => read_command(&cmd)?,
                (None, None) => {
                    return Err(ForgeError::Config(format!(
                        "[forges.forgejo] names user {user:?} but gives no password \
                         or password_command"
                    )))
                }
            };
            format!("Basic {}", base64(format!("{user}:{password}").as_bytes()))
        };
        Ok(Forgejo {
            agent: Agent::new_with_defaults(),
            base,
            owner,
            name,
            auth,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1/{path}", self.base)
    }

    fn get(&self, path: &str) -> Result<String, ForgeError> {
        let mut resp = self
            .agent
            .get(self.url(path))
            .header("Authorization", &self.auth)
            .header("Accept", "application/json")
            .call()
            .map_err(|e| ForgeError::Transport(format!("{path}: {e}")))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| ForgeError::Transport(format!("{path}: cannot read the reply: {e}")))
    }

    fn post(&self, path: &str, body: Value) -> Result<(), ForgeError> {
        self.agent
            .post(self.url(path))
            .header("Authorization", &self.auth)
            .header("Accept", "application/json")
            .send_json(&body)
            .map_err(|e| ForgeError::Refused {
                what: path.to_string(),
                why: e.to_string(),
            })?;
        Ok(())
    }

    fn repo(&self) -> String {
        format!("repos/{}/{}", self.owner, self.name)
    }

    fn own_login(&self) -> Result<String, ForgeError> {
        let u: User = serde_json::from_str(&self.get("user")?).map_err(|e| {
            ForgeError::Transport(format!("cannot read the authenticated identity: {e}"))
        })?;
        Ok(u.login)
    }
}

/// Read a secret from a command's first line of output.
fn read_command(cmd: &str) -> Result<String, ForgeError> {
    let out = Command::new("sh")
        .args(["-c", cmd])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| ForgeError::Config(format!("password_command {cmd:?} will not run: {e}")))?;
    if !out.status.success() {
        return Err(ForgeError::Config(format!(
            "password_command {cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let secret = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    if secret.is_empty() {
        return Err(ForgeError::Config(format!(
            "password_command {cmd:?} printed nothing"
        )));
    }
    Ok(secret)
}

/// Standard base64, which is all basic authentication needs.
///
/// Hand-rolled rather than taken as a dependency: it is sixteen lines, the
/// alphabet has not changed since 1987, and a transitive dependency added for
/// sixteen lines is a supply-chain decision nobody revisits.
fn base64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(A[(n >> 18 & 63) as usize] as char);
        out.push(A[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

impl Forge for Forgejo {
    fn open_requests(&self) -> Result<Vec<Request>, ForgeError> {
        let text = self.get(&format!(
            "{}/pulls?state=open&sort=oldest&limit=50",
            self.repo()
        ))?;
        let pulls: Vec<Pull> = serde_json::from_str(&text)
            .map_err(|e| ForgeError::Transport(format!("cannot read the open requests: {e}")))?;
        Ok(pulls
            .into_iter()
            .map(|p| Request {
                number: p.number,
                title: p.title,
                head: p.head.sha,
                fetch_ref: format!("refs/pull/{}/head", p.number),
            })
            .collect())
    }

    fn head_of(&self, number: u64) -> Result<String, ForgeError> {
        let text = self.get(&format!("{}/pulls/{number}", self.repo()))?;
        let p: Pull = serde_json::from_str(&text)
            .map_err(|e| ForgeError::Transport(format!("cannot read request {number}: {e}")))?;
        Ok(p.head.sha)
    }

    fn own_comments(&self, number: u64) -> Result<Vec<String>, ForgeError> {
        let me = self.own_login()?;
        let text = self.get(&format!("{}/issues/{number}/comments", self.repo()))?;
        let all: Vec<Comment> = serde_json::from_str(&text).map_err(|e| {
            ForgeError::Transport(format!("cannot read comments on request {number}: {e}"))
        })?;
        Ok(all
            .into_iter()
            .filter(|c| c.user.login == me)
            .map(|c| c.body)
            .collect())
    }

    fn comment(&self, number: u64, body: &str) -> Result<(), ForgeError> {
        self.post(
            &format!("{}/issues/{number}/comments", self.repo()),
            json!({ "body": body }),
        )
    }

    fn rebase_merge(&self, number: u64) -> Result<(), ForgeError> {
        // "rebase" rebases and fast-forwards, which is what the other forge
        // calls rebase-and-merge. "rebase-merge" is a different thing here --
        // it rebases and then adds a merge commit -- so the names cross over
        // and the wrong one is a silently different history.
        self.post(
            &format!("{}/pulls/{number}/merge", self.repo()),
            json!({ "Do": "rebase" }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_credential_says_which_table_to_write() {
        let m = match Forgejo::from_remote("https://forge.example.com/o/n", None) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("should refuse without a credential"),
        };
        assert!(m.contains("forges.forgejo"), "{m}");
        assert!(m.contains("forge.example.com"), "should name the host: {m}");
    }

    #[test]
    fn a_user_without_any_password_is_refused_before_any_request() {
        let t: toml::Value = toml::from_str("user = \"someone\"").unwrap();
        let m = match Forgejo::from_remote("https://forge.example.com/o/n", Some(&t)) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a user with no password at all must be refused"),
        };
        assert!(m.contains("password_command"), "{m}");
    }

    #[test]
    fn base64_matches_the_published_vectors() {
        // RFC 4648 section 10. The encoder is hand-rolled, so it is checked
        // against the standard's own vectors rather than against itself.
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input:?}");
        }
    }

    #[test]
    fn a_password_command_supplies_the_secret_and_a_broken_one_says_so() {
        assert_eq!(read_command("printf 'hunter2\\n'").unwrap(), "hunter2");
        let e = read_command("exit 3").expect_err("a failing command must not pass silently");
        assert!(e.to_string().contains("failed"), "{e}");
        let e = read_command("true").expect_err("an empty secret is not a secret");
        assert!(e.to_string().contains("printed nothing"), "{e}");
    }
}

#[cfg(test)]
mod override_tests {
    use super::*;

    fn table(text: &str) -> toml::Value {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn configuration_may_name_a_different_api_host_from_the_git_remote() {
        let t = table(
            "url = \"https://api.forge.example.com/\"\ntoken = \"t\"",
        );
        let f = Forgejo::from_remote("git@push.forge.example.com:o/n.git", Some(&t)).unwrap();
        assert_eq!(f.base, "https://api.forge.example.com");
        assert_eq!((f.owner.as_str(), f.name.as_str()), ("o", "n"));
    }

    #[test]
    fn a_remote_with_no_host_in_it_works_when_configuration_supplies_both() {
        // A filesystem remote -- which is what a clone of a clone has, and
        // what the end-to-end suite uses.
        let t = table(
            "url = \"http://127.0.0.1:9\"\nrepo = \"o/n\"\ntoken = \"t\"",
        );
        let f = Forgejo::from_remote("/srv/git/mirror.git", Some(&t)).unwrap();
        assert_eq!(f.base, "http://127.0.0.1:9");
        assert_eq!((f.owner.as_str(), f.name.as_str()), ("o", "n"));
    }

    #[test]
    fn a_remote_with_no_host_and_no_override_says_which_key_to_set() {
        let t = table("token = \"t\"");
        let m = match Forgejo::from_remote("/srv/git/mirror.git", Some(&t)) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("there is nothing here to address"),
        };
        assert!(m.contains("set url in [forges.forgejo]"), "{m}");
    }

    #[test]
    fn a_malformed_repo_override_is_refused_rather_than_half_used() {
        let t = table("url = \"http://x\"\nrepo = \"justaname\"\ntoken = \"t\"");
        let m = match Forgejo::from_remote("/srv/git/m.git", Some(&t)) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("owner/name is the only shape that works"),
        };
        assert!(m.contains("owner/name"), "{m}");
    }
}
