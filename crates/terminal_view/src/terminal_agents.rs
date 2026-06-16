//! Terminal-agent session integration: detecting which coding agent is running
//! in a terminal, capturing its native session id via a Zed-owned hook script,
//! and resuming the session after a restart.
//!
//! The hook script (`terminal_agent_session_hook.sh`) is installed into Zed's
//! data dir and registered in each agent's global hook config with a *stable*
//! pointer command. Keeping the command stable across Zed versions means an
//! agent like Codex (which records a trust hash of the exact hook command) only
//! has to trust it once. All churn happens inside the script file, which Zed
//! owns and rewrites in place.

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use terminal::Terminal;

/// Embedded source of the hook script Zed installs and owns.
const HOOK_SCRIPT_SOURCE: &str = include_str!("terminal_agent_session_hook.sh");
/// Script filename; the substring `zed-agent-session-hook` is also used to
/// recognize (and clean up) Zed-owned hook commands in agent config files.
const HOOK_SCRIPT_FILE_NAME: &str = "zed-agent-session-hook.sh";
/// Subdirectory of `paths::data_dir()` that holds the installed hook script.
const HOOK_SCRIPT_DIR_NAME: &str = "agent_hooks";

/// A coding agent Zed knows how to detect and resume in a terminal.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalAgentKind {
    Claude,
    Codex,
}

impl TerminalAgentKind {
    const ALL: [Self; 2] = [Self::Claude, Self::Codex];

    fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }

    /// Maps a terminal's foreground process name to an agent, if any.
    pub fn from_command_name(command: &str) -> Option<Self> {
        match command {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            command if command.starts_with("codex-") => Some(Self::Codex),
            _ => None,
        }
    }

    /// Shell command that resumes a previous session by id.
    fn resume_command(self, session_id: &str) -> String {
        match self {
            Self::Claude => format!("claude --resume {session_id}"),
            Self::Codex => format!("codex resume {session_id}"),
        }
    }

    /// Hook events Zed registers for this agent. Codex has no session-end hook;
    /// its dead sessions are cleared by foreground-process detection instead.
    ///
    /// Beyond session tracking, the tool/permission events drive the live
    /// activity state shown in the sidebar (see [`activity_for_event`]).
    fn hook_events(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &[
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "Notification",
                "Stop",
                "SessionEnd",
            ],
            Self::Codex => &[
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PermissionRequest",
                "PostToolUse",
                "Stop",
            ],
        }
    }

    /// Global config directory, honoring the agent's home-dir env override.
    fn config_dir(self) -> Option<PathBuf> {
        let (env_override, relative) = match self {
            Self::Claude => ("CLAUDE_CONFIG_DIR", ".claude"),
            Self::Codex => ("CODEX_HOME", ".codex"),
        };
        if let Some(dir) = std::env::var_os(env_override) {
            return Some(PathBuf::from(dir));
        }
        dirs::home_dir().map(|home| home.join(relative))
    }

    /// Config file (inside `config_dir`) that holds the agent's hooks.
    fn config_file(self) -> &'static str {
        match self {
            Self::Claude => "settings.json",
            Self::Codex => "hooks.json",
        }
    }
}

fn default_agent_kind() -> TerminalAgentKind {
    TerminalAgentKind::Codex
}

/// Persisted state needed to resume an agent session in a restored terminal.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TerminalAgentRestoreState {
    #[serde(default = "default_agent_kind")]
    pub agent: TerminalAgentKind,
    /// The agent's native session id, used to resume after a restart. `None`
    /// when the agent is running but hasn't surfaced a session yet.
    #[serde(default)]
    pub session_id: Option<String>,
}

impl TerminalAgentRestoreState {
    /// Recomputes the restore state for a terminal from its current foreground
    /// process and the latest hook payload, preserving `existing` when the
    /// terminal is mid-output and nothing conclusive is known yet.
    fn update(
        terminal: &Terminal,
        existing: Option<&TerminalAgentRestoreState>,
        hook_cache: &mut HookFileCache,
    ) -> TerminalAgentRestoreUpdate {
        // The cached foreground name only updates on PTY output, so it can stay
        // stuck on an agent after it exits to an idle shell. When the cache
        // claims an agent is in front, confirm with a fresh read so a stopped
        // session is actually noticed; otherwise the cheap cached read is fine.
        let cached = terminal.foreground_process_command_name();
        let command = if cached
            .as_deref()
            .and_then(TerminalAgentKind::from_command_name)
            .is_some()
        {
            match terminal.refresh_foreground_process_command_name() {
                Some(command) => command,
                // We were tracking an agent, but a live read can no longer find
                // any process in the terminal's foreground group. This is the
                // idle shell right after the agent quits: `tcgetpgrp` still
                // points at the agent's now-dead process group until the next
                // command runs. The agent is gone, so end the session rather
                // than treating it as unknown (which would keep resuming it).
                None => return TerminalAgentRestoreUpdate::Inactive,
            }
        } else {
            let Some(command) = cached else {
                return TerminalAgentRestoreUpdate::Unknown;
            };
            command
        };

        let Some(agent) = TerminalAgentKind::from_command_name(&command) else {
            // The agent is no longer the foreground process: the session ended.
            return TerminalAgentRestoreUpdate::Inactive;
        };

        // The agent is the foreground process, so it's running regardless of
        // whether we have a session id yet. Detect it from the process alone
        // (so the icon/title appear immediately) and fill in the session id
        // from the hook when available.
        let carry_over_session_id = || {
            existing
                .filter(|existing| existing.agent == agent)
                .and_then(|existing| existing.session_id.clone())
        };
        let session_id = match session_state_from_hook(terminal, hook_cache) {
            Some(TerminalAgentHookState::Active {
                agent: hook_agent,
                session_id,
                ..
            }) if hook_agent == agent => session_id.or_else(carry_over_session_id),
            Some(TerminalAgentHookState::Ended { agent: hook_agent }) if hook_agent == agent => {
                return TerminalAgentRestoreUpdate::Inactive;
            }
            // No usable hook session yet; carry over one we already had for
            // this agent so resume keeps working across refreshes.
            _ => carry_over_session_id(),
        };
        TerminalAgentRestoreUpdate::Active(Self { agent, session_id })
    }

    /// Shell input (with trailing newline) that resumes this session.
    pub fn resume_input(&self) -> Option<Vec<u8>> {
        let session_id = self.session_id.as_deref()?;
        Some(format!("{}\r", self.agent.resume_command(session_id)).into_bytes())
    }
}

/// What a terminal agent is currently doing, mapped onto the same UI states
/// the chat threads use. Derived from the latest lifecycle hook event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TerminalAgentActivity {
    /// No work in progress (freshly started, or finished its turn).
    #[default]
    Idle,
    /// Actively working on a turn (prompt submitted, running tools).
    Running,
    /// Blocked waiting for the user to approve an action.
    WaitingForConfirmation,
}

/// Maps a lifecycle hook event name to the agent's current activity. Event
/// names are the ones Zed registers in [`TerminalAgentKind::hook_events`].
fn activity_for_event(event: &str) -> TerminalAgentActivity {
    match event {
        // Claude surfaces permission prompts as `Notification`; Codex has a
        // dedicated `PermissionRequest` event.
        "Notification" | "PermissionRequest" => TerminalAgentActivity::WaitingForConfirmation,
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => TerminalAgentActivity::Running,
        // `SessionStart` (launched, awaiting first prompt), `Stop`/`SessionEnd`
        // (turn finished), and anything unrecognized: not actively working.
        _ => TerminalAgentActivity::Idle,
    }
}

/// Outcome of [`TerminalAgentRestoreState::update`].
pub enum TerminalAgentRestoreUpdate {
    /// An agent session is running; store/keep this restore state.
    Active(TerminalAgentRestoreState),
    /// No agent session is running; clear any stored restore state.
    Inactive,
    /// Inconclusive; keep whatever state was already stored.
    Unknown,
}

#[derive(Clone)]
enum TerminalAgentHookState {
    Active {
        agent: TerminalAgentKind,
        session_id: Option<String>,
        activity: TerminalAgentActivity,
    },
    Ended {
        agent: TerminalAgentKind,
    },
}

/// Wrapper payload written by the hook script to the terminal's state file.
#[derive(Deserialize)]
struct HookPayload {
    agent: Option<String>,
    event: Option<String>,
    payload: Option<Value>,
}

fn session_state_from_hook(
    terminal: &Terminal,
    cache: &mut HookFileCache,
) -> Option<TerminalAgentHookState> {
    let path = terminal.agent_session_state_path()?;
    cache.read(path)
}

/// Caches the parsed hook state keyed by the state file's mtime, so a terminal
/// streaming output doesn't re-read and re-parse the file on every wakeup.
#[derive(Default)]
struct HookFileCache {
    seen: bool,
    mtime: Option<SystemTime>,
    state: Option<TerminalAgentHookState>,
}

impl HookFileCache {
    fn read(&mut self, path: &Path) -> Option<TerminalAgentHookState> {
        let mtime = std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok();
        if self.seen && self.mtime == mtime {
            return self.state.clone();
        }
        self.seen = true;
        self.mtime = mtime;
        self.state = std::fs::read(path)
            .ok()
            .as_deref()
            .and_then(parse_hook_payload);
        self.state.clone()
    }

    /// The activity implied by the most recently parsed hook event.
    fn activity(&self) -> TerminalAgentActivity {
        match &self.state {
            Some(TerminalAgentHookState::Active { activity, .. }) => *activity,
            _ => TerminalAgentActivity::Idle,
        }
    }
}

fn parse_hook_payload(bytes: &[u8]) -> Option<TerminalAgentHookState> {
    let payload: HookPayload = serde_json::from_slice(bytes).ok()?;
    let agent = TerminalAgentKind::from_id(payload.agent.as_deref()?)?;

    let event = payload.event.as_deref().unwrap_or_default();
    if event == "SessionEnd" {
        return Some(TerminalAgentHookState::Ended { agent });
    }

    // The session id is optional: permission/notification events still tell us
    // the agent's activity even before (or without) a usable session id.
    let session_id = payload
        .payload
        .as_ref()
        .and_then(|payload| {
            payload
                .get("session_id")
                .or_else(|| payload.get("sessionId"))
        })
        .and_then(Value::as_str)
        .filter(|session_id| valid_session_id(session_id))
        .map(|session_id| session_id.to_string());
    Some(TerminalAgentHookState::Active {
        agent,
        session_id,
        activity: activity_for_event(event),
    })
}

fn valid_session_id(session_id: &str) -> bool {
    !session_id.is_empty()
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

// ---------------------------------------------------------------------------
// Session-name resolution
// ---------------------------------------------------------------------------

/// The title to display for a terminal that may be running a coding agent.
pub enum TerminalAgentTitle {
    /// No agent is running; show a static idle label.
    Idle,
    /// The agent writes its session name into the OSC terminal title (Claude),
    /// so the caller should use the live terminal title as-is.
    TerminalTitle,
    /// A resolved agent session name (a named or unnamed Codex session).
    Name(String),
}

/// Tracks which coding agent (if any) is running in a terminal and what to
/// title its thread, caching file reads so a terminal streaming output stays
/// cheap to refresh.
#[derive(Default)]
pub struct TerminalAgentTracker {
    restore_state: Option<TerminalAgentRestoreState>,
    activity: TerminalAgentActivity,
    hook_cache: HookFileCache,
    name_cache: RefCell<CodexNameCache>,
}

impl TerminalAgentTracker {
    pub fn restore_state(&self) -> Option<TerminalAgentRestoreState> {
        self.restore_state.clone()
    }

    pub fn set_restore_state(&mut self, restore_state: TerminalAgentRestoreState) {
        self.restore_state = Some(restore_state);
    }

    /// The agent's current activity, or `Idle` when no agent is running.
    pub fn activity(&self) -> TerminalAgentActivity {
        self.activity
    }

    /// Recomputes the running-agent state and activity from the terminal's
    /// foreground process and latest hook event. Returns whether either changed.
    pub fn refresh(&mut self, terminal: &Terminal) -> bool {
        let previous = (self.restore_state.clone(), self.activity);
        match TerminalAgentRestoreState::update(
            terminal,
            self.restore_state.as_ref(),
            &mut self.hook_cache,
        ) {
            TerminalAgentRestoreUpdate::Active(restore_state) => {
                self.restore_state = Some(restore_state);
            }
            TerminalAgentRestoreUpdate::Inactive => {
                self.restore_state = None;
            }
            TerminalAgentRestoreUpdate::Unknown => {}
        }
        // Activity is only meaningful while an agent is actually running.
        self.activity = if self.restore_state.is_some() {
            self.hook_cache.activity()
        } else {
            TerminalAgentActivity::Idle
        };
        previous != (self.restore_state.clone(), self.activity)
    }

    /// What to title the terminal's thread based on the running agent. For
    /// Codex this resolves the session's name (mtime-cached), falling back to a
    /// static label for an unnamed session.
    pub fn agent_title(&self) -> TerminalAgentTitle {
        match self.restore_state.as_ref() {
            None => TerminalAgentTitle::Idle,
            Some(restore_state) => match restore_state.agent {
                TerminalAgentKind::Claude => TerminalAgentTitle::TerminalTitle,
                TerminalAgentKind::Codex => {
                    let name = restore_state
                        .session_id
                        .as_deref()
                        .and_then(|session_id| {
                            self.name_cache.borrow_mut().session_name(session_id)
                        })
                        .unwrap_or_else(|| TerminalAgentKind::Codex.display_name().to_string());
                    TerminalAgentTitle::Name(name)
                }
            },
        }
    }
}

fn codex_session_index_path() -> Option<PathBuf> {
    Some(
        TerminalAgentKind::Codex
            .config_dir()?
            .join("session_index.jsonl"),
    )
}

#[derive(Deserialize)]
struct CodexSessionIndexEntry {
    id: Option<String>,
    thread_name: Option<String>,
}

/// Scans Codex's append-only `session_index.jsonl` for the latest non-empty
/// `thread_name` recorded for `session_id`. Each `/rename` appends a new line,
/// so the last matching entry wins.
fn read_codex_thread_name(path: &Path, session_id: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut name = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<CodexSessionIndexEntry>(line) else {
            continue;
        };
        if entry.id.as_deref() == Some(session_id)
            && let Some(thread_name) = entry.thread_name.filter(|name| !name.trim().is_empty())
        {
            name = Some(thread_name);
        }
    }
    name
}

/// Caches the resolved Codex session name keyed by the session index file's
/// mtime, so the file is only re-read after an actual rename.
#[derive(Default)]
struct CodexNameCache {
    session_id: Option<String>,
    seen: bool,
    mtime: Option<SystemTime>,
    name: Option<String>,
}

impl CodexNameCache {
    fn session_name(&mut self, session_id: &str) -> Option<String> {
        let path = codex_session_index_path()?;
        let mtime = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();
        if self.seen && self.session_id.as_deref() == Some(session_id) && self.mtime == mtime {
            return self.name.clone();
        }
        self.seen = true;
        self.session_id = Some(session_id.to_string());
        self.mtime = mtime;
        self.name = read_codex_thread_name(&path, session_id);
        self.name.clone()
    }
}

// ---------------------------------------------------------------------------
// Hook installation
// ---------------------------------------------------------------------------

/// Installs the Zed-owned hook script and registers it in each supported
/// agent's global hook config. Idempotent and safe to run on every launch:
/// files are only rewritten when their contents change, user/third-party hooks
/// are preserved, and the registered command is stable across Zed versions.
pub fn install_terminal_agent_hooks() -> anyhow::Result<()> {
    let script_path = install_hook_script().context("installing terminal agent hook script")?;
    for agent in TerminalAgentKind::ALL {
        install_agent_hooks(agent, &script_path)
            .with_context(|| format!("installing {} session hooks", agent.id()))?;
    }
    Ok(())
}

fn install_hook_script() -> anyhow::Result<PathBuf> {
    let dir = paths::data_dir().join(HOOK_SCRIPT_DIR_NAME);
    let path = dir.join(HOOK_SCRIPT_FILE_NAME);
    write_file_if_changed(&path, HOOK_SCRIPT_SOURCE.as_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .context("setting hook script executable bit")?;
    }
    Ok(path)
}

fn install_agent_hooks(agent: TerminalAgentKind, script_path: &Path) -> anyhow::Result<()> {
    let Some(config_dir) = agent.config_dir() else {
        return Ok(());
    };
    let config_path = config_dir.join(agent.config_file());

    let mut config = match std::fs::read(&config_path) {
        Ok(contents) => serde_json::from_slice::<Value>(&contents)
            .with_context(|| format!("parsing {}", config_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(error) => return Err(error.into()),
    };

    apply_zed_hooks(agent, &mut config, script_path)?;

    let mut serialized = serde_json::to_vec_pretty(&config)?;
    serialized.push(b'\n');
    write_file_if_changed(&config_path, &serialized)?;
    Ok(())
}

/// Removes any previously-installed Zed hooks from `config`, then appends the
/// current ones. Splitting this out from the filesystem keeps it unit-testable.
fn apply_zed_hooks(
    agent: TerminalAgentKind,
    config: &mut Value,
    script_path: &Path,
) -> anyhow::Result<()> {
    let config = config
        .as_object_mut()
        .context("agent hook config must be a JSON object")?;
    let hooks = config
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("agent `hooks` field must be a JSON object")?;

    remove_zed_hooks(hooks);

    for event in agent.hook_events() {
        let command = hook_pointer_command(script_path, agent, event);
        let groups = hooks
            .entry(*event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("agent hook `{event}` must be a JSON array"))?;
        groups.push(serde_json::json!({
            "hooks": [{ "type": "command", "command": command, "timeout": 10 }]
        }));
    }
    Ok(())
}

/// The stable shell command registered in an agent's hook config. It no-ops
/// (exit 0) when not running inside a Zed terminal or when the script is
/// missing, so the agent works normally outside Zed.
fn hook_pointer_command(script_path: &Path, agent: TerminalAgentKind, event: &str) -> String {
    let script = shell_single_quote(&script_path.to_string_lossy());
    format!(
        "[ -n \"$ZED_AGENT_SESSION_STATE_FILE\" ] && [ -x {script} ] && {script} {agent} {event} || true",
        agent = agent.id(),
    )
}

fn remove_zed_hooks(hooks: &mut Map<String, Value>) {
    let emptied: Vec<String> = hooks
        .iter_mut()
        .filter_map(|(event, value)| {
            let groups = value.as_array_mut()?;
            groups.retain(|group| !group_contains_zed_command(group));
            groups.is_empty().then(|| event.clone())
        })
        .collect();
    for event in emptied {
        hooks.remove(&event);
    }
}

fn group_contains_zed_command(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_zed_hook_command)
            })
        })
}

/// Recognizes Zed-owned hook commands, including the legacy inline form that
/// embedded the whole shell program directly in the agent config.
fn is_zed_hook_command(command: &str) -> bool {
    command.contains("zed-agent-session-hook")
        || command.contains("ZED_AGENT_SESSION_STATE_FILE")
        || command.contains("ZED_CODEX_SESSION_STATE_FILE")
}

fn write_file_if_changed(path: &Path, contents: &[u8]) -> anyhow::Result<bool> {
    if std::fs::read(path).ok().as_deref() == Some(contents) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(true)
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script_path() -> PathBuf {
        PathBuf::from("/tmp/agent_hooks/zed-agent-session-hook.sh")
    }

    #[test]
    fn reads_latest_codex_thread_name_for_session() {
        let dir = std::env::temp_dir().join(format!("zed-codex-index-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session_index.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"id":"other","thread_name":"Nope","updated_at":"t0"}"#,
                "\n",
                r#"{"id":"sid","thread_name":"First","updated_at":"t1"}"#,
                "\n",
                "\n",
                r#"{"id":"sid","thread_name":"Second","updated_at":"t2"}"#,
                "\n",
            ),
        )
        .unwrap();

        // Most recent (last) matching entry wins.
        assert_eq!(
            read_codex_thread_name(&path, "sid").as_deref(),
            Some("Second")
        );
        // A session never renamed has no entry -> no name.
        assert_eq!(read_codex_thread_name(&path, "missing"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parses_active_and_ended_hook_payloads() {
        // SessionStart carries a session id but counts as idle (awaiting the
        // first prompt).
        let codex = parse_hook_payload(
            br#"{"agent":"codex","event":"SessionStart","payload":{"session_id":"019eb5c6-f9fa","cwd":"/x"}}"#,
        );
        assert!(matches!(
            codex,
            Some(TerminalAgentHookState::Active {
                agent: TerminalAgentKind::Codex,
                session_id: Some(session_id),
                activity: TerminalAgentActivity::Idle,
            }) if session_id == "019eb5c6-f9fa"
        ));

        // UserPromptSubmit means the agent is working.
        let claude = parse_hook_payload(
            br#"{"agent":"claude","event":"UserPromptSubmit","payload":{"session_id":"abc_123"}}"#,
        );
        assert!(matches!(
            claude,
            Some(TerminalAgentHookState::Active {
                agent: TerminalAgentKind::Claude,
                session_id: Some(session_id),
                activity: TerminalAgentActivity::Running,
            }) if session_id == "abc_123"
        ));

        let ended = parse_hook_payload(
            br#"{"agent":"claude","event":"SessionEnd","payload":{"session_id":"abc_123"}}"#,
        );
        assert!(matches!(
            ended,
            Some(TerminalAgentHookState::Ended {
                agent: TerminalAgentKind::Claude
            })
        ));
    }

    #[test]
    fn parses_waiting_for_confirmation_events() {
        // Claude permission prompts arrive as `Notification`, Codex as
        // `PermissionRequest`; both map to waiting, even without a session id.
        for payload in [
            br#"{"agent":"claude","event":"Notification","payload":{"session_id":"abc"}}"#
                .as_slice(),
            br#"{"agent":"codex","event":"PermissionRequest","payload":{}}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_hook_payload(payload),
                Some(TerminalAgentHookState::Active {
                    activity: TerminalAgentActivity::WaitingForConfirmation,
                    ..
                })
            ));
        }
    }

    #[test]
    fn maps_terminal_command_names_to_agents() {
        assert_eq!(
            TerminalAgentKind::from_command_name("claude"),
            Some(TerminalAgentKind::Claude)
        );
        assert_eq!(
            TerminalAgentKind::from_command_name("codex"),
            Some(TerminalAgentKind::Codex)
        );
        assert_eq!(
            TerminalAgentKind::from_command_name("codex-aarch64-apple-darwin"),
            Some(TerminalAgentKind::Codex)
        );
        assert_eq!(
            TerminalAgentKind::from_command_name("codex-extra"),
            Some(TerminalAgentKind::Codex)
        );
        assert_eq!(TerminalAgentKind::from_command_name("my-codex"), None);
    }

    #[test]
    fn rejects_unknown_agent_and_malformed_payloads() {
        assert!(
            parse_hook_payload(
                br#"{"agent":"grok","event":"SessionStart","payload":{"session_id":"x"}}"#
            )
            .is_none()
        );
        assert!(parse_hook_payload(br#"not json"#).is_none());
        // A valid agent/event with no payload is still a usable activity
        // signal; it just has no session id.
        assert!(matches!(
            parse_hook_payload(br#"{"agent":"codex","event":"SessionStart"}"#),
            Some(TerminalAgentHookState::Active {
                session_id: None,
                ..
            })
        ));
        // session ids with shell metacharacters are rejected, but the event is
        // still parsed (without a session id).
        assert!(matches!(
            parse_hook_payload(
                br#"{"agent":"codex","event":"Stop","payload":{"session_id":"a; rm -rf"}}"#
            ),
            Some(TerminalAgentHookState::Active {
                session_id: None,
                ..
            })
        ));
    }

    #[test]
    fn install_is_idempotent_and_preserves_foreign_hooks() {
        // A config with a user hook, a third-party (superset) hook, and a stale
        // legacy Zed inline hook.
        let mut config = serde_json::json!({
            "permissions": { "allow": ["*"] },
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "my-own-script.sh" }] },
                    { "hooks": [{ "type": "command", "command": "[ -n \"$SUPERSET_HOME_DIR\" ] && notify.sh || true" }] },
                    { "hooks": [{ "type": "command", "command": "sh -c '...' sh '{\"agent\":\"claude\"}' # ZED_AGENT_SESSION_STATE_FILE" }] }
                ]
            }
        });

        apply_zed_hooks(TerminalAgentKind::Claude, &mut config, &script_path()).unwrap();
        let after_first = config.clone();

        // Foreign hooks survive; the legacy Zed inline hook is gone.
        let stop = config["hooks"]["Stop"].as_array().unwrap();
        let commands: Vec<&str> = stop
            .iter()
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .map(|hook| hook["command"].as_str().unwrap())
            .collect();
        assert!(commands.contains(&"my-own-script.sh"));
        assert!(commands.iter().any(|c| c.contains("SUPERSET_HOME_DIR")));
        assert!(!commands.iter().any(|c| c.contains("sh -c '...'")));
        // Our pointer command was added for each Claude event.
        assert!(
            commands
                .iter()
                .any(|c| c.contains("zed-agent-session-hook.sh")
                    && c.ends_with("claude Stop || true"))
        );
        assert!(config["hooks"]["SessionEnd"].is_array());
        // Untouched config is preserved.
        assert_eq!(config["permissions"], serde_json::json!({ "allow": ["*"] }));

        // Second run is a no-op.
        apply_zed_hooks(TerminalAgentKind::Claude, &mut config, &script_path()).unwrap();
        assert_eq!(config, after_first);
    }

    #[test]
    fn codex_registers_lifecycle_events_without_session_end() {
        let mut config = serde_json::json!({});
        apply_zed_hooks(TerminalAgentKind::Codex, &mut config, &script_path()).unwrap();
        let hooks = config["hooks"].as_object().unwrap();
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "Stop",
        ] {
            assert!(hooks.contains_key(event), "missing Codex hook {event}");
        }
        // Codex has no session-end hook; dead sessions are cleared by
        // foreground-process detection.
        assert!(!hooks.contains_key("SessionEnd"));
    }
}
