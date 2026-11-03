# `reaper auto`: the unattended loop

```sh
reaper auto [PATH]
```

Watch a forge. When a review request is open and has not yet been judged at its
current commit, fetch that commit into the clone, run the battery against it,
and then either land it or say on the request why not. Then do it again.

`PATH` is a local clone and defaults to the working directory.

## What one pass does

1. **List what is open**, oldest first. Oldest rather than newest so that a
   request which keeps losing the race does not starve.
2. **Skip anything already judged.** A verdict is a comment from reaper's own
   identity naming the commit it judged; if the current head is named, the
   request is left alone.
3. **Fetch and check out** the request's head, detached. Refused outright if
   the tree has uncommitted changes.
4. **Make sure a session exists.** `up` is idempotent against a live one; if
   the record names a machine that is gone, the session is destroyed and
   remade.
5. **Run the battery** as a child process, re-reading the request's head
   throughout.
6. **Act on the verdict**: green rebases and merges, red comments with the tail
   of the log. Either way the comment names the commit, so the next pass knows
   this one has been answered.

With `--once` it stops after one pass, which is the shape for a cron entry.
Without it, it sleeps `--poll` between passes and runs until interrupted.

## Why a comment is the record, and not a file

The obvious way to remember what has been judged is a file somewhere. This uses
a comment on the request instead, naming the commit:

```
reaper verdict for 0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c
```

Three things fall out of that and none of them is a coincidence. There is no
local state to go stale, be lost with the machine, or disagree with the forge --
so two people can run `auto` against the same repository and neither will
retest what the other has answered. The record is visible to the people whose
request it is, rather than being in a directory they have no reason to look in.
And a verdict survives reinstalling the tool.

The rule is "has **this commit** been judged", not "has anyone spoken". A
timestamp comparison would have been shorter and is wrong twice over: forge
timestamps are second-granular, so a commit pushed in the same second as the
comment would be skipped forever; and a force-push of an *older* commit has a
commit date earlier than the comment, so a genuinely new head would look
judged. It also means an ordinary human comment from the same account does not
silence the loop.

## The three windows a commit can land in

A commit arriving mid-pass is the normal case, not an edge case, and each
window is handled differently.

**Between the listing and the fetch.** The fetched commit will not be the one
that was listed. Noticed by comparing them, and the pass starts again.

**During the battery.** The head is re-read every `--poll`; a change kills the
battery. The session is then **destroyed rather than reused**: a sync cut off
part way through leaves a half-written tree in the guest, and the next run on
that machine fails for reasons that have nothing to do with the change under
test. That is the costly part of an interruption and the reason it is worth
noticing early.

**Between the battery and the merge.** The window a poll cannot cover. The head
is read once more immediately before merging, and a change throws the verdict
away. Without this the loop would land a commit it never tested, which is the
worst thing this verb could do.

## Configuration

Which adapter drives a repository is worked out from the git remote, and
`--forge` overrides it. An adapter's credentials live in site configuration:

```toml
[forges.github]
# Nothing is required: the GitHub adapter drives the gh CLI, which already
# holds the credential. Only override the command if yours is wrapped.
command = "gh"

[forges.forgejo]
# Where the API is, if not the git remote's own host.
url = "https://forge.example.com"
# Which repository, if the remote does not say -- a filesystem remote, say.
repo = "owner/name"
# A token, or a user with a password, or a user with a command that prints one.
token = "..."
# user = "someone"
# password_command = "pass show forge/someone"
```

Prefer `password_command` to `password`. A secret in a configuration file is
the kind of convenience that is only noticed once.

## What it will never do

**Decide what green means.** The battery's exit status is the verdict, whole.
There is no notion here of an acceptable failure, a flaky step or a retry.

**Judge the same commit twice.** Not as an optimisation -- as the thing that
stops a red request being retested every poll for as long as it is open.

**Touch a dirty tree.** It will not stash, because a stash this process made is
a stash nobody is expecting to find.

**Merge anything it did not test.** See the third window above.

**Run more than one battery at a time.** A session is one machine. Requests are
taken in turn.

## Known limits

**Interrupting `auto` can orphan a battery.** `auto` runs `reaper test` as a
child so that a new commit can stop it. Kill `auto` itself and that child is
not reaped, so the battery runs on and the session stays up. Check with
`reaper list` and clear it with `reaper down`.

**The post-battery re-read is a point check, not a lock.** It closes the window
to about the time one API call takes, and the forges this drives do not offer a
way to say "merge only if the head is still X" through the same call.

**A forge that goes away mid-battery is tolerated, not handled.** The head
re-read failing is reported and the battery continues, on the grounds that a
transient failure is not worth throwing away a battery for. If the forge is
still unreachable when the verdict is due, that is where the pass fails -- with
the verdict in hand and nothing recorded.
