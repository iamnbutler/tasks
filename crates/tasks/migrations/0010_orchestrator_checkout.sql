-- "Open in Claude Code": the orchestrator's CC session is a normal Claude
-- Code session a human can resume interactively (`claude --resume <id>`).
-- Sessions have no file locking, so while a human has it checked out the
-- headless tick must not write to it.
--
-- `workdir` is the effective ORCHESTRATOR_WORKDIR, written at startup so the
-- API can tell clients where to `cd` before resuming (resume-from-elsewhere
-- works, but would hand the agent a different cwd mid-session).
-- `checked_out_at` is a heartbeat, not a flag: the interactive wrapper
-- renews it every minute and ticks stay suspended while it's fresh, so a
-- killed terminal un-suspends by itself instead of wedging the loop.
ALTER TABLE orchestrator ADD COLUMN workdir TEXT;
ALTER TABLE orchestrator ADD COLUMN checked_out_at TEXT;
