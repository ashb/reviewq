CREATE TABLE prs (
  number            INTEGER PRIMARY KEY,
  title             TEXT NOT NULL,
  author            TEXT NOT NULL,
  author_association TEXT NOT NULL,
  head_sha          TEXT NOT NULL,
  is_draft          INTEGER NOT NULL,
  state             TEXT NOT NULL,
  updated_at        TEXT NOT NULL,
  labels            TEXT NOT NULL,
  milestone         TEXT,
  files             TEXT,
  files_truncated   INTEGER NOT NULL DEFAULT 0,
  tracked_reason    TEXT,
  first_seen_at     TEXT NOT NULL,
  detail_synced_at  TEXT
);

CREATE TABLE my_state (
  number            INTEGER PRIMARY KEY REFERENCES prs(number),
  last_reviewed_sha TEXT,
  last_verdict      TEXT,
  last_action_at    TEXT,
  done_sha          TEXT,
  snoozed_until     TEXT,
  muted             INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE threads (
  thread_id           TEXT PRIMARY KEY,
  pr_number           INTEGER NOT NULL REFERENCES prs(number),
  i_own               INTEGER NOT NULL,
  is_resolved         INTEGER NOT NULL,
  resolved_by         TEXT,
  last_comment_author TEXT,
  last_comment_at     TEXT,
  my_last_comment_at  TEXT
);

CREATE TABLE attention (
  pr_number         INTEGER NOT NULL REFERENCES prs(number),
  reason            TEXT NOT NULL,
  detail            TEXT NOT NULL,
  since             TEXT NOT NULL,
  PRIMARY KEY (pr_number, reason)
);

CREATE TABLE sync_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
