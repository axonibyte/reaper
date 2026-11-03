#!/bin/sh
#
# Seam guards.
#
# reaper is meant to host any program, on any guest, under any hypervisor. That
# is only true for as long as no tenant, operating system or hypervisor has
# leaked out of the place it belongs -- and the leak is never a decision anyone
# announces. It is a one-line special case at 2am that works.
#
# So the seams are checked mechanically, in the source-as-data spirit of the
# testing methodology's Tier 3: parse the project's own source and assert
# structural claims about it. Milliseconds, no build, no network.
#
# Usage: guards.sh [tenant|guest|provider|forge]   (no argument runs all four)
#
# Exit 0 if every seam holds, 1 otherwise.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "${root}"

failures=0

# scan <label> <allow-regex> <forbid-regex> <why> [blind-sed]
#
# Reports every file outside the allowed paths that matches the forbidden
# pattern. Untracked files are scanned too: a new source file is untracked for
# exactly the window in which this gate is its only reviewer. Build output
# stays out via .gitignore (--exclude-standard).
#
# blind-sed, when given, is applied to each file before the pattern is tried.
# It exists for the forge seam, where one adapter's name is also a substring of
# the CI directory's name, so that the guard can be blinded to that one exact
# spelling rather than to whole directories. It is line-preserving, so the
# reported line numbers are still the file's own. Note what it cannot do: it is
# a plain substitution, so blinding it to a spelling blinds it everywhere that
# spelling appears, including inside a genuine leak that happened to contain
# it.
scan() {
    label=$1
    allow=$2
    forbid=$3
    why=$4
    blind=${5:-}

    hits=''
    for f in $(git ls-files --cached --others --exclude-standard | grep -Ev "${allow}"); do
        # Binary files and deleted-but-tracked paths are skipped rather than
        # reported as errors; neither is source anyone can leak through.
        [ -f "${f}" ] || continue
        if [ -n "${blind}" ]; then
            text=$(sed "${blind}" "${f}" 2>/dev/null || true)
        else
            text=$(cat "${f}" 2>/dev/null || true)
        fi
        if match=$(printf '%s\n' "${text}" | grep -nIiE "${forbid}"); then
            hits="${hits}$(printf '%s\n' "${match}" | sed "s|^|  ${f}:|")
"
        fi
    done

    if [ -n "${hits}" ]; then
        failures=$((failures + 1))
        echo "FAIL  ${label}"
        echo "${why}" | sed 's/^/      /'
        printf '%s' "${hits}"
        echo
    else
        echo "ok    ${label}"
    fi
}

guard_tenant() {
    # The forbidden names are read out of the examples rather than written
    # here, which is why this guard needs no exemption for itself: it does not
    # contain the words it polices. Add an example, and it is policed from then
    # on with no edit to this file.
    names=$(sed -n 's/^project *= *"\([^"]*\)".*/\1/p' manifest/examples/*.reaper.toml \
            | tr '\n' '|' | sed 's/|$//')
    if [ -z "${names}" ]; then
        echo "FAIL  tenant seam"
        echo "      no project names found in manifest/examples; this guard is"
        echo "      not checking anything, which is worse than it failing."
        failures=$((failures + 1))
        return
    fi

    # .forgejo/ is allowed, 2026-10-26: the workflow's own header explains why
    # it runs where it does by naming the sibling repositories that share the
    # runner, and it is vendor CI configuration that no reaper code reads. The
    # cost is stated below, at the guest seam, once for all three seams.
    scan "tenant seam" \
        '^(docs/|manifest/examples/|\.forgejo/)' \
        "\\b(${names})\\b" \
        "A tenant name appeared in framework code. Tenants are configuration:
they belong in their own .reaper.toml and in manifest/examples, and
nowhere else. If the framework needs to know which project it is
working on, the manifest is how it finds out."
}

guard_guest() {
    # Operating-system specifics belong in the runner's platform modules, where
    # they are declared as such, and in documentation. Everywhere else they are
    # an assumption that only one guest exists.
    #
    # manifest/examples is allowed because guest *names* are tenant-chosen
    # strings that quite reasonably say which system they mean.
    #
    # runner/ is allowed because it *is* the platform module -- the one place
    # operating-system specifics are supposed to live. This used to say
    # runner/platform/, from when the runner was going to be a compiled binary
    # with a platform sub-module; it is a single shell script delivered over
    # SSH, so the directory is the boundary.
    #
    # .forgejo/ and ci/ are allowed because they name *build hosts*, not
    # guests, and the two are unrelated questions. This seam exists so that
    # adding a guest system is a template build and a registry entry rather
    # than a code change; a release matrix saying which machines compile the
    # binary cannot make that false, and no reaper code reads these files.
    #
    # .forgejo/ replaced bitbucket-pipelines.yml here on 2026-10-26, when CI
    # moved to the forge and that file was deleted, and it is allowed by the
    # tenant and provider seams as well, which the Bitbucket file was not: the
    # workflow's header explains why the job runs on the host rather than in
    # the jail, and that explanation names the hypervisor whose API the suite
    # stands in for and the sibling repositories that share the runner. Be
    # honest about the cost, as the provider seam is below: the directory is
    # skipped wholesale, so a workflow that reached for the hypervisor's API
    # itself would pass these seams and has to be caught in review. The
    # alternative -- a workflow that may not say why it is shaped the way it
    # is -- was judged worse. Cal's call.
    #
    # This replaces the .github/workflows/ exemption rather than adding to it.
    # That directory was deleted when the repository moved to Bitbucket, and
    # the release matrix it used to hold now lives in these two places: the
    # pipeline names the targets, ci/build-target.sh knows what each one needs
    # to link. An exemption kept alive after its files are gone is a place
    # coupling can hide, so it went with them.
    #
    # Narrow deliberately: two directories. The tenant and provider seams
    # below still apply to ci/ in full -- a build script naming a tenant, or
    # reaching for a hypervisor's API, is still a failure. .forgejo/ is the
    # exception, explained above.
    scan "guest seam" \
        '^(docs/|README\.md$|manifest/examples/|\.reaper\.toml$|runner/|tools/guards\.sh$|\.forgejo/|ci/)' \
        'systemctl|systemd|apt-get|dpkg |zfsutils-linux|/dev/(vd|vtbd|sd|nvme)[a-z0-9]|rc\.conf|\brc\.d\b|\b(ubuntu|debian|freebsd|alpine|centos|fedora)\b' \
        "An operating system leaked out of the runner's platform modules. The
guest seam exists so that adding a system is a template build and a
registry entry, never a code change -- and a single hardcoded device
path or init command is enough to make that false."
}

guard_forge() {
    # Forge specifics belong to an adapter and to the registry that selects
    # one. Everywhere else they are an assumption that only one forge exists.
    #
    # The policed names are read out of the registry's own list rather than
    # written here, which is why this guard needs no exemption for itself: it
    # does not contain the words it polices. Add an adapter to AVAILABLE and it
    # is policed from then on with no edit to this file.
    #
    # Note what this does NOT police: the name of a CI vendor or a code host
    # that this build has no adapter for. Where the repository happens to live,
    # and which machines compile it, are different questions from which forges
    # `auto` can drive -- and conflating them would have meant exempting
    # .gitignore, ci/ and the manifest schema wholesale, which is a wider hole
    # than the one it closed.
    #
    # .forgejo/ is exempt as it is at the other three seams: it is vendor CI
    # configuration whose directory name is the vendor's choice, and no reaper
    # code reads it.
    #
    # And one spelling is BLINDED rather than exempted, because that directory
    # name happens to contain an adapter's name: every mention of the CI
    # directory elsewhere in the tree -- a build script saying where the
    # workflow went, an invariant anchored on it -- would otherwise read as a
    # leak. Exempting the files that mention it (ci/, tools/, .gitignore, the
    # manifest schema) would have been the easy fix and a far wider hole; this
    # blinds the scan to those literals and to nothing else -- both of them,
    # since the tree's history names the previous host's directory too.
    #
    # tools/ is exempt, and this is the one exemption here worth arguing about.
    # These are the gate scripts: they hold no reaper code, link against
    # nothing, and cannot carry a forge dependency in any form -- and the only
    # forge-shaped words in them name the CI SYSTEM, which happens to be
    # spelled the same as an adapter. The cost is that a gate script could
    # reach for a forge's API and pass this seam, which has to be caught in
    # review. The alternative was a gate script that may not say which CI
    # product it is accommodating, which is worse.
    names=$(sed -n 's/^pub const AVAILABLE: &\[&str\] = &\[\(.*\)\];.*/\1/p' \
            forges/registry/src/lib.rs | tr -d '"' | tr -d ' ' | tr ',' '|' | sed 's/|$//')
    if [ -z "${names}" ]; then
        echo "FAIL  forge seam"
        echo "      no forge names found in the registry; this guard is not"
        echo "      checking anything, which is worse than it failing."
        failures=$((failures + 1))
        return
    fi

    scan "forge seam" \
        '^(docs/|README\.md$|Cargo\.toml$|Cargo\.lock$|forges/|\.forgejo/|tools/)' \
        "\\b(${names})\\b" \
        "A forge name appeared outside the adapters. Which forge hosts a
repository is configuration, and which adapter drives it is the
registry's one decision -- so the name belongs in forges/ and in a
site configuration file, and nowhere else. A test that needs a forge
takes the stand-in from the registry, which names none." \
        's|\.forgejo|CI-DIRECTORY|g; s|\.github|CI-DIRECTORY|g'
}

guard_provider() {
    # Hypervisor vocabulary belongs to a provider and its sweeper.
    #
    # Cargo.toml and Cargo.lock are allowed because a workspace root names its
    # members and a lockfile enumerates them: a member called
    # reaper-provider-proxmox appearing there is the seam working rather than
    # leaking. Be honest about what the exemption costs: these files are skipped
    # wholesale, not scanned for names only. It is acceptable because neither
    # carries code, and Cargo.lock is generated rather than written -- so the
    # exemption cannot hide a decision a person made. README.md is documentation.
    #
    # Note what is *not* exempt: the CLI, tests included. Driving the CLI end to
    # end needs a hypervisor, but it does not need a named one -- the provider
    # registry re-exports a stand-in neutrally, so no exemption is required. An
    # exemption is a place coupling can hide; not needing one is better.
    scan "provider seam" \
        '^(docs/|README\.md$|Cargo\.toml$|Cargo\.lock$|providers/|cull/|tools/guards\.sh$|\.forgejo/)' \
        '\bpve\b|\bvmid\b|\bupid\b|proxmox|pveapitoken|\b(9000|9099)\b' \
        "Hypervisor vocabulary leaked into the core. Machine identifiers,
identifier ranges, resource pools, task handles and API tokens belong
behind the provider trait. Note what this guard can and cannot do: it
proves the boundary has not been crossed, not that the boundary is in
the right place. Only a second provider would prove that."
}

case "${1:-all}" in
    tenant)   guard_tenant ;;
    guest)    guard_guest ;;
    provider) guard_provider ;;
    forge)    guard_forge ;;
    all)      guard_tenant; guard_guest; guard_provider; guard_forge ;;
    *)        echo "usage: $0 [tenant|guest|provider|forge]" >&2; exit 2 ;;
esac

[ "${failures}" -eq 0 ]
