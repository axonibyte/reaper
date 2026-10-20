//! Tests for manifest loading.
//!
//! The fixtures under `test/` are shared with `test/run.sh`, which drives the
//! same cases through the binary. Both matter: this module proves the typed
//! model is right, and the shell suite proves the tool a person actually runs
//! behaves the same way.

use super::*;

/// A scratch directory unique to this CALL, removed on drop.
///
/// The counter is the whole point. Both helpers below used to name the
/// directory after `text.len()`, so two cases whose text happened to be the
/// same length shared one directory -- and `Drop` runs `remove_dir_all`, so
/// whichever finished first deleted the file the other was still reading.
/// Sequentially that is invisible (the first is dropped before the second is
/// built); under `cargo test`'s threads it fails as
/// "m.toml: cannot read: No such file or directory", from a test that never
/// touches the filesystem on purpose. Observed on
/// reaper-manifest-err-5439-104.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    // Drop, not a call at the end: a panic inside load() must still remove the
    // directory. The invariants battery refuses this file without it, and it
    // caught this very helper leaking on its first draft.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Scratch {
    fn new(label: &str) -> Scratch {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "reaper-manifest-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Scratch(dir)
    }

    fn file(&self, name: &str, body: &str) -> std::path::PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }
}

fn fixture(rel: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// The one job a manifest that declares none resolves to.
///
/// Asserted rather than indexed. Every caller below was written before jobs
/// existed, so what each of them now also checks is that such a manifest still
/// resolves to exactly one job carrying no name -- which is the whole of the
/// backwards-compatibility claim, and is worth re-stating at every site that
/// depends on it rather than once somewhere else.
fn sole_job(g: &Guest) -> &Job {
    assert_eq!(
        g.run.jobs.len(),
        1,
        "{}: a run declaring no jobs resolves to exactly one",
        g.name
    );
    let j = &g.run.jobs[0];
    assert_eq!(j.name, None, "{}: the implicit job carries no name", g.name);
    j
}

fn load_ok(rel: &str) -> Manifest {
    load(&fixture(rel)).unwrap_or_else(|e| panic!("{rel} should load: {e}"))
}

fn load_err(rel: &str) -> Error {
    match load(&fixture(rel)) {
        Ok(_) => panic!("{rel} should have been rejected"),
        Err(e) => e,
    }
}

/// Every shipped example must load. Discovered rather than listed, so that
/// adding an example extends this test with no edit here -- and so that these
/// tests name no tenant, which is the same rule the framework itself follows.
#[test]
fn every_shipped_example_loads() {
    let dir = fixture("examples");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("examples directory") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let m = load(&path).unwrap_or_else(|e| panic!("{} should load: {e}", path.display()));
        assert!(!m.project.is_empty());
        assert!(!m.guests.is_empty());
        seen += 1;
    }
    assert!(
        seen >= 2,
        "expected at least two worked examples, found {seen}"
    );
}

/// The execution-mode invariant, asserted over whatever examples exist rather
/// than over a remembered list of their contents. Asserting the property is
/// also a better test than asserting the fixture: it keeps holding when the
/// examples change.
#[test]
fn every_example_verb_agrees_with_its_execution_mode() {
    // Per verb, not per guest: the two verbs of one guest may legitimately
    // differ, so checking the guest's default against the build's image -- as
    // this once did -- would ask the wrong question of a manifest that splits
    // them.
    fn coherent(what: &str, exec: Exec, image: Option<&String>) {
        match exec {
            Exec::Container => assert!(
                image.is_some(),
                "{what}: container execution needs a toolchain image"
            ),
            Exec::Host => assert!(
                image.is_none(),
                "{what}: host execution has no image to run in"
            ),
        }
    }

    let dir = fixture("examples");
    for entry in std::fs::read_dir(&dir).expect("examples directory") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        for g in &load(&path).expect("loads").guests {
            if let Some(b) = &g.build {
                coherent(&format!("{} build", g.name), b.exec, b.image.as_ref());
            }
            assert!(
                !g.run.jobs.is_empty(),
                "{}: a resolved run always has a job",
                g.name
            );
            for j in &g.run.jobs {
                let what = match &j.name {
                    Some(n) => format!("{} run job {n}", g.name),
                    None => format!("{} run", g.name),
                };
                coherent(&what, j.exec, j.image.as_ref());
                assert!(!j.cmd.is_empty(), "{what}: a job without a command");
            }
        }
    }
}

#[test]
fn a_container_guest_carries_its_toolchain_and_pre_pulled_images() {
    let m = load_ok("test/valid/container-with-images.toml");
    assert_eq!(m.guests.len(), 1);

    let g = &m.guests[0];
    assert_eq!(g.exec, Some(Exec::Container));
    assert!(g.build.as_ref().unwrap().image.is_some());
    assert_eq!(g.run.images.len(), 3);
    assert_eq!(m.reset, vec!["state".to_string()]);
    assert_eq!(g.resources.cores, Some(4));
    assert_eq!(g.resources.ram_gb, Some(8));
}

#[test]
fn a_project_that_runs_no_containers_declares_no_images() {
    // The assertion that the schema is not shaped around one reference tenant.
    let m = load_ok("test/valid/no-images.toml");
    assert_eq!(m.guests[0].exec, Some(Exec::Host));
    assert!(m.guests[0].run.images.is_empty());
    assert!(m.guests[0].build.as_ref().unwrap().image.is_none());
}

#[test]
fn the_smallest_legal_manifest_needs_no_build_and_no_reset() {
    let m = load_ok("test/valid/minimal.toml");
    assert_eq!(m.guests.len(), 1);
    assert!(m.guests[0].build.is_none());
    assert!(m.reset.is_empty());
    assert!(m.profiles.is_empty());
}

#[test]
fn defaults_and_overrides_merge_across_scopes() {
    let m = load_ok("test/valid/inherited-across-scopes.toml");
    let a = m.guest("guest-a").unwrap();
    let b = m.guest("guest-b").unwrap();

    assert_eq!(a.exec, Some(Exec::Host));
    assert_eq!(b.exec, Some(Exec::Container));
    // Command from the top level, image from the guest: neither scope holds
    // both, so this passes only if resolution merges before the typed model is
    // built.
    assert_eq!(a.build.as_ref().unwrap().cmd, "make build");
    assert_eq!(b.build.as_ref().unwrap().cmd, "make build");
    assert!(b.build.as_ref().unwrap().image.is_some());
}

#[test]
fn profiles_are_read_but_not_interpreted() {
    let m = load_ok("test/valid/container-with-images.toml");
    let nightly = m.profiles.get("nightly").expect("nightly profile");
    // Kept as written. This crate has no notion of time, deliberately: turning
    // "12h" into a duration is the caller's job.
    assert_eq!(nightly.ttl.as_deref(), Some("12h"));
    assert_eq!(nightly.warm_cache, Some(false));
}

/// Every invalid fixture must be rejected as *invalid* rather than blowing up
/// as an internal error, because the difference is what a user is told.
#[test]
fn invalid_fixtures_are_reported_as_invalid() {
    // Discovered rather than listed. A remembered list is a list that goes stale
    // the first time someone adds a fixture and does not think to come here.
    let dir = fixture("test/invalid");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("invalid fixtures") {
        let path = entry.expect("readable entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let e = match load(&path) {
            Ok(_) => panic!("{} should have been rejected", path.display()),
            Err(e) => e,
        };
        assert!(
            matches!(e, Error::Invalid { .. }),
            "{} should be Invalid, got: {e}",
            path.display()
        );
        seen += 1;
    }
    assert!(seen >= 8, "expected the invalid fixtures, found {seen}");
}

#[test]
fn the_exec_conditional_is_checked_after_merging() {
    // exec on the guest, build.image at the top level. Neither location is
    // wrong alone; only the merged form is.
    let e = load_err("test/invalid/host-exec-with-image.toml");
    let Error::Invalid { problems, .. } = &e else {
        panic!("expected Invalid, got {e}");
    };
    assert!(
        problems
            .iter()
            .any(|p| p.contains("once defaults are merged in")),
        "the failure should name the merged form: {problems:?}"
    );
}

#[test]
fn every_problem_is_reported_not_just_the_first() {
    // Two independent faults: an unpinned image and an unknown key.
    let text = r#"
schema = 1
project = "two-faults"
guests = ["g"]
exec = "container"
build = { image = "docker.io/library/x:latest", cmd = "make" }
run = { cmd = "make test" }
nonsense = true
"#;
    let e = match from_str(text, "<inline>") {
        Err(e) => e,
        Ok(_) => panic!("should be invalid"),
    };
    let Error::Invalid { problems, .. } = &e else {
        panic!("expected Invalid, got {e}");
    };
    assert!(
        problems.len() >= 2,
        "both faults should be reported, got: {problems:?}"
    );
}

#[test]
fn unparseable_toml_is_a_parse_error_not_an_invalid_manifest() {
    // The distinction matters: "your TOML is broken" and "your manifest is
    // wrong" send a reader to different places.
    let e = match from_str("guests = [\"unclosed\"", "<inline>") {
        Err(e) => e,
        Ok(_) => panic!("should not parse"),
    };
    assert!(matches!(e, Error::Parse { .. }), "got {e}");
}

#[test]
fn a_missing_file_is_a_read_error() {
    let e = match load(&fixture("test/does-not-exist.toml")) {
        Err(e) => e,
        Ok(_) => panic!("should not load"),
    };
    assert!(matches!(e, Error::Read { .. }), "got {e}");
}

#[test]
fn the_embedded_schema_compiles() {
    // If this fails, every other test in this file is failing for the wrong
    // reason.
    let schema: Value = serde_json::from_str(SCHEMA).expect("schema is JSON");
    jsonschema::validator_for(&schema).expect("schema compiles");
}

/// Execution mode is a property of a verb, and the guest's is only a default.
///
/// This is the case that made the change necessary: a project whose build needs
/// a pinned toolchain and whose run drives the guest's own container engine.
/// Running the second inside the first cannot work -- a toolchain image carries
/// no engine client -- so the pair has to be expressible.
#[test]
fn a_verb_may_override_the_guests_execution_mode() {
    let m = load_ok("test/valid/per-verb-exec.toml");
    let g = &m.guests[0];

    assert_eq!(g.exec, Some(Exec::Container), "the guest's default");
    assert_eq!(g.build.as_ref().unwrap().exec, Exec::Container);
    assert_eq!(sole_job(g).exec, Exec::Host, "the run overrode it");

    // And the override does not drag an image along with it.
    assert!(g.build.as_ref().unwrap().image.is_some());
    assert_eq!(
        sole_job(g).image,
        None,
        "a host-execution run has nowhere to run an image, so it must not \
         inherit one -- it would then be rejected for a key nobody wrote"
    );
}

/// Two verbs in one toolchain declare the digest once.
#[test]
fn a_container_run_inherits_the_build_image() {
    let m = load_ok("test/valid/run-inherits-the-build-image.toml");
    let g = &m.guests[0];
    let build_image = g.build.as_ref().unwrap().image.clone();

    assert!(build_image.is_some());
    assert_eq!(
        sole_job(g).image,
        build_image,
        "the run declared no image of its own, so it runs in the build's"
    );
}

/// The axis a tenant was previously buying a second manifest to get: one
/// command, several shards, each on its own session.
#[test]
fn jobs_shard_one_command_across_sessions() {
    let m = load_ok("test/valid/run-jobs.toml");
    let g = &m.guests[0];

    let names: Vec<Option<&str>> = g.run.jobs.iter().map(|j| j.name.as_deref()).collect();
    assert_eq!(
        names,
        vec![
            Some("shard-1"),
            Some("shard-2"),
            Some("shard-3"),
            Some("shard-4")
        ],
        "declaration order is kept: it is the order sessions are named in"
    );

    for j in &g.run.jobs {
        assert_eq!(
            j.cmd, "make check",
            "every shard inherits the run's command"
        );
        assert_eq!(j.exec, Exec::Host, "and the guest's execution mode");
        assert_eq!(j.image, None);
    }

    // env is replaced whole rather than merged, exactly as a guest's override
    // of `run` is. The fixture restates CI in every shard for that reason, and
    // this is the assertion that would fail if someone made it merge -- which
    // would be a second answer to a question the manifest already answers.
    let shard = |n: &str| {
        g.job(Some(n))
            .unwrap_or_else(|| panic!("no job {n}"))
            .env
            .clone()
    };
    assert_eq!(shard("shard-1").get("SHARD").map(String::as_str), Some("1"));
    assert_eq!(shard("shard-4").get("SHARD").map(String::as_str), Some("4"));
    assert_eq!(
        shard("shard-2").get("CI").map(String::as_str),
        Some("1"),
        "restated by the shard, so present"
    );
    assert_eq!(
        shard("shard-2").len(),
        2,
        "and nothing else arrived from the run"
    );
}

/// Jobs are not only shards of one command: they may differ in command,
/// execution mode and toolchain. The last of those is the interesting one,
/// because image inheritance has to be decided per job rather than once for
/// the run -- a host-execution job must not be handed an image it has nowhere
/// to run, and would then be rejected for a key the tenant never wrote.
#[test]
fn a_job_may_differ_in_command_mode_and_toolchain() {
    let m = load_ok("test/valid/jobs-that-differ.toml");
    let g = &m.guests[0];
    let build_image = g.build.as_ref().unwrap().image.clone();
    assert!(build_image.is_some());

    let unit = g.job(Some("unit")).expect("unit");
    assert_eq!(unit.cmd, "make check", "inherited from the run");
    assert_eq!(unit.exec, Exec::Container);
    assert_eq!(unit.image, build_image, "and the build's toolchain with it");

    let integration = g.job(Some("integration")).expect("integration");
    assert_eq!(integration.cmd, "make integration");
    assert!(integration.image.as_deref().unwrap().contains("driver"));

    let e2e = g.job(Some("e2e")).expect("e2e");
    assert_eq!(e2e.exec, Exec::Host, "overrode the guest's mode");
    assert_eq!(
        e2e.image, None,
        "a host-execution job has nowhere to run an image, so it must not \
         inherit one -- not even one its siblings are using"
    );
}

/// The backwards-compatibility claim, stated once on its own rather than only
/// as a side effect of `sole_job`: a manifest written before jobs existed
/// resolves to one job, it carries no name, and it carries the run's command.
#[test]
fn a_run_declaring_no_jobs_resolves_to_exactly_one_unnamed_job() {
    let m = load_ok("test/valid/minimal.toml");
    let g = &m.guests[0];
    let j = sole_job(g);
    assert!(!j.cmd.is_empty());
    assert!(
        g.job(None).is_some(),
        "and it is reachable as the implicit job"
    );
    assert!(
        g.job(Some("")).is_none(),
        "which is not the same as a job named with the empty string"
    );
}

/// A guest's override of `run` replaces its keys whole, `jobs` included. Two
/// guests may therefore shard differently -- a slow platform into more pieces
/// than a fast one -- and a guest that restates `jobs` does not accumulate the
/// top level's.
#[test]
fn a_guests_jobs_replace_the_top_levels() {
    let m = load_str(
        r#"
schema = 1
project = "mixed"
guests = [
  "fast",
  { name = "slow", run = { jobs = [ { name = "a" }, { name = "b" }, { name = "c" } ] } },
]
exec = "host"

[run]
cmd = "make check"

[[run.jobs]]
name = "whole"
"#,
    );

    let fast = m.guest("fast").expect("fast");
    assert_eq!(
        fast.run.jobs.len(),
        1,
        "the guest that overrode nothing keeps the top level's single job"
    );
    assert_eq!(fast.run.jobs[0].name.as_deref(), Some("whole"));

    let slow = m.guest("slow").expect("slow");
    let names: Vec<Option<&str>> = slow.run.jobs.iter().map(|j| j.name.as_deref()).collect();
    assert_eq!(
        names,
        vec![Some("a"), Some("b"), Some("c")],
        "replaced whole: the top level's job is not among them"
    );
    for j in &slow.run.jobs {
        assert_eq!(
            j.cmd, "make check",
            "though every other key still falls back to the run"
        );
    }
}

/// Two jobs cannot share a name, and the reason is worth keeping next to the
/// test: the name is the session's and the results directory's, so a duplicate
/// is two machines writing one directory with the second entry losing
/// everywhere downstream. uniqueItems cannot see it once the bodies differ.
#[test]
fn two_jobs_cannot_share_a_name() {
    let e = from_str(
        r#"
schema = 1
project = "collide"
guests = ["g"]
exec = "host"

[run]
cmd = "make check"

[[run.jobs]]
name = "slow"
cmd = "make slow"

[[run.jobs]]
name = "slow"
cmd = "make slower"
"#,
        "<inline>",
    )
    .expect_err("must be refused");

    let text = e.to_string();
    assert!(
        text.contains("more than once"),
        "the complaint must name the problem: {text}"
    );
    assert!(text.contains("slow"), "and the job it is about: {text}");
}

/// A run that names its own image keeps it. Inheritance is a fallback, not an
/// overwrite -- the opposite would silently ignore what the tenant wrote.
#[test]
fn a_run_that_names_an_image_keeps_it() {
    let text = r#"
schema = 1
project = "two-images"
guests = ["g"]
exec = "container"

[build]
image = "docker.io/library/builder@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
cmd = "make"

[run]
image = "docker.io/library/driver@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
cmd = "make check"
"#;
    let m = from_str(text, "<inline>").expect("valid");
    assert!(sole_job(&m.guests[0])
        .image
        .as_deref()
        .unwrap()
        .contains("driver"));
}

/// Sync exclusions are read and handed on uninterpreted. The framework does not
/// know what any of these patterns mean, and must not learn.
#[test]
fn sync_exclusions_are_read_verbatim() {
    let m = load_ok("test/valid/sync-excludes.toml");
    assert_eq!(m.sync_exclude, vec!["/target/", "*.tmp", ".venv/"]);

    // A manifest with no sync block excludes nothing of its own.
    assert!(load_ok("test/valid/minimal.toml").sync_exclude.is_empty());
}

/// A verb cannot ask for container execution when no image exists to run in --
/// including the case where there is no build block to inherit one from.
#[test]
fn a_container_verb_with_no_image_anywhere_is_refused() {
    let e = load_err("test/invalid/container-run-without-any-image.toml");
    let Error::Invalid { problems, .. } = &e else {
        panic!("expected Invalid, got {e}");
    };
    assert!(
        problems
            .iter()
            .any(|p| p.contains("/run") && p.contains("image")),
        "the failure should name the run's missing image: {problems:?}"
    );
}

// ---------------------------------------------------------------------------
// Hardening: defects found by adversarial review. Every test here was watched
// failing against the code as first written.
// ---------------------------------------------------------------------------

#[test]
fn the_same_guest_in_two_spellings_is_refused() {
    let e = load_err("test/invalid/duplicate-guest.toml");
    let Error::Invalid { problems, .. } = &e else {
        panic!("a doubled guest is the tenant's mistake, not ours: {e}");
    };
    assert!(
        problems.iter().any(|p| p.contains("more than once")),
        "{problems:?}"
    );
}

#[test]
fn cache_names_that_mangle_to_one_variable_are_refused() {
    let e = load_err("test/invalid/cache-name-collision.toml");
    let Error::Invalid { problems, .. } = &e else {
        panic!("colliding caches are the tenant's mistake, not ours: {e}");
    };
    assert!(
        problems.iter().any(|p| p.contains("REAPER_CACHE_MY_CACHE")),
        "the refusal must name the variable both would become: {problems:?}"
    );
}

#[test]
fn an_image_without_a_real_registry_host_is_refused() {
    // A hub namespace is not a host, and uppercase paths fail at pull time.
    load_err("test/invalid/hostless-image.toml");
    load_err("test/invalid/uppercase-image-path.toml");
    // The forms real sites use still pass.
    load_ok("test/valid/per-verb-exec.toml");
}

#[test]
fn a_manifest_that_states_exec_only_per_verb_is_complete() {
    // Both verbs carry their mode; requiring an unread guest-level default on
    // top refused coherent manifests.
    let m = load_str(
        r#"
schema = 1
project = "verbwise"
guests = ["some-guest"]
build = { exec = "host", cmd = "make deps" }
run = { exec = "host", cmd = "make check" }
"#,
    );
    let g = &m.guests[0];
    assert_eq!(g.exec, None, "no default was stated, and none is invented");
    assert_eq!(g.build.as_ref().unwrap().exec, Exec::Host);
    assert_eq!(sole_job(g).exec, Exec::Host);
}

/// Load from a string by way of a scratch file, for shapes small enough that
/// a fixture file would just put distance between the test and its data.
fn load_str(text: &str) -> Manifest {
    let dir = Scratch::new("test");
    let path = dir.file("m.toml", text);
    load(&path).unwrap_or_else(|e| panic!("should load: {e}"))
}

// ---------------------------------------------------------------------------
// Fresh battery: validation the adversarial review found no test exercising.
// ---------------------------------------------------------------------------

/// Like load_str, for text that must be refused.
fn load_str_err(text: &str) -> Vec<String> {
    let dir = Scratch::new("err");
    let path = dir.file("m.toml", text);
    match load(&path) {
        Ok(_) => panic!("should have been refused:\n{text}"),
        Err(Error::Invalid { problems, .. }) => problems,
        Err(e) => panic!("expected Invalid, got {e}"),
    }
}

#[test]
fn two_scratch_dirs_in_one_process_do_not_share_a_directory() {
    // The deterministic form of a race that only showed under cargo's
    // threads. The old helper keyed the directory on `text.len()`, so same
    // length meant same directory; here the two bodies are deliberately the
    // same length. Holding the first alive while the second is built and
    // dropped reproduces the damage without needing two threads to interleave
    // -- `remove_dir_all` in Drop took the other case's file with it.
    let keep = Scratch::new("collide");
    let kept = keep.file("m.toml", "aaaa");
    assert!(kept.exists());

    {
        let other = Scratch::new("collide");
        let _ = other.file("m.toml", "bbbb");
        assert_ne!(
            keep.0, other.0,
            "two calls must not name one directory, whatever the text"
        );
    }

    assert!(
        kept.exists(),
        "another scratch's drop removed this one's file: {}",
        kept.display()
    );
}

const SKELETON: &str =
    "\nschema = 1\nproject = \"p\"\nguests = [\"g\"]\nexec = \"host\"\nrun = { cmd = \"make\" }\n";

#[test]
fn profile_names_and_env_keys_are_held_to_their_patterns() {
    let p = load_str_err(&format!(
        "{SKELETON}[profiles.\"Bad Name\"]\nwarm_cache = false\n"
    ));
    assert!(p.iter().any(|m| m.contains("Bad Name")), "{p:?}");

    let p = load_str_err(&format!("{SKELETON}profiles = {{}}\n"));
    assert!(
        !p.is_empty(),
        "an empty profiles table is dead weight: {p:?}"
    );

    let p = load_str_err("\nschema = 1\nproject = \"p\"\nguests = [\"g\"]\nexec = \"host\"\nrun = { cmd = \"make\", env = { \"1BAD\" = \"x\" } }\n");
    assert!(p.iter().any(|m| m.contains("1BAD")), "{p:?}");

    let p = load_str_err("\nschema = 1\nproject = \"p\"\nguests = [\"g\"]\nexec = \"host\"\nrun = { cmd = \"make\", env = { GOOD = 42 } }\n");
    assert!(
        p.iter().any(|m| m.contains("env") || m.contains("GOOD")),
        "an env value that is not a string is a refusal, not a coercion: {p:?}"
    );
}

#[test]
fn resource_bounds_hold_for_every_resource() {
    for bad in [
        "resources = { cores = 0 }",
        "resources = { ram_gb = 0 }",
        "resources = { disk_gb = 0 }",
    ] {
        let p = load_str_err(&format!("{SKELETON}{bad}\n"));
        assert!(!p.is_empty(), "{bad} should be refused");
    }
}

#[test]
fn resolution_fallout_stays_quiet_when_the_structure_is_already_wrong() {
    // Pass two only runs when pass one is clean; reporting resolution fallout
    // on top of the structural break that caused it buries the error that
    // matters. This input breaks both ways -- cores: 0 structurally, and a
    // nameless guest object that resolution would refuse -- so the test can
    // tell "suppressed" from "had nothing to say".
    let p = load_str_err(
        "\nschema = 1\nproject = \"p\"\nguests = [{ resources = { cores = 0 } }]\nexec = \"host\"\nrun = { cmd = \"make\" }\n",
    );
    assert!(p.iter().any(|m| m.contains("cores")), "{p:?}");
    assert!(
        !p.iter().any(|m| m.contains("without a name")),
        "the structural error is the one that matters: {p:?}"
    );
}
