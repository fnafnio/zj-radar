//! The bounded-send argv for the status broadcast — how every producer should
//! actually spawn `zellij pipe`.
//!
//! `zellij pipe` is a backpressure channel: the client process is held until
//! **every** loaded plugin instance consumes the message, and an instance
//! wedged at Zellij's permission prompt holds it forever. Producers fire at
//! hook rate (one send per tool call), and each blocked client pins two FDs in
//! the Zellij *server*, so an unbounded send turns one wedged rail into an
//! EMFILE crash of the whole session. Hence the send deadline.
//!
//! The deadline cannot live only in the producer process: hook runners kill
//! their hooks, and a producer killed mid-send never runs its kill-on-deadline
//! — the blocked `zellij pipe` child re-parents to init and leaks forever
//! (observed in production as orphaned clients pinning server FDs for hours).
//! This argv makes the spawned subtree limit **itself**: a detached sleep+kill
//! watchdog rides inside the same `sh`, so the hung client is reaped no matter
//! what happens to the process that spawned it. Killing a client past the
//! deadline retracts nothing — the message is queued server-side the moment it
//! is sent — so latest-wins ordering holds.

use crate::payload::STATUS_PIPE_NAME;

/// Default send deadline in whole seconds — orders of magnitude above a
/// healthy send (milliseconds) yet caps a wedged one at hook rate. The CLI's
/// `ZJ_RADAR_PIPE_TIMEOUT` override falls back to this; the plugin (which has
/// no environment) uses it directly.
pub const DEFAULT_PIPE_TIMEOUT_SECS: u64 = 5;

/// Send deadline for `running` heartbeats, which ride the hottest hooks —
/// per tool call, twice (Pre + Post). Deliberately shorter than
/// [`DEFAULT_PIPE_TIMEOUT_SECS`]: an expired `running` is a dropped heartbeat
/// the next tool event replaces, so against a wedged rail the hot path stalls
/// ~2 s per hook instead of ~5, while the once-per-turn edges
/// (`done`/`pending`/`idle`) keep the full default — dropping one of those
/// loses real state (a lost `done` sticks the spinner with no later event to
/// clear it). 2 s rather than 1: under full-parallel test load a 1 s deadline
/// lost the race to fork/exec alone (see the shim comment in cli_notify.rs),
/// and a `running` is not always pure heartbeat — PostToolUse is the
/// Pending→Running recovery edge after an answered permission prompt, and
/// UserPromptSubmit carries the sticky task label.
pub const RUNNING_PIPE_TIMEOUT_SECS: u64 = 2;
// "Deliberately shorter" is the invariant, pinned at compile time: the hot
// heartbeat deadline must stay below the once-per-turn edge deadline.
const _: () = assert!(RUNNING_PIPE_TIMEOUT_SECS < DEFAULT_PIPE_TIMEOUT_SECS);

/// Seconds a `Running` pane may sit at a shell prompt before the plugin
/// declares its pushed status stale and clears it to idle (the plugin counts
/// them as Fast-cadence ticks, one per second). Long enough that a mid-turn
/// foreground flicker — which re-asserts the agent's foreground or a fresh
/// hook payload well inside the window — never trips it; short enough that
/// killing an agent mid-turn doesn't leave a "working" row spinning forever.
///
/// Lives here, not in the plugin's status store, because it is half of a
/// producer ↔ plugin contract: ANY payload for the pane cancels the clock,
/// so a producer that suppresses identical `running` heartbeats
/// (`crates/cli/src/dedup.rs`) must go quiet for strictly less than this,
/// or a live agent's row clears to idle while it works. The CLI pins that
/// ordering at compile time against this constant.
pub const RUNNING_QUIET_MAX_SECS: u64 = 15;

/// Cap on hook stdin read by a producer before JSON parsing — the shared
/// contract between the CLI producer's bounded read and the bash fallback's
/// `head -c`. Hook payloads are small JSON (well under a megabyte); the cap
/// only bounds a runaway or hostile writer. Lives beside the send deadlines
/// because it is the same family: a producer limiting itself so a misbehaving
/// peer cannot wedge or bloat the hook path. This is the *input* cap, distinct
/// from the plugin's 64 KB *wire* cap on the broadcast payload the producer
/// derives from that input.
pub const MAX_STDIN_BYTES: u64 = 8 * 1024 * 1024;

// $1 = deadline seconds, $2 = pipe name, $3 = payload — positional parameters,
// never interpolated into the script (same no-escaping rule as the plugin's
// `notify_command`), so an arbitrary payload cannot break out of the command.
//
// Accepted residuals, shared with notify.sh's inline copy of this idiom (keep
// the two in sync):
//  - The watchdog's expiring `kill` could in principle target a recycled pid;
//    the disarm after a normal send narrows that window to microseconds.
//  - Disarming kills only the watchdog subshell: its orphaned `sleep` lingers
//    ≤ $1 seconds after every healthy send and exits without acting (the kill
//    line is unreachable once the subshell is gone) — accepted `ps` noise.
//  - The kill is a single SIGTERM to the direct child: sufficient because
//    `zellij pipe` is one process with the default TERM disposition (it execs,
//    forks no helpers, traps nothing). A client that ignored TERM or hid the
//    blocked process behind a child would outlive the watchdog.
//  - If the watchdog's own fork fails (process-table exhaustion), the client
//    runs unbounded: forks 1-2 succeeding while fork 3 fails is a double-
//    failure corner. The total fix — spawning the subtree in its own process
//    group and group-killing from the producer's backstop — isn't worth the
//    platform surface yet.
//
// The wrapper exits with the CLIENT's status (`wait "$p"` yields it: 0 on a
// delivered send, non-zero when zellij failed, 128+SIGTERM when the watchdog
// killed it). The CLI producer keys its last-sent dedup on that: only a
// confirmed delivery may be recorded, or a killed-at-deadline `running` would
// suppress its own retries for the dedup TTL. Callers that don't care (the
// plugin's ack echo via `run_command`) ignore it.
const SELF_LIMITING_SEND: &str = concat!(
    "zellij pipe --name \"$2\" -- \"$3\" >/dev/null 2>&1 & p=$!; ",
    "( sleep \"$1\"; kill \"$p\" ) >/dev/null 2>&1 & w=$!; ",
    "wait \"$p\" 2>/dev/null; s=$?; kill \"$w\" 2>/dev/null; exit \"$s\"",
);

/// Directories Git for Windows installs its bundled MSYS `sh.exe` under,
/// relative to the Git install root. Neither is on `PATH` by default.
const WINDOWS_GIT_SH_SUFFIXES: &[&str] = &["bin\\sh.exe", "usr\\bin\\sh.exe"];

/// Env vars that may hold a Git-for-Windows install root: per-machine
/// installs first, then the per-user root (handled separately in
/// `resolve_sh` since it needs an extra `Programs\Git` segment).
const WINDOWS_GIT_ROOT_VARS: &[&str] = &["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"];

/// Pure candidate-path generation for the Windows `sh` fallback: given
/// whatever Git-for-Windows install roots were found in the environment,
/// produce the absolute `sh.exe` paths to probe, in priority order. Split
/// out from `resolve_sh` (which owns the env/fs I/O) so this is host-testable
/// without touching the real environment or filesystem.
fn windows_git_sh_candidates(
    machine_roots: &[&std::ffi::OsStr],
    local_app_data: Option<&std::ffi::OsStr>,
) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for root in machine_roots {
        for suffix in WINDOWS_GIT_SH_SUFFIXES {
            out.push(std::path::Path::new(root).join("Git").join(suffix));
        }
    }
    if let Some(local) = local_app_data {
        for suffix in WINDOWS_GIT_SH_SUFFIXES {
            out.push(std::path::Path::new(local).join("Programs").join("Git").join(suffix));
        }
    }
    out
}

/// Resolve the interpreter argv[0] for the self-limiting send. Unix hosts
/// always have `sh` on `PATH`. On Windows, `sh` alone is often not
/// resolvable even with Git for Windows installed, which would silently fail
/// every status push. PATH is checked first, then the well-known
/// Git-for-Windows install roots; if nothing is found, the bare name is
/// returned unchanged (same "not found" spawn failure as before).
fn resolve_sh() -> String {
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if dir.join("sh").is_file() || dir.join("sh.exe").is_file() {
                return "sh".to_string();
            }
        }
    }
    let machine_roots: Vec<std::ffi::OsString> = WINDOWS_GIT_ROOT_VARS
        .iter()
        .filter_map(|var| std::env::var_os(var))
        .collect();
    let machine_roots: Vec<&std::ffi::OsStr> = machine_roots.iter().map(std::ffi::OsString::as_os_str).collect();
    let local_app_data = std::env::var_os("LocalAppData");
    for candidate in windows_git_sh_candidates(&machine_roots, local_app_data.as_deref()) {
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }
    "sh".to_string()
}

/// Argv for one self-limiting status broadcast: spawn it and the subtree
/// guarantees its own exit within `timeout_secs` (plus scheduling slack),
/// even if the spawner dies first. The script itself is POSIX `sh`, but
/// argv[0] is resolved (`resolve_sh`) rather than the bare literal `"sh"` so
/// a Windows host with Git for Windows but no `sh` on `PATH` still works.
pub fn self_limiting_pipe_argv(payload: &str, timeout_secs: u64) -> Vec<String> {
    vec![
        resolve_sh(),
        "-c".to_string(),
        SELF_LIMITING_SEND.to_string(),
        "zj-radar-pipe".to_string(), // $0 — a label for ps output
        timeout_secs.to_string(),    // $1
        STATUS_PIPE_NAME.to_string(), // $2
        payload.to_string(),         // $3
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-machine root yields both `bin` and `usr\bin` candidates under
    /// `Git`. Expected paths are built with the same `Path::join` calls as
    /// the actual value (not a raw backslash literal) so this holds on every
    /// host this crate builds for, including non-Windows targets.
    #[test]
    fn windows_git_sh_candidates_covers_bin_and_usr_bin_per_machine_root() {
        let root = std::ffi::OsStr::new(r"C:\Program Files");
        let candidates = windows_git_sh_candidates(&[root], None);
        assert_eq!(
            candidates,
            vec![
                std::path::Path::new(root).join("Git").join("bin\\sh.exe"),
                std::path::Path::new(root).join("Git").join("usr\\bin\\sh.exe"),
            ]
        );
    }

    /// The per-user installer root carries one extra `Programs` segment.
    #[test]
    fn windows_git_sh_candidates_adds_the_per_user_programs_segment() {
        let local = std::ffi::OsStr::new(r"C:\Users\x\AppData\Local");
        let candidates = windows_git_sh_candidates(&[], Some(local));
        assert_eq!(
            candidates,
            vec![
                std::path::Path::new(local).join("Programs").join("Git").join("bin\\sh.exe"),
                std::path::Path::new(local).join("Programs").join("Git").join("usr\\bin\\sh.exe"),
            ]
        );
    }

    /// Multiple per-machine roots each contribute their own pair, in order,
    /// with the per-user root last.
    #[test]
    fn windows_git_sh_candidates_orders_multiple_roots_machine_before_user() {
        let a = std::ffi::OsStr::new(r"C:\Program Files");
        let b = std::ffi::OsStr::new(r"C:\Program Files (x86)");
        let local = std::ffi::OsStr::new(r"C:\Users\x\AppData\Local");
        let candidates = windows_git_sh_candidates(&[a, b], Some(local));
        assert_eq!(
            candidates,
            vec![
                std::path::Path::new(a).join("Git").join("bin\\sh.exe"),
                std::path::Path::new(a).join("Git").join("usr\\bin\\sh.exe"),
                std::path::Path::new(b).join("Git").join("bin\\sh.exe"),
                std::path::Path::new(b).join("Git").join("usr\\bin\\sh.exe"),
                std::path::Path::new(local).join("Programs").join("Git").join("bin\\sh.exe"),
                std::path::Path::new(local).join("Programs").join("Git").join("usr\\bin\\sh.exe"),
            ]
        );
    }

    /// No roots found must yield no candidates, never a panic.
    #[test]
    fn windows_git_sh_candidates_empty_when_no_roots_found() {
        assert!(windows_git_sh_candidates(&[], None).is_empty());
    }

    #[test]
    fn argv_carries_payload_and_deadline_as_positionals() {
        let argv = self_limiting_pipe_argv(r#"{"v":1,"msg":"a b; $(rm)"}"#, 5);
        // argv[0] is `resolve_sh()`'s pick — bare "sh" or a resolved
        // Windows `sh.exe` path — this test only pins the positional shape.
        assert!(argv[0] == "sh" || argv[0].ends_with("sh.exe"));
        assert_eq!(argv[1], "-c");
        // The payload rides verbatim as a positional parameter — never
        // interpolated into the script text, so no quoting/escaping exists
        // to get wrong.
        assert_eq!(argv[4], "5");
        assert_eq!(argv[5], STATUS_PIPE_NAME);
        assert_eq!(argv[6], r#"{"v":1,"msg":"a b; $(rm)"}"#);
        assert!(!argv[2].contains("rm"), "script must not embed the payload");
    }

    #[test]
    fn script_arms_a_watchdog_and_disarms_it_after_a_normal_send() {
        // Structural guard on the script itself: the deadline must ride
        // INSIDE the spawned subtree (sleep+kill against the pipe's pid),
        // not in the spawning process — that is the whole point of this
        // module (a killed producer must not orphan a blocked client).
        assert!(SELF_LIMITING_SEND.contains("sleep \"$1\""));
        assert!(SELF_LIMITING_SEND.contains("kill \"$p\""));
        assert!(SELF_LIMITING_SEND.contains("wait \"$p\""));
        assert!(SELF_LIMITING_SEND.contains("kill \"$w\""));
    }
}

/// Tests that spawn the argv for real against POSIX `sh` shims — gated to
/// `unix` since the shims are executable `#!/bin/sh` scripts.
#[cfg(all(test, unix))]
mod unix_process_tests {
    use super::*;

    /// The healthy path must not ride the watchdog: a fast send exits the
    /// wrapper immediately, well before the deadline. Guards the disarm
    /// against regressions like `wait "$p"` becoming a bare `wait` (which
    /// would block on the watchdog's sleep too) — that would stall every
    /// producer hook ~5s per tool call with the whole suite still green,
    /// since the hung-path tests only assert reaping, not latency.
    #[test]
    fn healthy_send_exits_immediately_not_at_the_watchdog_deadline() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let shim = dir.path().join("zellij");
        std::fs::write(&shim, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

        let argv = self_limiting_pipe_argv("{}", DEFAULT_PIPE_TIMEOUT_SECS);
        let mut path = dir.path().as_os_str().to_owned();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let start = std::time::Instant::now();
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("PATH", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "wrapper must exit 0 on a healthy send");
        // Milliseconds when healthy; 2s is pure loaded-CI slack, still well
        // clear of the 5s watchdog a coupled exit would wait on.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "wrapper rode the watchdog instead of exiting with the send ({}ms)",
            start.elapsed().as_millis()
        );
    }

    /// Spawn the argv with a shim dir prepended to PATH and return the
    /// wrapper's exit status. Shared by the exit-status tests below.
    fn run_wrapper(shim_dir: &std::path::Path, timeout_secs: u64) -> std::process::ExitStatus {
        let argv = self_limiting_pipe_argv("{}", timeout_secs);
        let mut path = shim_dir.as_os_str().to_owned();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("PATH", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
    }

    fn install_shim(dir: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let shim = dir.join("zellij");
        std::fs::write(&shim, body).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The wrapper's exit status IS `zellij pipe`'s: the CLI producer records
    /// a send as delivered (for its last-sent dedup) only on success, so a
    /// client that failed — no server, bad session — must not read as sent.
    #[test]
    fn wrapper_propagates_the_clients_exit_status() {
        let dir = tempfile::TempDir::new().unwrap();
        install_shim(dir.path(), "#!/bin/sh\nexit 3\n");
        let status = run_wrapper(dir.path(), DEFAULT_PIPE_TIMEOUT_SECS);
        assert_eq!(status.code(), Some(3), "wrapper must exit with the client's status");
    }

    /// A client killed by the watchdog at the deadline was never confirmed
    /// delivered (the message is queued server-side, but the producer cannot
    /// know that), so the wrapper must exit non-zero — never a silent `exit 0`
    /// that would let the producer record the payload as sent.
    #[test]
    fn wrapper_exits_nonzero_when_the_watchdog_kills_the_client() {
        let dir = tempfile::TempDir::new().unwrap();
        install_shim(dir.path(), "#!/bin/sh\nexec sleep 60\n");
        let start = std::time::Instant::now();
        let status = run_wrapper(dir.path(), 1);
        assert!(!status.success(), "a deadline-killed send must not exit 0");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "wrapper must return at the 1s deadline, not the 60s hang ({}ms)",
            start.elapsed().as_millis()
        );
    }

    /// The property the argv exists for, exercised for real: spawn it against
    /// a hanging `zellij` shim, SIGKILL the spawner immediately, and the hung
    /// child is still reaped by the in-subtree watchdog.
    #[test]
    fn subtree_reaps_a_hung_send_even_when_the_spawner_dies() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        // Hanging `zellij` shim that reports its own pid, then blocks. `exec`
        // so the reported pid IS the sleeper the watchdog must reap.
        let shim = dir.path().join("zellij");
        std::fs::write(&shim, "#!/bin/sh\necho $$ > \"$(dirname \"$0\")/pid\"\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

        let argv = self_limiting_pipe_argv("{}", 1);
        let mut path = dir.path().as_os_str().to_owned();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        let mut spawner = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("PATH", path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        // Wait for the shim to be up (pid file written), then kill the
        // spawner — the moment a real hook runner would kill the producer.
        let pid_file = dir.path().join("pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Ok(mut f) = std::fs::File::open(&pid_file) {
                let mut s = String::new();
                let _ = f.read_to_string(&mut s);
                if let Ok(pid) = s.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(std::time::Instant::now() < deadline, "shim never started");
            std::thread::sleep(std::time::Duration::from_millis(25));
        };
        let _ = spawner.kill();
        let _ = spawner.wait();

        // The orphaned subtree must still reap the hung client at its 1s
        // deadline. Poll (with slack for loaded CI) rather than sleep-once.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            let alive = std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                break; // reaped — the property holds
            }
            if std::time::Instant::now() >= deadline {
                let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
                panic!("hung pipe client leaked past the watchdog deadline after spawner death");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
