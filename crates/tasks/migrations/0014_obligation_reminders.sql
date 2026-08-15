-- Rate-limit bookkeeping for standing obligations. Deliberately NOT the
-- obligations themselves.
--
-- An obligation is *derived* from pipeline state -- "a spec sits in
-- pending_review with no decision row" -- and that is the whole point. The
-- old design made a nudge the only reason work happened, so an obligation
-- was a message, and a message is consumed: a tick that timed out after the
-- nudge was folded into its prompt settled the watermark anyway, and the spec
-- was never reviewed again. Nothing pointed at it, because the thing pointing
-- at it had been eaten.
--
-- Deriving instead means a timeout costs latency rather than the work. What
-- still has to be remembered is only how recently we mentioned each one, so
-- a standing obligation does not repeat every tick. That is rate limiting,
-- not authority: losing this table re-reminds, it never loses work.
CREATE TABLE obligation_reminders (
    kind             TEXT NOT NULL,
    subject_id       TEXT NOT NULL,
    last_surfaced_at TEXT NOT NULL,
    PRIMARY KEY (kind, subject_id)
);
