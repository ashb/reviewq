CREATE TABLE labels (
  repo_id INTEGER NOT NULL REFERENCES repos(id),
  name    TEXT NOT NULL,
  color   TEXT NOT NULL,
  PRIMARY KEY (repo_id, name)
);
