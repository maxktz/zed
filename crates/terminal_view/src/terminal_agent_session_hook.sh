#!/bin/sh
# zed-agent-session-hook v1
#
# Installed and owned by Zed. Records the active terminal-agent session id so
# Zed can resume the session after a restart. Agents (Claude, Codex, ...) invoke
# this from their hook config with two arguments and the hook payload on stdin:
#
#   args:  $1 = agent id (e.g. "codex"), $2 = event name (e.g. "SessionStart")
#   stdin: the agent's raw hook payload JSON
#
# It writes {"agent":<id>,"event":<event>,"payload":<stdin>} atomically to
# $ZED_AGENT_SESSION_STATE_FILE, then prints "{}" so the hook is a no-op for the
# agent. Outside a Zed terminal $ZED_AGENT_SESSION_STATE_FILE is unset and the
# payload is discarded.
agent="$1"
event="$2"
if [ -n "$ZED_AGENT_SESSION_STATE_FILE" ]; then
  umask 077
  temp="$ZED_AGENT_SESSION_STATE_FILE.tmp.$$"
  {
    printf '{"agent":"%s","event":"%s","payload":' "$agent" "$event"
    cat
    printf '}\n'
  } >"$temp" 2>/dev/null && mv "$temp" "$ZED_AGENT_SESSION_STATE_FILE" 2>/dev/null
else
  cat >/dev/null
fi
printf '{}\n'
