-- PR numbers are only unique within a repository, so every PR-scoped table
-- gains repo_id as part of its key. A pre-v4 ledger contains only one repo;
-- its rows are assigned to a placeholder that Ledger::ensure_repo adopts when
-- the real repository is first resolved.
CREATE TABLE repos (
  id    INTEGER PRIMARY KEY,
  host  TEXT NOT NULL,
  owner TEXT NOT NULL,
  name  TEXT NOT NULL,
  UNIQUE (host, owner, name)
);

INSERT INTO repos (id, host, owner, name)
SELECT 1, '', '', ''
WHERE EXISTS (SELECT 1 FROM prs) OR EXISTS (SELECT 1 FROM sync_meta);

-- Every _v4 table below references the other _v4 tables, not the old ones —
-- so nothing here depends on the old tables' drop order, which matters
-- because SQLite's own foreign_keys enforcement refuses to DROP a table
-- while a FK-enforced child still holds rows referencing it (dropping old
-- `prs` first, with old `my_state` etc. still populated and still pointing
-- at it, fails exactly that check). All the old tables are dropped only
-- once every new one is fully built and populated.

CREATE TABLE prs_v4 (
  repo_id             INTEGER NOT NULL REFERENCES repos(id),
  number              INTEGER NOT NULL,
  title               TEXT NOT NULL,
  author              TEXT NOT NULL,
  author_association  TEXT NOT NULL,
  head_sha            TEXT NOT NULL,
  is_draft            INTEGER NOT NULL,
  state               TEXT NOT NULL,
  updated_at          TEXT NOT NULL,
  labels              TEXT NOT NULL,
  milestone           TEXT,
  files               TEXT,
  files_truncated     INTEGER NOT NULL DEFAULT 0,
  tracked_reason      TEXT,
  first_seen_at       TEXT NOT NULL,
  detail_synced_at    TEXT,
  PRIMARY KEY (repo_id, number)
);
INSERT INTO prs_v4
  SELECT 1, number, title, author, author_association, head_sha, is_draft,
         state, updated_at, labels, milestone, files, files_truncated,
         tracked_reason, first_seen_at, detail_synced_at
  FROM prs;

CREATE TABLE my_state_v4 (
  repo_id           INTEGER NOT NULL,
  number            INTEGER NOT NULL,
  last_reviewed_sha TEXT,
  last_verdict      TEXT,
  last_action_at    TEXT,
  done_sha          TEXT,
  snoozed_until     TEXT,
  muted             INTEGER NOT NULL DEFAULT 0,
  deferred_at       TEXT,
  done_at           TEXT,
  PRIMARY KEY (repo_id, number),
  FOREIGN KEY (repo_id, number) REFERENCES prs_v4(repo_id, number)
);
INSERT INTO my_state_v4
  SELECT 1, number, last_reviewed_sha, last_verdict, last_action_at, done_sha,
         snoozed_until, muted, deferred_at, done_at
  FROM my_state;

CREATE TABLE threads_v4 (
  thread_id           TEXT PRIMARY KEY,
  repo_id             INTEGER NOT NULL,
  pr_number           INTEGER NOT NULL,
  i_own               INTEGER NOT NULL,
  is_resolved         INTEGER NOT NULL,
  resolved_by         TEXT,
  last_comment_author TEXT,
  last_comment_at     TEXT,
  my_last_comment_at  TEXT,
  FOREIGN KEY (repo_id, pr_number) REFERENCES prs_v4(repo_id, number)
);
INSERT INTO threads_v4
  SELECT thread_id, 1, pr_number, i_own, is_resolved, resolved_by,
         last_comment_author, last_comment_at, my_last_comment_at
  FROM threads;

CREATE TABLE attention_v4 (
  repo_id     INTEGER NOT NULL,
  pr_number   INTEGER NOT NULL,
  reason      TEXT NOT NULL,
  detail      TEXT NOT NULL,
  since       TEXT NOT NULL,
  PRIMARY KEY (repo_id, pr_number, reason),
  FOREIGN KEY (repo_id, pr_number) REFERENCES prs_v4(repo_id, number)
);
INSERT INTO attention_v4
  SELECT 1, pr_number, reason, detail, since FROM attention;

CREATE TABLE reviewers_v4 (
  repo_id       INTEGER NOT NULL,
  pr_number     INTEGER NOT NULL,
  login         TEXT NOT NULL,
  verdict       TEXT NOT NULL,
  submitted_at  TEXT NOT NULL,
  PRIMARY KEY (repo_id, pr_number, login),
  FOREIGN KEY (repo_id, pr_number) REFERENCES prs_v4(repo_id, number)
);
INSERT INTO reviewers_v4
  SELECT 1, pr_number, login, verdict, submitted_at FROM reviewers;

CREATE TABLE sync_meta_v4 (
  repo_id INTEGER NOT NULL REFERENCES repos(id),
  key     TEXT NOT NULL,
  value   TEXT NOT NULL,
  PRIMARY KEY (repo_id, key)
);
INSERT INTO sync_meta_v4
  SELECT 1, key, value FROM sync_meta;

-- Children before the parent: an old child with rows still referencing the
-- old `prs` blocks dropping `prs` first.
DROP TABLE my_state;
DROP TABLE threads;
DROP TABLE attention;
DROP TABLE reviewers;
DROP TABLE prs;
DROP TABLE sync_meta;

ALTER TABLE prs_v4 RENAME TO prs;
ALTER TABLE my_state_v4 RENAME TO my_state;
ALTER TABLE threads_v4 RENAME TO threads;
ALTER TABLE attention_v4 RENAME TO attention;
ALTER TABLE reviewers_v4 RENAME TO reviewers;
ALTER TABLE sync_meta_v4 RENAME TO sync_meta;
