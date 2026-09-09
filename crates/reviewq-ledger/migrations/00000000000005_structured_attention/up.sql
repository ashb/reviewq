-- The old detail column stored rendered prose, which cannot be converted back
-- into a structured reason. Attention is derived data, so discard those rows
-- and make their PRs stale so the next detail sync reconstructs them.
-- Before dropping the rows, mark their PRs for a fresh detail pass.
UPDATE prs SET detail_synced_at = NULL
WHERE EXISTS (
  SELECT 1 FROM attention a
  WHERE a.repo_id = prs.repo_id AND a.pr_number = prs.number
);

CREATE TABLE attention_v5 (
  repo_id     INTEGER NOT NULL,
  pr_number   INTEGER NOT NULL,
  reason      TEXT NOT NULL,
  since       TEXT NOT NULL,
  payload     TEXT NOT NULL,
  PRIMARY KEY (repo_id, pr_number, reason),
  FOREIGN KEY (repo_id, pr_number) REFERENCES prs(repo_id, number)
);

DROP TABLE attention;
ALTER TABLE attention_v5 RENAME TO attention;
