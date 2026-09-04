//! The small amount of process handling the heartbeat needs.

/// Is a process with this identifier alive?
///
/// Signal 0 performs the permission and existence checks without delivering
/// anything, which is the standard way to ask -- but what it actually answers
/// is "is there an entry in the process table", and a process that has exited
/// keeps one until its parent reaps it. Those are the same question only when
/// something reaps promptly.
///
/// For the heartbeat they are not. It is orphaned by design -- `up` spawns it,
/// never waits, and exits -- so the parent it inherits is whatever init the
/// machine has, and how quickly that reaps is not something reaper gets to
/// assume: a workstation's init does it the instant the process dies, a
/// container's pid 1 is often a shell that never reaps at all. A heartbeat
/// that has exited has stopped renewing whether or not anyone has collected
/// the corpse, so counting it alive is wrong in both directions -- it is why
/// `stop` reported a process it had just killed as unstoppable, and why `up`
/// would reuse a session whose dead-man's switch had already fired.
///
/// It still cannot distinguish "gone" from "alive but not ours"; for a process
/// this one started, that difference does not arise.
pub fn is_alive(pid: u32) -> bool {
    // Safety: kill with signal 0 delivers nothing and only inspects.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    !is_zombie(pid)
}

/// Has this process terminated, leaving only an unreaped entry behind?
///
/// Asked through `ps` for the reason `looks_like_heartbeat` gives: it is the
/// one spelling that works on both systems reaper's CLI runs on, where /proc
/// is Linux-only and not always mounted even there. The state field is `Z` on
/// both, sometimes with flags after it, so match the first letter rather than
/// the whole string.
///
/// If `ps` cannot answer, the honest reply is "not known to be a zombie",
/// which leaves `is_alive` exactly as conservative as it was before this
/// existed -- a process is never declared dead on the strength of a missing
/// answer.
fn is_zombie(pid: u32) -> bool {
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "state="])
        .output()
    {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim().starts_with('Z')
        }
        _ => false,
    }
}

/// Ask a process to stop, and confirm it did.
///
/// Polite first: the heartbeat has nothing to clean up, but a process that is
/// given no chance to exit is a process whose logs end mid-sentence. Escalates
/// only if the request is ignored.
pub fn stop(pid: u32) -> bool {
    if !is_alive(pid) {
        return true;
    }
    // Safety: sending a signal to a process identifier this process recorded.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };

    for _ in 0..20 {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    std::thread::sleep(std::time::Duration::from_millis(50));
    !is_alive(pid)
}

/// Is this pid still the heartbeat that was recorded, or has the OS reused it?
///
/// `Some(args.contains("heartbeat"))` when the process exists and its command line says heartbeat,
/// `Some(false)` when it exists and says something else, `None` when it is
/// gone. Asked through `ps`, which is the one spelling that works on both
/// systems reaper's CLI runs on; if `ps` itself cannot answer, the honest
/// reply is "not verifiably ours", never "go ahead".
pub fn looks_like_heartbeat(pid: u32) -> Option<bool> {
    if !is_alive(pid) {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "args="])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let args = String::from_utf8_lossy(&o.stdout);
            Some(args.contains("heartbeat"))
        }
        // ps failing for a live pid usually means it exited between the two
        // checks; anything else still must not license a kill.
        _ => Some(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reused_pid_is_recognized_as_not_ours() {
        // This test process is alive and is not a heartbeat; killing it on
        // the strength of a recorded number would be the bug.
        assert_eq!(looks_like_heartbeat(std::process::id()), Some(false));
    }

    #[test]
    fn a_zombie_is_not_alive() {
        // A process that has exited but has not been reaped keeps its entry in
        // the process table, and kill(pid, 0) still succeeds on it. `spawn`
        // without `wait` is exactly the shape `up` leaves the heartbeat in, so
        // this test makes itself the parent that has not reaped -- which is
        // what makes the window deterministic here.
        //
        // In the wild the window's length is somebody else's decision: this
        // was found by CI, where a container's pid 1 never reaps and the
        // window is forever, after passing for months on a workstation whose
        // init closes it within microseconds. Owning the child removes that
        // difference from the test.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();

        // `true` exits immediately, but "has exited" and "is visible as
        // exited" are not the same instant.
        let mut corpse = false;
        for _ in 0..200 {
            if is_zombie(pid) {
                corpse = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            corpse,
            "the child never became a zombie, so this test cannot check what it exists to check"
        );

        // The trap, stated: the raw syscall still says the identifier exists.
        // Safety: kill with signal 0 delivers nothing and only inspects.
        assert_eq!(
            unsafe { libc::kill(pid as libc::pid_t, 0) },
            0,
            "an unreaped child should still answer kill(pid, 0)"
        );
        // And the judgement that must not be fooled by it. A heartbeat that
        // has exited is not renewing anything, reaped or not.
        assert!(
            !is_alive(pid),
            "a process that has exited is not alive, whoever has or has not reaped it"
        );

        child.wait().expect("reap");
    }

    #[test]
    fn a_dead_pid_is_gone_not_foreign() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap");
        // Reaped, so the identifier is free; the answer is "gone", which the
        // caller treats as nothing to do.
        assert_eq!(looks_like_heartbeat(pid), None);
    }
}
