# Orchestrator chat as a stacked stream of messages and tool calls

The chat pane kept one text slot and one active-tool slot for the tick in
flight, so a turn that went text → tool → text overwrote itself and arrived as
a single collapsed line. The feed already carries the ordering — `Delta`
chunks come in generation order and every invocation gets its own `Tool` frame
— so the fix is entirely client-side: give the client somewhere to put the
sequence. This replaces the two slots with a flat row list in which durable
conversation turns and the tick's trail entries interleave in arrival order,
coalescing consecutive tool calls into one expandable group and keeping every
entry on screen after the turn ends. The ordering rules live in a new,
gpui-free `app-gpui/src/chat_log.rs` (15 unit tests) built on three
invariants: the live trail is always the tail, so inserting a durable turn
above it and appending at the end are the same operation; consecutive tool
calls coalesce; and concluding a trail drops its trailing text segment,
because per the feed's contract that segment *is* the reply and the reply
arrives durably — keeping both would print the answer twice.

Rendering follows from the row model. The list syncs by longest-common-prefix
diff plus one `splice`, which covers append, insert-above-the-trail, the reply
segment retiring, and a server whose seqs started over while everything above
the change point keeps its measurements and scroll position; content growth
re-measures the last row instead, since `splice` resets the offset within the
item and makes streaming stutter. The elapsed clock moves out of the list into
the fixed footer, so it stays visible when the human has scrolled up and a
value that changes once a second stops re-measuring a list item every second.
`MarkdownCache::clear` becomes `remove`, so a chat reset no longer blows away
spec and briefing parses and the cache stops growing by one orphaned parse per
turn. Two smaller fixes came with the work: element ids for the copy
affordance and tool groups key off seq/entry id rather than row index, which
would otherwise move under the pointer when a trail's reply segment retires;
and an `owed_replies` counter fixes a pre-existing ordering bug this exposes,
where a reply the client learns about asynchronously can be overtaken by the
next tick's `Started` and retire the wrong tick. Persistence is answered
**session-local scrollback** — a concluded trail lives for the life of the app
process, and a restart or reconnect collapses the conversation back to the
durable `orchestrator_messages`, because writing down a feed the server
documents as ephemeral would be the wrong side of that contract.
`docs/clients.md` now records both the minimal and the stacked shape as
legitimate, with that caveat and the shared "drop the trailing segment when
the reply lands" rule.
