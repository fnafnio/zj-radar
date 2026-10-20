//! omp (oh-my-pi) extension bridge → Radar status update.
//!
//! omp is a fork of pi, and its producer is a sibling of pi's: a thin JS
//! bridge (`setup/omp_extension.js`, vendored into omp's auto-loaded
//! extensions dir) translates omp's extension events into the same wire
//! vocabulary the pi bridge speaks (`prompt`, `tool`, `ui_prompt.*`,
//! `settled`, `error`, `session.*`) and spawns `zj-radar notify omp`. So the
//! event → status table is pi's; this module supplies omp's tool names. The
//! bridge resolves omp's dialogs (the `ask` tool, tool approvals) into
//! `ui_prompt.*` itself, so neither reaches the tool table.

use super::{AgentUpdate, Bridge, Intake};

/// omp's half of the shared bridge derivation. Tool names are omp's
/// built-ins (`packages/coding-agent/src/tools`, `edit/`, `task/`, all
/// lowercase); the file arg is `path`, as in pi. `ast_grep` is a structural
/// search; any other tool (extension, MCP, or custom) falls to the `working`
/// baseline.
const BRIDGE: Bridge = Bridge {
    status_from_event: super::pi::status_from_event,
    tool_event: "tool",
    prompt_event: "prompt",
    tool_names: &[
        ("read", "Read"),
        ("write", "Write"),
        ("edit", "Edit"),
        ("bash", "Bash"),
        ("grep", "Grep"),
        ("ast_grep", "Grep"),
        ("find", "Glob"),
        ("glob", "Glob"),
        ("task", "Task"),
        ("todo", "TodoWrite"),
    ],
    arg_keys: &[("path", "file_path")],
};

/// Decide omp's status + msg + cwd via `agents::derive_bridged`. Returns
/// `None` for a no-op.
pub fn derive(intake: &Intake) -> Option<AgentUpdate> {
    super::derive_bridged(intake, &BRIDGE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Status;

    fn intake<'a>(raw: &'a str, status_arg: Option<&'a str>) -> Intake<'a> {
        Intake { raw, status_arg }
    }

    #[test]
    fn prompt_is_running_and_captures_the_task() {
        let u = derive(&intake(r#"{"event":"prompt","prompt":"wire up omp\nmore","cwd":"/repo"}"#, Some("running")))
            .unwrap();
        assert_eq!(u.status, Status::Running);
        assert_eq!(u.msg, "working");
        assert_eq!(u.task.as_deref(), Some("wire up omp"));
        assert_eq!(u.cwd.as_deref(), Some("/repo"));
    }

    #[test]
    fn tool_events_normalize_omp_tools_into_shared_vocab() {
        for (raw, expected) in [
            (r#"{"event":"tool","tool":"read","tool_input":{"path":"/p/auth.rs"}}"#, "reading auth.rs"),
            (r#"{"event":"tool","tool":"edit","tool_input":{"path":"/p/mod.rs"}}"#, "editing mod.rs"),
            (r#"{"event":"tool","tool":"write","tool_input":{"path":"/p/new.rs"}}"#, "editing new.rs"),
            (r#"{"event":"tool","tool":"bash","tool_input":{"command":"cargo test"}}"#, "running tests"),
            (r#"{"event":"tool","tool":"grep","tool_input":{}}"#, "searching"),
            (r#"{"event":"tool","tool":"ast_grep","tool_input":{}}"#, "searching"),
            (r#"{"event":"tool","tool":"find","tool_input":{}}"#, "searching"),
            (r#"{"event":"tool","tool":"glob","tool_input":{}}"#, "searching"),
            (r#"{"event":"tool","tool":"task","tool_input":{}}"#, "delegating"),
            (r#"{"event":"tool","tool":"todo","tool_input":{}}"#, "planning"),
            (r#"{"event":"tool","tool":"vibe_spawn","tool_input":{}}"#, "working"),
        ] {
            let u = derive(&intake(raw, Some("running"))).unwrap();
            assert_eq!(u.status, Status::Running, "{raw}");
            assert_eq!(u.msg, expected, "{raw}");
            assert_eq!(u.task, None, "tool events never carry a task: {raw}");
        }
    }

    #[test]
    fn dialogs_are_pending_and_blank_is_dropped() {
        let u = derive(&intake(r#"{"event":"ui_prompt.start","message":"Allow bash?"}"#, Some("pending"))).unwrap();
        assert_eq!(u.status, Status::Pending);
        assert_eq!(u.msg, "Allow bash?");
        assert!(derive(&intake(r#"{"event":"ui_prompt.start","message":" "}"#, Some("pending"))).is_none());
        let u = derive(&intake(r#"{"event":"ui_prompt.end"}"#, Some("running"))).unwrap();
        assert_eq!((u.status, u.msg.as_str()), (Status::Running, "working"));
    }

    #[test]
    fn settled_error_and_session_events() {
        let u = derive(&intake(r#"{"event":"settled","message":"Done.\n\nShip it?"}"#, Some("done"))).unwrap();
        assert_eq!((u.status, u.msg.as_str()), (Status::Pending, "Ship it?"));
        let u = derive(&intake(r#"{"event":"error","message":""}"#, Some("error"))).unwrap();
        assert_eq!((u.status, u.msg.as_str()), (Status::Error, "errored"));
        for ev in ["session.new", "session.end"] {
            let raw = format!(r#"{{"event":"{ev}","cwd":"/repo"}}"#);
            let u = derive(&intake(&raw, Some("idle"))).unwrap();
            assert_eq!((u.status, u.msg.as_str()), (Status::Idle, ""), "{ev}");
        }
        assert_eq!(derive(&intake(r#"{"event":"settled","message":"ok."}"#, None)).unwrap().status, Status::Done);
    }
}
