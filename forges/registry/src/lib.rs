//! Turning a git remote into a forge implementation.
//!
//! This crate exists for the same boundary reason the provider registry does.
//! Something has to decide which adapter a remote URL implies, and wherever
//! that decision lives will contain the name of every supported forge. Put it
//! in the CLI and the forge guard fires -- correctly, because the CLI would
//! then know what a GitHub is.
//!
//! Adding a forge means adding a dependency, one arm in `build` and one line
//! in `detect`. There is no dynamic loading and no registration protocol.

use reaper_core::forge::{split_remote, Forge, ForgeError};

/// A stand-in forge, for tests that need to drive the whole stack.
///
/// Which adapter supplies it is this crate's business, exactly as the real
/// implementations are: a caller driving `auto` end to end needs *a* forge,
/// not a particular one, so the CLI's tests need no exemption from the forge
/// lint.
#[cfg(feature = "mock")]
pub mod mock {
    pub use reaper_forge_forgejo::mock::StandIn;

    /// The adapter name the stand-in answers as, and the key its configuration
    /// table takes.
    ///
    /// Exported so a caller driving the whole stack never has to write a
    /// forge's name: the end-to-end suite passes this to `--forge` and uses it
    /// as the table key, exactly as it takes its hypervisor from here rather
    /// than naming one. An exemption from the forge lint would be a place
    /// coupling could hide later; not needing one is better.
    pub const NAME: &str = "forgejo";
}

/// Forge names this build supports, in the order they are offered to a reader
/// who got the name wrong.
pub const AVAILABLE: &[&str] = &["github", "forgejo"];

/// Which adapter a remote URL implies.
///
/// One host is recognised by name and everything else falls through to the
/// self-hosted adapter. That asymmetry is deliberate: there is exactly one
/// host for the hosted forge, while the other is self-hosted by definition and
/// its hostname is whatever the operator chose. A list of known hostnames
/// would be a list that is wrong for every new installation.
pub fn detect(remote: &str) -> &'static str {
    let Some(parts) = split_remote(remote) else {
        // Unparseable: fall through, and let `build` produce the message that
        // names the remote. Guessing an adapter here would hand a credential
        // to whatever the string turned out to mean.
        return "forgejo";
    };
    let host = parts.host;
    let hosted = reaper_forge_github::HOST;
    if host == hosted || host.ends_with(&format!(".{hosted}")) {
        "github"
    } else {
        "forgejo"
    }
}

/// Build the named adapter for a remote, from its own slice of site config.
pub fn build(
    name: &str,
    remote: &str,
    table: Option<&toml::Value>,
) -> Result<Box<dyn Forge>, ForgeError> {
    match name {
        "github" => Ok(Box::new(reaper_forge_github::GitHub::from_remote(
            remote, table,
        )?)),
        "forgejo" => Ok(Box::new(reaper_forge_forgejo::Forgejo::from_remote(
            remote, table,
        )?)),
        other => Err(ForgeError::Config(format!(
            "no forge named {other:?} in this build; available: {}",
            AVAILABLE.join(", ")
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_forge_names_the_ones_that_exist() {
        let m = match build("sourcehut", "https://example.com/o/n", None) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a forge this build does not have should not be buildable"),
        };
        assert!(m.contains("sourcehut"), "{m}");
        assert!(m.contains("github") && m.contains("forgejo"), "{m}");
    }

    #[test]
    fn the_hosted_forge_is_recognised_in_every_remote_shape() {
        for remote in [
            "https://github.com/o/n.git",
            "git@github.com:o/n.git",
            "ssh://git@github.com/o/n",
            "HTTPS://GitHub.com/o/n",
        ] {
            assert_eq!(detect(remote), "github", "{remote}");
        }
    }

    #[test]
    fn anything_self_hosted_falls_through_to_the_other_adapter() {
        for remote in [
            "https://forge.axonibyte.com/axonibyte/reaper.git",
            "git@git.example.org:o/n.git",
            "https://github.example.com/o/n",
        ] {
            assert_eq!(detect(remote), "forgejo", "{remote}");
        }
    }

    #[test]
    fn a_lookalike_hostname_is_not_mistaken_for_the_hosted_forge() {
        // The failure this guards: a host that merely CONTAINS the hosted
        // forge's name -- an internal mirror called github.example.com, or a
        // typosquat -- must not be handed that adapter and its credential.
        // Only the host itself and its own subdomains count.
        for remote in [
            "https://github.com.evil.example/o/n",
            "https://notgithub.com/o/n",
            "https://github.example.com/o/n",
        ] {
            assert_eq!(detect(remote), "forgejo", "{remote}");
        }
        assert_eq!(detect("https://www.github.com/o/n"), "github");
    }
}
