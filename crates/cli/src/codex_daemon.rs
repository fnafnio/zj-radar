//! Codex's shared app-server daemon (on by default since Codex 0.157) runs
//! every thread's hooks itself, with the environment it captured when it
//! started: the `$ZELLIJ_PANE_ID` / `$ZELLIJ_SESSION_NAME` of whichever pane
//! happened to launch it. A hook fired from there names the wrong pane (or a
//! closed one, or another session's), and nothing in the hook payload names
//! the client terminal — Codex keeps hooks client-agnostic by design
//! (openai/codex#48500, proposal #44902). So `notify codex` drops such a hook
//! rather than paint a stranger's row, and the doctor says why the rows are
//! dark (issue #71). The only reliable tell is the process tree: a daemon
//! hook descends from `codex app-server`, an in-process one from the TUI.

/// How many ancestors to inspect. Codex spawns a hook as `$SHELL -lc <cmd>`,
/// so the runner is the parent (the shell exec'd us) or the grandparent;
/// the slack covers a wrapper script in between.
const MAX_ANCESTORS: usize = 4;

/// Pure: classify one ancestor by its argv (space-joined, as `ps` prints
/// it). `None` = not a Codex process, keep walking; `Some(true)` = Codex's
/// app-server (the daemon, or any app-server — its env belongs to no pane);
/// `Some(false)` = another Codex process (the TUI, which runs hooks in-process
/// with its own pane's env). Keyed on the `app-server` SUBCOMMAND: the TUI
/// itself carries an internal `--managed-daemon` flag and paths under
/// `app-server-daemon/`, so neither substring is a tell.
pub(crate) fn classify_ancestor(args: &str) -> Option<bool> {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    // argv[0] for a native binary; argv[1] behind an interpreter (npm's
    // `node …/codex.js` launcher).
    let at = tokens.iter().take(2).position(|t| {
        let base = t.rsplit('/').next().unwrap_or(t);
        base == "codex" || base == "codex.js"
    })?;
    Some(tokens.get(at + 1) == Some(&"app-server"))
}

/// Is this process a hook run by a Codex app-server (whose env names no
/// particular pane)? Walks up to [`MAX_ANCESTORS`] parents and answers at the
/// first Codex process; false when there is none (a legacy wrapper, a manual
/// `notify codex` from a shell) — the env is then trusted as before.
pub(crate) fn hook_runs_in_app_server() -> bool {
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..MAX_ANCESTORS {
        if pid <= 1 {
            return false;
        }
        let Some((ppid, args)) = proc_info(pid) else {
            return false;
        };
        if let Some(app_server) = classify_ancestor(&args) {
            return app_server;
        }
        pid = ppid;
    }
    false
}

/// The daemon's pid record under `$CODEX_HOME/app-server-daemon/`: JSON
/// with a `pid` field. Released Codex (0.157–0.162) names it `daemon.pid`;
/// Codex's main branch has since renamed it `app-server.pid`.
const PID_RECORDS: [&str; 2] = ["daemon.pid", "app-server.pid"];

/// Is a Codex app-server daemon live for this `$CODEX_HOME`? Reads the
/// daemon's pid record and confirms the process is still an app-server, so a
/// stale record or a recycled pid reads as not running. For the doctor only.
pub(crate) fn daemon_running(codex_home: &std::path::Path) -> bool {
    let state = codex_home.join("app-server-daemon");
    PID_RECORDS.iter().any(|name| {
        let Ok(record) = std::fs::read(state.join(name)) else {
            return false;
        };
        let pid = serde_json::from_slice::<serde_json::Value>(&record)
            .ok()
            .and_then(|v| v.get("pid")?.as_u64())
            .and_then(|p| u32::try_from(p).ok())
            .filter(|&p| p > 1);
        pid.and_then(proc_info).is_some_and(|(_, args)| classify_ancestor(&args) == Some(true))
    })
}

/// `(ppid, argv)` of `pid`. Linux reads procfs (no spawn — this runs on
/// every Codex hook); elsewhere one `ps` call.
#[cfg(target_os = "linux")]
fn proc_info(pid: u32) -> Option<(u32, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid …` — comm may hold spaces/parens, so split after
    // the LAST ')'.
    let ppid = stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()?;
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args = String::from_utf8_lossy(&cmdline).replace('\0', " ");
    Some((ppid, args.trim_end().to_string()))
}

#[cfg(not(target_os = "linux"))]
fn proc_info(pid: u32) -> Option<(u32, String)> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-o", "args=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_ps_line(&String::from_utf8_lossy(&out.stdout))
}

/// Pure: parse `ps -o ppid= -o args=` output (`  123 codex app-server …`).
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn parse_ps_line(out: &str) -> Option<(u32, String)> {
    let (ppid, args) = out.trim().split_once(char::is_whitespace)?;
    Some((ppid.parse().ok()?, args.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_is_an_app_server() {
        let daemon = "/Users/u/.codex/packages/app-server-daemon/releases/0.159.2-aarch64-apple-darwin/bin/codex \
                      app-server --listen unix:// --managed-daemon";
        assert_eq!(classify_ancestor(daemon), Some(true));
        assert_eq!(classify_ancestor("codex app-server"), Some(true));
    }

    #[test]
    fn the_tui_is_not_even_with_daemon_flags_and_paths() {
        assert_eq!(classify_ancestor("codex --yolo"), Some(false));
        assert_eq!(classify_ancestor("codex"), Some(false));
        assert_eq!(
            classify_ancestor(
                "/opt/homebrew/bin/codex --managed-daemon --daemon-cli-executable \
                 /Users/u/.codex/packages/app-server-daemon/current/bin/codex"
            ),
            Some(false)
        );
        // npm's launcher: node runs codex.js, which spawns the native TUI.
        assert_eq!(classify_ancestor("node /usr/lib/node_modules/@openai/codex/bin/codex.js"), Some(false));
        // `codex resume app-server`? A thread id, not the subcommand position.
        assert_eq!(classify_ancestor("codex resume app-server"), Some(false));
    }

    #[test]
    fn non_codex_ancestors_keep_the_walk_going() {
        assert_eq!(classify_ancestor("/bin/zsh -lc zj-radar notify codex"), None);
        assert_eq!(classify_ancestor("bash -c codex app-server"), None, "a shell running codex is not codex");
        assert_eq!(classify_ancestor("zellij --session main"), None);
        assert_eq!(classify_ancestor("/usr/bin/codexctl app-server"), None);
        assert_eq!(classify_ancestor(""), None);
    }

    #[test]
    fn parses_ps_output() {
        assert_eq!(
            parse_ps_line("  4242 codex app-server --listen unix://\n"),
            Some((4242, "codex app-server --listen unix://".into()))
        );
        assert_eq!(parse_ps_line("1 /sbin/launchd"), Some((1, "/sbin/launchd".into())));
        assert_eq!(parse_ps_line(""), None, "a dead pid prints nothing");
        assert_eq!(parse_ps_line("  77"), None, "ppid with no args (zombie) — unknown");
    }

    #[test]
    fn daemon_running_reads_the_pid_record() {
        let dir = std::env::temp_dir().join(format!("zjr-codex-daemon-{}", std::process::id()));
        let state = dir.join("app-server-daemon");
        std::fs::create_dir_all(&state).unwrap();
        assert!(!daemon_running(&dir), "no record");
        // This test process is alive but is not an app-server → not running.
        for name in PID_RECORDS {
            std::fs::write(state.join(name), format!("{{\"pid\":{}}}", std::process::id())).unwrap();
            assert!(!daemon_running(&dir), "a live non-app-server pid is a stale record");
            std::fs::write(state.join(name), "not json").unwrap();
            assert!(!daemon_running(&dir));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
