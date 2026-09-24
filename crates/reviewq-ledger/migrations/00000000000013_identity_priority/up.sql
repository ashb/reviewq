CREATE TABLE team_memberships (
    host TEXT NOT NULL,
    organization TEXT NOT NULL,
    team TEXT NOT NULL,
    members TEXT NOT NULL,
    refreshed_at TEXT NOT NULL,
    PRIMARY KEY (host, organization, team)
);

ALTER TABLE attention ADD COLUMN priority BOOLEAN NOT NULL DEFAULT FALSE;

-- Existing requests need one detail fetch to learn who requested the review.
UPDATE prs SET detail_synced_at = NULL
WHERE EXISTS (
    SELECT 1 FROM attention
    WHERE attention.repo_id = prs.repo_id
      AND attention.pr_number = prs.number
      AND attention.reason = 'review_requested'
);
