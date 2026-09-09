CREATE TABLE reviewers (
  pr_number     INTEGER NOT NULL REFERENCES prs(number),
  login         TEXT NOT NULL,
  verdict       TEXT NOT NULL,
  submitted_at  TEXT NOT NULL,
  PRIMARY KEY (pr_number, login)
);
