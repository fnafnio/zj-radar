//! `zj-radar setup pi` — vendor the bridge extension into pi's auto-loaded
//! global extensions dir (`<agent dir>/extensions/zj-radar.js`). No settings
//! edit; a clean uninstall is one file delete. Install, uninstall, `--check`
//! and producer detection all key on the header marker (`detect.rs`).

use super::detect::pi_extension_is_ours;
use super::vendored::{
    dry_run_backup_line, kept_backup_line, plan_install, plan_uninstall, read_existing, refuse_if_managed,
    remove_backup_if_ours, write_bridge, Existing, InstallPlan, UninstallPlan,
};
use super::*;

use std::ffi::OsString;
use std::path::PathBuf;

/// The bridge file: `<agent dir>/extensions/zj-radar.js`.
pub(crate) fn pi_extension_path() -> Option<PathBuf> {
    pi_agent_dir().map(|d| d.join("extensions").join(PI_EXTENSION_FILE_NAME))
}

/// Read the vendored bridge for producer detection — routed through the same
/// resolver `setup pi` writes with, so `$PI_CODING_AGENT_DIR` is honored on
/// both sides.
pub(crate) fn pi_extension_text() -> Option<String> {
    pi_extension_path().and_then(|p| std::fs::read_to_string(p).ok())
}

/// Is our bridge extension on disk? The bare doctor's "pi is set up here"
/// signal for a PATH-less (bun/Nix-run) pi.
pub(crate) fn pi_extension_is_ours_on_disk() -> bool {
    pi_extension_text().is_some_and(|t| pi_extension_is_ours(&t))
}

fn pi_agent_dir() -> Option<PathBuf> {
    pi_agent_dir_from(std::env::var_os("PI_CODING_AGENT_DIR"), std::env::var_os("HOME"))
}

/// Resolve pi's agent dir the way pi's `getAgentDir()` does
/// (`dist/config.js`): `$PI_CODING_AGENT_DIR` wins (with a leading `~`
/// expanded against `$HOME`), else `$HOME/.pi/agent`. `None` when neither
/// resolves. Pure (env passed in) so the precedence is unit-tested.
fn pi_agent_dir_from(override_dir: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let home = home.filter(|h| !h.is_empty()).map(PathBuf::from);
    if let Some(dir) = override_dir.filter(|d| !d.is_empty()) {
        let text = dir.to_string_lossy();
        if text == "~" {
            return home;
        }
        if let Some(rest) = text.strip_prefix("~/") {
            return home.map(|h| h.join(rest));
        }
        return Some(PathBuf::from(dir));
    }
    home.map(|h| h.join(".pi").join("agent"))
}

/// Pure: is pi present? The binary on PATH, an explicitly set
/// `$PI_CODING_AGENT_DIR` (the user told us where pi's agent dir is — that is
/// itself evidence pi is configured here, even before the dir exists on
/// disk), an existing agent dir (a bun/Nix-run user may have no `pi` on
/// PATH), or our bridge already there.
fn pi_installed_from(on_path: bool, override_set: bool, agent_dir_exists: bool, extension_exists: bool) -> bool {
    on_path || override_set || agent_dir_exists || extension_exists
}

pub(crate) fn setup_pi(uninstall: bool, opts: BridgeSetupOpts) {
    let (Some(agent_dir), Some(path)) = (pi_agent_dir(), pi_extension_path()) else {
        crate::exit::fail_report("pi", "skipped — set $HOME or $PI_CODING_AGENT_DIR so pi's agent dir can be resolved");
        return;
    };
    let existing = read_existing(&path);
    let override_set = std::env::var_os("PI_CODING_AGENT_DIR").is_some_and(|d| !d.is_empty());
    if !uninstall
        && !pi_installed_from(which("pi"), override_set, agent_dir.is_dir(), !matches!(existing, Existing::Absent))
    {
        return report_skip("pi", "binary/agent dir not found", opts.explicit);
    }
    setup_extension_file(
        &ExtensionFile {
            label: "pi",
            path: &path,
            js: PI_EXTENSION_JS,
            is_ours: pi_extension_is_ours,
            restart_hint: "restart pi (or run /reload)",
        },
        &existing,
        uninstall,
        opts,
    );
}

/// One vendored single-file extension bridge (pi's, omp's): where it lives,
/// what we write, and how we recognize ours.
#[derive(Clone, Copy)]
pub(super) struct ExtensionFile<'a> {
    pub label: &'static str,
    pub path: &'a std::path::Path,
    pub js: &'static str,
    pub is_ours: fn(&str) -> bool,
    /// How the agent picks up a freshly written bridge.
    pub restart_hint: &'static str,
}

/// Install or uninstall one extension bridge file — the shared body of
/// `setup pi` and `setup omp` once the caller has resolved the path and
/// gated on the agent being present.
pub(super) fn setup_extension_file(ext: &ExtensionFile, existing: &Existing, uninstall: bool, opts: BridgeSetupOpts) {
    let ExtensionFile { label, path, js, is_ours, restart_hint } = *ext;
    if uninstall {
        match plan_uninstall(existing, is_ours) {
            UninstallPlan::Absent => println!("{label}: extension not installed ({})", path.display()),
            UninstallPlan::NotOurs => {
                println!("{label}: extension not ours (marker absent) — leaving {}", path.display())
            }
            UninstallPlan::Remove if refuse_if_managed(label, path, true) => {}
            UninstallPlan::Remove if opts.dry_run => {
                println!("--- would remove {} (dry-run) ---", path.display());
                if let Some(line) = dry_run_backup_line(label, path, is_ours) {
                    println!("{line}");
                }
            }
            UninstallPlan::Remove => {
                if !confirm(&format!("Remove {}?", path.display()), opts.yes, opts.is_tty) {
                    return report_declined(label, opts.is_tty, opts.explicit);
                }
                if let Err(e) = std::fs::remove_file(path) {
                    crate::exit::fail_report(label, format!("remove failed — {e}"));
                    return;
                }
                println!("{label}: extension removed ({})", path.display());
                // Its restore point goes too when ours (a stale-ours rewrite); a
                // foreign one — what `--force` replaced — is the user's only copy.
                if let Some(bak) = remove_backup_if_ours(path, is_ours) {
                    println!("{}", kept_backup_line(label, &bak, path, false));
                }
            }
        }
        return;
    }

    match plan_install(existing, js, opts.force, is_ours) {
        InstallPlan::RefuseForeign => {
            let why = existing.refusal_reason(path);
            crate::exit::fail_report(
                label,
                format!("{why}. Refusing to overwrite it.\nRe-run with --force to replace it."),
            );
        }
        InstallPlan::UpToDate => {
            println!("{label}: extension already up to date ({})", path.display());
            print_extension_guidance(label, restart_hint, false);
        }
        InstallPlan::Write if refuse_if_managed(label, path, false) => {}
        InstallPlan::Write if opts.dry_run => {
            println!("--- {} (dry-run) ---\n{}", path.display(), js);
            print_extension_guidance(label, restart_hint, true);
        }
        InstallPlan::Write => {
            if !confirm(&format!("Write {}?", path.display()), opts.yes, opts.is_tty) {
                return report_declined(label, opts.is_tty, opts.explicit);
            }
            if let Err(e) = write_bridge(path, js, existing, is_ours) {
                crate::exit::fail_report(label, format!("write failed — {e}"));
                return;
            }
            println!("{label}: extension installed ({})", path.display());
            print_extension_guidance(label, restart_hint, true);
        }
    }
}

fn print_extension_guidance(label: &str, restart_hint: &str, wrote: bool) {
    if !which("zj-radar") {
        eprintln!(
            "{label}: warning — `zj-radar` not found on PATH; the bridge spawns it per event, \
             so status won't broadcast until it's installed"
        );
    }
    if wrote {
        println!("{label}: {restart_hint} for the bridge to take effect.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn pi_agent_dir_prefers_the_override_and_expands_tilde() {
        assert_eq!(pi_agent_dir_from(Some(os("/x/pi")), Some(os("/home/u"))), Some(PathBuf::from("/x/pi")));
        assert_eq!(pi_agent_dir_from(Some(os("~/p")), Some(os("/home/u"))), Some(PathBuf::from("/home/u/p")));
        assert_eq!(pi_agent_dir_from(Some(os("~")), Some(os("/home/u"))), Some(PathBuf::from("/home/u")));
        assert_eq!(pi_agent_dir_from(Some(os("~/p")), None), None, "a tilde override needs a HOME");
    }

    #[test]
    fn pi_agent_dir_falls_back_to_home_and_treats_empty_as_unset() {
        assert_eq!(pi_agent_dir_from(None, Some(os("/home/u"))), Some(PathBuf::from("/home/u/.pi/agent")));
        assert_eq!(
            pi_agent_dir_from(Some(OsString::new()), Some(os("/home/u"))),
            Some(PathBuf::from("/home/u/.pi/agent"))
        );
        assert_eq!(pi_agent_dir_from(None, Some(OsString::new())), None);
        assert_eq!(pi_agent_dir_from(None, None), None);
    }

    #[test]
    fn pi_counts_as_installed_via_binary_agent_dir_or_extension() {
        assert!(pi_installed_from(true, false, false, false));
        // An explicitly set `$PI_CODING_AGENT_DIR` is itself evidence pi is
        // configured here, even before that dir exists on disk.
        assert!(pi_installed_from(false, true, false, false));
        // A populated ~/.pi/agent with no binary on PATH (Nix/bun-run users)
        // must not make an explicit `setup pi` silently skip.
        assert!(pi_installed_from(false, false, true, false));
        assert!(pi_installed_from(false, false, false, true));
        assert!(!pi_installed_from(false, false, false, false));
    }

    /// Weld: the embedded bridge carries the marker the install path, doctor
    /// and producer detection key on, speaks the spawn contract, and imports
    /// only node builtins (a bare extensions-dir file has no node_modules).
    #[test]
    fn embedded_extension_carries_marker_and_contract() {
        let js = super::super::PI_EXTENSION_JS;
        assert!(super::super::PI_EXTENSION_MARKER.starts_with(super::super::PI_EXTENSION_MARKER_PREFIX));
        assert!(js.lines().next().is_some_and(|l| l.contains(super::super::PI_EXTENSION_MARKER)));
        assert!(js.contains("\"notify\", \"pi\", \"--status\""));
        assert!(!js.contains("spawnSync") && !js.contains("execSync"), "never block pi's event loop");
        assert!(js.contains("ZELLIJ"), "must gate on $ZELLIJ");
        assert!(js.contains("\"ignore\", \"ignore\""), "child stdout/stderr must never reach pi's TUI");
        assert!(js.contains("stdin.on(\"error\""), "child.stdin needs an error listener or an async EPIPE crashes pi");
        for line in js.lines().filter(|l| l.starts_with("import ")) {
            assert!(line.contains("from \"node:"), "only node: builtins may be imported: {line}");
        }
        assert!(js.contains("export default function"));
    }
}
