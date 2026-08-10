-- System principals: a real, resolvable UUID identity for a non-human note
-- author (e.g. an AI triage bot, an inbound-email ingestion service) --
-- deliberately NOT a nullable `notes.created_by` and NOT a fake human team
-- member. `notes_acl::can_edit`/`can_view` both key on `created_by ==
-- user.user_id` with no null branch, and `TackUser::user_id` is non-optional
-- everywhere in this codebase -- a nullable `created_by` would ripple into
-- every ACL and roster-resolution call site for a feature (system-authored
-- notes) that's the exception, not the rule. A system principal is a real
-- row with a real id instead: `notes.created_by` can point at one exactly
-- as it points at a real user, and the existing `is_admin || created_by ==
-- user.user_id` rule falls out correctly for free -- a system principal's
-- own id never matches a real caller's, so its notes are editable/deletable
-- by admins only, which is the right behavior with no special-casing.
--
-- Org-scoped, not team-scoped: the bot identities driving this (an AI
-- triage service, inbound-email ingestion) are conceptually org-wide
-- services, not tied to one team, and every other tenant-scoped table here
-- already partitions by organization_id -- this one follows the same
-- convention for consistency even though it has no `team_id` column of its
-- own to justify the partition key otherwise.
--
-- No FK from notes.created_by to this table (or to a users table) -- same
-- reasoning as every other polymorphic-friendly id in this schema
-- (content_attachments/content_references have none either): resolving
-- whether a given `created_by` is a real user or a system principal is a
-- live lookup the client does at read time (this table first, falling back
-- to the team roster), never denormalized.
CREATE TABLE IF NOT EXISTS system_principals (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    -- Display label shown in place of a resolved username, e.g. "AI Triage",
    -- "Inbound Email". Not unique -- nothing stops two principals sharing a
    -- label, same as nothing stops two real users sharing a display name.
    label           TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id)
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS system_principals_p%1$s PARTITION OF system_principals FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;
