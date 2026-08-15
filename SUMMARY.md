# Orchestrator chat: show the tick while it is happening

The orchestrator chat in `app-gpui` showed nothing at all between a message
being sent and the reply landing — no clock, no streamed text, and, for a
proactive tick nobody asked for, no sign anything had happened until the answer
appeared. The live feed the server has published all along
(`GET /orchestrator/stream`) simply had no subscriber. This change subscribes to
it: a second SSE thread (`tasks-orchestrator-feed`, its own channel and its own
connection lifetime) accumulates `Delta` text into a provisional assistant
bubble at the end of the conversation, shows the latest `Tool` label as that
bubble's status line, and retires the whole provisional view the moment the
durable `orchestrator_messages` row that replaces it lands. The chat row in the
left sidebar wears the elapsed clock as its badge, so a tick is visible from
every section rather than only from Chat.

The two design gaps in the issue are answered rather than deferred.
`OrchestratorFeedEvent` gains a `Started` variant (`{"kind":"started"}`),
published at the top of `tick()` before the charter read, the prompt build or
the agent spawn — a no-op tick still publishes nothing, so a client is never
told a tick began when none did. And the "is it working" indicator is driven by
turn lifecycle, an elapsed clock running from `Started` (or from the local send,
because the round trip to the tick loop is part of the wait) until the reply
lands, rather than by text arriving: that stays correct through extended
thinking, through slow tool calls, and for an operator who overrides
`ORCHESTRATOR_CMD` without the stream-json flags and therefore gets no deltas at
all. Retirement has three independent paths — the durable reply arriving, a feed
error, and the feed iterator ending — because `Done` can be dropped by a lagged
subscriber, a dead connection, or a server that died mid-tick, and a clock that
only `Done` can stop is a clock that runs forever. Two smaller notes: the
provisional bubble resets its accumulated text on each `Tool` frame, per the
rendering contract already written down in `docs/clients.md` (pre-tool text is
working narration; the reply is the segment after the last tool call), which is
also what makes the swap to the durable message flicker-free; and parsing
`thinking_tokens` into a distinct feed event is deliberately left out, as it is
the one piece that would collide with the queued `parse_stream_line` rework in
#827. Tests cover the new frame end to end — the tick's feed sequence, the SSE
relay, a proactive tick where `[Started, Done]` is the only thing a client can
see, a no-op tick announcing nothing, and a new `tasks-client` test exercising
`stream_orchestrator()` against a real server, which had no client-level
coverage before. The GUI itself was typechecked and linted but not run: there is
no display on the build host.
