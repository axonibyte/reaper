//! Validates tenant manifests against the normative schema.
//!
//! A thin wrapper: everything it knows lives in the library, so that the CLI
//! and this tool can never disagree about whether a manifest is acceptable.

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("usage: reaper-manifest-validate <manifest.toml> [...]");
        eprintln!();
        eprintln!("Validates each manifest against the reaper manifest schema, v1.");
        eprintln!("Exit 0 if every manifest is valid, 1 if any is invalid, 2 on a");
        eprintln!("usage, I/O or internal error.");
        return ExitCode::from(2);
    }

    let mut invalid = 0usize;
    let mut errored = 0usize;

    for path in &args {
        match reaper_manifest::load(Path::new(path)) {
            Ok(m) => {
                let n = m.guests.len();
                // Jobs multiply guests into sessions, and a session is a
                // machine. "How many machines will this cost me" is the
                // question a tenant sharding a suite actually has, and this is
                // the tool the onboarding instructions tell them to run --
                // so it is answered here rather than discovered at `up`.
                // Said only where jobs are declared: a manifest without them
                // has one session per guest and nothing to add.
                let sessions: usize = m.guests.iter().map(|g| g.run.jobs.len()).sum();
                let plural = |k: usize| if k == 1 { "" } else { "s" };
                if sessions > n {
                    println!(
                        "ok    {path}  ({n} guest{}, {sessions} session{} -- one per job)",
                        plural(n),
                        plural(sessions)
                    );
                } else {
                    println!("ok    {path}  ({n} guest{})", plural(n));
                }
            }
            Err(e @ reaper_manifest::Error::Invalid { .. }) => {
                invalid += 1;
                println!("FAIL  {e}");
            }
            Err(e) => {
                errored += 1;
                println!("ERROR {e}");
            }
        }
    }

    if errored > 0 {
        ExitCode::from(2)
    } else if invalid > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
