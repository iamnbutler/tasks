-- What the project's own test suite said about a build, run by the Builder
-- supervisor inside the VM rather than claimed by the agent in SUMMARY.md.
--
-- Two columns on the `directions`/`directions_author` precedent: one is
-- branched on, the other is only ever rendered. Splitting them is what keeps a
-- decision off prose — the same rule `FailureClass` follows one level up.
--
-- Additive and both nullable, so every existing row reads as "no run on
-- record", which is never green and routes its batch to a human. That is also
-- what a build from an un-rebuilt image reads as, so the two degrade
-- identically.
ALTER TABLE builds ADD COLUMN verification_status TEXT;
ALTER TABLE builds ADD COLUMN verification_detail TEXT;
