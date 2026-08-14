-- Stand-in table for the one admin route and one public route this template
-- ships. Delete it (and this migration) once real domain tables replace it —
-- nothing else in the template depends on its columns beyond `id`, `name`,
-- and `created_at`.

CREATE TABLE example_items (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    name text NOT NULL,
    is_public boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- The public route (`GET /example`) lists only `is_public = true` rows; the
-- admin route (`GET /admin/example`) lists everything. A partial index keeps
-- the public query cheap without penalizing the admin one.
CREATE INDEX example_items_public_idx ON example_items (created_at DESC) WHERE is_public;
CREATE INDEX example_items_created_at_idx ON example_items (created_at DESC);
