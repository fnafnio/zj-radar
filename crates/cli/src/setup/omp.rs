//! `zj-radar setup omp` — vendor the bridge extension into omp's (oh-my-pi's)
//! auto-loaded user extensions dir (`<agent dir>/extensions/zj-radar-omp.js`).
//! No config edit; a clean uninstall is one file delete. Install, uninstall,
//! `--check` and producer detection all key on the header marker
//! (`detect.rs`). The install flow itself is pi's (`pi::setup_extension_file`)
//! — omp is a pi fork with the same single-file extension model.
//!
//! The file is NOT `zj-radar.js` like pi's: omp honors `$PI_CODING_AGENT_DIR`
//! too, so a user who sets it runs both agents off one extensions dir, and
//! the two bridges must not overwrite each other there (each bridge also
//! stays dark inside the other agent's process).

use super::detect::omp_extension_is_ours;
use super::pi::{setup_extension_file, ExtensionFile};
use super::vendored::{read_existing, Existing};
use super::*;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The bridge file: `<agent dir>/extensions/zj-radar-omp.js`.
pub(crate) fn omp_extension_path() -> Option<PathBuf> {
    omp_agent_dir().map(|d| d.join("extensions").join(OMP_EXTENSION_FILE_NAME))
}

/// Read the vendored bridge for producer detection — routed through the same
/// resolver `setup omp` writes with.
pub(crate) fn omp_extension_text() -> Option<String> {
    omp_extension_path().and_then(|p| std::fs::read_to_string(p).ok())
}

/// Is our bridge extension on disk? The bare doctor's "omp is set up here"
/// signal for a PATH-less omp.
pub(crate) fn omp_extension_is_ours_on_disk() -> bool {
    omp_extension_text().is_some_and(|t| omp_extension_is_ours(&t))
}

fn omp_agent_dir() -> Option<PathBuf> {
    let var = std::env::var_os;
    omp_agent_dir_from(&OmpDirEnv {
        agent_dir: var("PI_CODING_AGENT_DIR"),
        config_dir: var("PI_CONFIG_DIR"),
        omp_profile: var("OMP_PROFILE"),
        pi_profile: var("PI_PROFILE"),
        home: var("HOME"),
        cwd: std::env::current_dir().ok(),
    })
}

/// The env omp's agent-dir resolution reads (`pi-utils/src/dirs.ts`).
struct OmpDirEnv {
    agent_dir: Option<OsString>,
    config_dir: Option<OsString>,
    omp_profile: Option<OsString>,
    pi_profile: Option<OsString>,
    home: Option<OsString>,
    cwd: Option<PathBuf>,
}

/// Resolve omp's active agent dir the way omp does: the profile is
/// `$OMP_PROFILE` when set at all (an empty one selects the default), else
/// `$PI_PROFILE`; `"default"` or blank is no profile. The config root is
/// `$HOME/<$PI_CONFIG_DIR or .omp>` (node's `path.join`, so even a leading-`/`
/// value stays under `$HOME`). A named profile's agent dir is
/// `<root>/profiles/<name>/agent`; the default profile's is
/// `$PI_CODING_AGENT_DIR` (resolved against the cwd, no `~` expansion) when
/// set, else `<root>/agent`. Pure (env passed in) so the precedence is
/// unit-tested.
fn omp_agent_dir_from(env: &OmpDirEnv) -> Option<PathBuf> {
    let non_empty =
        |v: &Option<OsString>| v.as_ref().filter(|s| !s.is_empty()).map(|s| s.to_string_lossy().into_owned());
    let profile = match &env.omp_profile {
        Some(p) => Some(p.to_string_lossy().into_owned()),
        None => non_empty(&env.pi_profile),
    }
    .map(|p| p.trim().to_string())
    .filter(|p| !p.is_empty() && p != "default");

    if profile.is_none() {
        if let Some(dir) = non_empty(&env.agent_dir) {
            let dir = PathBuf::from(dir);
            return if dir.is_absolute() { Some(dir) } else { env.cwd.as_ref().map(|c| c.join(dir)) };
        }
    }
    let home = non_empty(&env.home).map(PathBuf::from)?;
    let config_dir = non_empty(&env.config_dir).unwrap_or_else(|| ".omp".to_string());
    let root = home.join(config_dir.trim_start_matches('/'));
    Some(match profile {
        Some(p) => root.join("profiles").join(p).join("agent"),
        None => root.join("agent"),
    })
}

/// Pure: is omp present? The binary on PATH, an existing agent dir (omp
/// creates it on first run), or our bridge already there.
fn omp_installed_from(on_path: bool, agent_dir_exists: bool, extension_exists: bool) -> bool {
    on_path || agent_dir_exists || extension_exists
}

pub(crate) fn setup_omp(uninstall: bool, opts: BridgeSetupOpts) {
    let (Some(agent_dir), Some(path)) = (omp_agent_dir(), omp_extension_path()) else {
        crate::exit::fail_report("omp", "skipped — set $HOME so omp's agent dir can be resolved");
        return;
    };
    let existing = read_existing(&path);
    if !uninstall && !omp_installed_from(which("omp"), agent_dir.is_dir(), !matches!(existing, Existing::Absent)) {
        return report_skip("omp", "binary/agent dir not found", opts.explicit);
    }
    setup_extension_file(
        &ExtensionFile {
            label: "omp",
            path: Path::new(&path),
            js: OMP_EXTENSION_JS,
            is_ours: omp_extension_is_ours,
            restart_hint: "restart omp",
        },
        &existing,
        uninstall,
        opts,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(agent_dir: Option<&str>, config_dir: Option<&str>, omp: Option<&str>, pi: Option<&str>) -> OmpDirEnv {
        OmpDirEnv {
            agent_dir: agent_dir.map(OsString::from),
            config_dir: config_dir.map(OsString::from),
            omp_profile: omp.map(OsString::from),
            pi_profile: pi.map(OsString::from),
            home: Some(OsString::from("/home/u")),
            cwd: Some(PathBuf::from("/work")),
        }
    }
    fn dir(e: OmpDirEnv) -> Option<PathBuf> {
        omp_agent_dir_from(&e)
    }

    #[test]
    fn default_profile_lives_under_dot_omp() {
        assert_eq!(dir(env(None, None, None, None)), Some(PathBuf::from("/home/u/.omp/agent")));
        assert_eq!(dir(env(None, Some(".config/omp"), None, None)), Some(PathBuf::from("/home/u/.config/omp/agent")));
        // node's path.join keeps a leading-slash config dir under $HOME.
        assert_eq!(dir(env(None, Some("/abs"), None, None)), Some(PathBuf::from("/home/u/abs/agent")));
        assert_eq!(dir(env(None, Some(""), Some(""), None)), Some(PathBuf::from("/home/u/.omp/agent")));
    }

    #[test]
    fn agent_dir_override_applies_to_the_default_profile_only() {
        assert_eq!(dir(env(Some("/x/agent"), None, None, None)), Some(PathBuf::from("/x/agent")));
        assert_eq!(dir(env(Some("rel"), None, None, None)), Some(PathBuf::from("/work/rel")));
        assert_eq!(
            dir(env(Some("/x/agent"), None, Some("work"), None)),
            Some(PathBuf::from("/home/u/.omp/profiles/work/agent"))
        );
    }

    #[test]
    fn omp_profile_wins_and_an_empty_one_selects_default() {
        assert_eq!(dir(env(None, None, None, Some("p"))), Some(PathBuf::from("/home/u/.omp/profiles/p/agent")));
        assert_eq!(dir(env(None, None, Some("o"), Some("p"))), Some(PathBuf::from("/home/u/.omp/profiles/o/agent")));
        // An explicitly empty OMP_PROFILE never falls through to PI_PROFILE.
        assert_eq!(dir(env(None, None, Some(""), Some("p"))), Some(PathBuf::from("/home/u/.omp/agent")));
        assert_eq!(dir(env(None, None, Some(" default "), None)), Some(PathBuf::from("/home/u/.omp/agent")));
    }

    #[test]
    fn no_home_resolves_only_through_the_override() {
        let mut e = env(None, None, None, None);
        e.home = None;
        assert_eq!(dir(e), None);
        let mut e = env(Some("/x"), None, None, None);
        e.home = None;
        assert_eq!(dir(e), Some(PathBuf::from("/x")));
    }

    #[test]
    fn omp_counts_as_installed_via_binary_agent_dir_or_extension() {
        assert!(omp_installed_from(true, false, false));
        assert!(omp_installed_from(false, true, false));
        assert!(omp_installed_from(false, false, true));
        assert!(!omp_installed_from(false, false, false));
    }

    /// Weld: the embedded bridge carries the marker the install path, doctor
    /// and producer detection key on, speaks the spawn contract, and imports
    /// only node builtins (a bare extensions-dir file has no node_modules).
    #[test]
    fn embedded_extension_carries_marker_and_contract() {
        let js = OMP_EXTENSION_JS;
        assert!(OMP_EXTENSION_MARKER.starts_with(OMP_EXTENSION_MARKER_PREFIX));
        assert!(js.lines().next().is_some_and(|l| l.contains(OMP_EXTENSION_MARKER)));
        assert!(!js.contains(PI_EXTENSION_MARKER_PREFIX), "never mistaken for pi's bridge");
        assert!(!PI_EXTENSION_JS.contains(OMP_EXTENSION_MARKER_PREFIX), "pi's bridge never mistaken for ours");
        assert_ne!(OMP_EXTENSION_FILE_NAME, PI_EXTENSION_FILE_NAME, "the two bridges may share one dir");
        assert!(js.contains("\"notify\", \"omp\", \"--status\""));
        assert!(!js.contains("spawnSync") && !js.contains("execSync"), "never block omp's event loop");
        assert!(js.contains("ZELLIJ"), "must gate on $ZELLIJ");
        assert!(js.contains("\"ignore\", \"ignore\""), "child stdout/stderr must never reach omp's TUI");
        assert!(js.contains("stdin.on(\"error\""), "child.stdin needs an error listener or an async EPIPE crashes omp");
        for line in js.lines().filter(|l| l.starts_with("import ")) {
            assert!(line.contains("from \"node:"), "only node: builtins may be imported: {line}");
        }
        assert!(js.contains("export default function"));
    }
}
