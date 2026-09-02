ALTER TABLE prs ADD COLUMN state_changed_at TEXT;

CREATE TABLE activity_events (
  id          INTEGER PRIMARY KEY,
  repo_id     INTEGER NOT NULL,
  pr_number   INTEGER NOT NULL,
  source      TEXT NOT NULL,
  kind        TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  recorded_at TEXT NOT NULL,
  actor       TEXT,
  head_sha    TEXT,
  external_id TEXT,
  permalink   TEXT,
  payload     TEXT NOT NULL,
  observed_transition INTEGER NOT NULL DEFAULT 0,
  relation TEXT NOT NULL DEFAULT 'context' CHECK (relation IN ('own', 'relevant', 'context')),
  FOREIGN KEY (repo_id, pr_number)
    REFERENCES prs(repo_id, number)
    ON DELETE RESTRICT
);

CREATE TABLE activity_sync_state (
  repo_id      INTEGER NOT NULL,
  pr_number    INTEGER NOT NULL,
  cursor       TEXT,
  completed_at TEXT,
  next_rate_limit TEXT,
  incremental_cursor TEXT,
  incremental_next_rate_limit TEXT,
  requested_generation BIGINT NOT NULL DEFAULT 0,
  completed_generation BIGINT NOT NULL DEFAULT 0,
  incremental_generation BIGINT,
  incremental_revision BIGINT NOT NULL DEFAULT 0,
  incremental_stop_at TEXT,
  covered_through TEXT,
  backfill_started_at TEXT,
  incremental_started_at TEXT,
  PRIMARY KEY (repo_id, pr_number),
  FOREIGN KEY (repo_id, pr_number)
    REFERENCES prs(repo_id, number)
    ON DELETE RESTRICT
);

CREATE TABLE activity_retention (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  cutoff    TEXT NOT NULL
);

ALTER TABLE threads ADD COLUMN resolution_event_id BIGINT
  REFERENCES activity_events(id) ON DELETE RESTRICT;

ALTER TABLE my_state ADD COLUMN last_review_event_id BIGINT
  REFERENCES activity_events(id) ON DELETE RESTRICT;

CREATE INDEX activity_events_by_pr_time
  ON activity_events (repo_id, pr_number, occurred_at, id);

CREATE INDEX activity_events_by_time
  ON activity_events (occurred_at, id);

CREATE UNIQUE INDEX activity_events_external
  ON activity_events (repo_id, kind, external_id)
  WHERE external_id IS NOT NULL;

INSERT INTO activity_events
    (repo_id, pr_number, source, kind, occurred_at, recorded_at, actor, external_id, payload, relation)
SELECT t.repo_id, t.pr_number, 'forge', 'thread_resolved',
    strftime('%Y-%m-%dT%H:%M:%S', COALESCE(p.detail_synced_at, p.first_seen_at)) || '.000000000Z',
    strftime('%Y-%m-%dT%H:%M:%S', COALESCE(p.detail_synced_at, p.first_seen_at)) || '.000000000Z',
    t.resolved_by, 'initial-resolution:' || t.thread_id,
    json_object('thread_state_changed', json_object('thread_id', t.thread_id,
        'resolved', json('true'), 'observed', json('true'))),
    CASE WHEN t.i_own = 1 OR t.my_last_comment_at IS NOT NULL THEN 'relevant' ELSE 'context' END
FROM threads t JOIN prs p ON p.repo_id = t.repo_id AND p.number = t.pr_number
WHERE t.is_resolved = 1;

UPDATE threads SET resolution_event_id = (
  SELECT e.id FROM activity_events e
  WHERE e.repo_id = threads.repo_id
    AND e.external_id = 'initial-resolution:' || threads.thread_id
    AND e.kind = 'thread_resolved' AND e.relation != 'context'
) WHERE is_resolved = 1;
