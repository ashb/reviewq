//! Interest-rule evaluation.
//!
//! Pure: given a PR and a compiled rule set, decide whether it is interesting
//! and name the rule that made it so. The name is what the ledger stores as a
//! PR's `tracked_reason` and what `reviewq show` prints, so it is deliberately
//! stable and human-readable.
//!
//! Interest is a **disjunction of rules** — a PR is interesting if any rule
//! matches. A rule is a **conjunction of conditions** — every condition must
//! match, and within a condition any listed value is enough. So "one of these
//! authors *and* one of these paths" is one rule with two conditions.
//!
//! Repo scoping lives a layer up: each project compiles its own [`Interest`],
//! so nothing here needs to know which repo a PR came from.

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::model::PrSnapshot;

/// A rule as given by config, before its path globs are compiled.
#[derive(Debug, Clone)]
pub struct RuleInput {
    /// Optional label; when set it is the rule's reason string.
    pub name: Option<String>,
    /// Conditions, all of which must match.
    pub conditions: Vec<ConditionInput>,
    /// This rule's PRs stay worth reviewing after they merge.
    pub after_merge: bool,
}

/// One condition of a rule, before compilation. Each carries the set of values
/// that satisfy it (any one is enough — an OR within the dimension).
#[derive(Debug, Clone)]
pub enum ConditionInput {
    /// PR carries any of these labels.
    Labels(Vec<String>),
    /// A changed path matches any of these globs.
    Paths(Vec<String>),
    /// The author is any of these logins.
    Authors(Vec<String>),
    /// The author's association is any of these.
    AuthorAssociations(Vec<String>),
    /// The milestone title contains any of these substrings.
    Milestones(Vec<String>),
}

/// The outcome of evaluating a PR against the whole rule set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluation {
    /// Interesting; carries the reason (rule name, else the matched conditions).
    Match(String),
    /// Definitely not interesting: every rule was decided and none matched.
    NoMatch,
    /// A rule turned on a path condition whose file list has not been fetched;
    /// the caller should fetch files and re-evaluate. (Moot once the sweep
    /// carries files, but modelled for correctness.)
    NeedsFiles,
    /// A path condition's file list was fetched but truncated, and nothing in
    /// the part we saw matched. Unknown, not a no-match — surfaced not dropped.
    Unknown,
}

/// A compiled interest rule set for one project.
#[derive(Debug, Clone)]
pub struct Interest {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    name: Option<String>,
    conditions: Vec<Condition>,
    after_merge: bool,
}

#[derive(Debug, Clone)]
enum Condition {
    Labels(Vec<String>),
    Paths { set: GlobSet, patterns: Vec<String> },
    Authors(Vec<String>),
    AuthorAssociations(Vec<String>),
    Milestones(Vec<String>),
}

/// How a single condition came out. `Match` carries its rendered reason
/// fragment so the whole rule can name itself.
enum CondOutcome {
    Match(String),
    NoMatch,
    NeedsFiles,
    Unknown,
}

/// How a whole rule (conjunction of conditions) came out.
enum RuleOutcome {
    Match(String),
    NoMatch,
    NeedsFiles,
    Unknown,
}

impl Interest {
    /// Compile a rule set. The only fallible part is the path globs.
    pub fn compile(rules: Vec<RuleInput>) -> Result<Self, globset::Error> {
        let mut compiled = Vec::with_capacity(rules.len());
        for rule in rules {
            let mut conditions = Vec::with_capacity(rule.conditions.len());
            for condition in rule.conditions {
                conditions.push(Condition::compile(condition)?);
            }
            compiled.push(Rule {
                name: rule.name,
                conditions,
                after_merge: rule.after_merge,
            });
        }
        Ok(Self { rules: compiled })
    }

    /// Whether there are no rules at all.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Evaluate `pr` against every rule, returning the first match. If nothing
    /// matched but a rule couldn't be decided without files, that is surfaced
    /// rather than reported as a no-match.
    pub fn evaluate(&self, pr: &PrSnapshot) -> Evaluation {
        let mut needs_files = false;
        let mut unknown = false;
        for rule in &self.rules {
            match rule.evaluate(pr) {
                RuleOutcome::Match(reason) => return Evaluation::Match(reason),
                RuleOutcome::NoMatch => {}
                RuleOutcome::NeedsFiles => needs_files = true,
                RuleOutcome::Unknown => unknown = true,
            }
        }
        if needs_files {
            Evaluation::NeedsFiles
        } else if unknown {
            Evaluation::Unknown
        } else {
            Evaluation::NoMatch
        }
    }

    /// Whether any rule that matches `pr` asks to keep it reviewable after it
    /// merges.
    ///
    /// Asked separately from [`evaluate`](Self::evaluate), and of *every*
    /// matching rule rather than the first: interest is a disjunction, so a rule
    /// saying "these paths, this author, still worth a look post-merge" must be
    /// heard even when a broader rule happened to name the PR first.
    pub fn keeps_after_merge(&self, pr: &PrSnapshot) -> bool {
        self.rules
            .iter()
            .filter(|rule| rule.after_merge)
            .any(|rule| matches!(rule.evaluate(pr), RuleOutcome::Match(_)))
    }
}

impl Rule {
    fn evaluate(&self, pr: &PrSnapshot) -> RuleOutcome {
        let mut matched = Vec::with_capacity(self.conditions.len());
        let mut needs_files = false;
        let mut unknown = false;

        for condition in &self.conditions {
            match condition.evaluate(pr) {
                // Any definite non-match fails the whole conjunction.
                CondOutcome::NoMatch => return RuleOutcome::NoMatch,
                CondOutcome::Match(reason) => matched.push(reason),
                CondOutcome::NeedsFiles => needs_files = true,
                CondOutcome::Unknown => unknown = true,
            }
        }

        // Nothing said no, but something couldn't be decided.
        if needs_files {
            return RuleOutcome::NeedsFiles;
        }
        if unknown {
            return RuleOutcome::Unknown;
        }
        // Every condition matched. Prefer the rule's name; else describe what
        // matched (joined for a future multi-condition rule).
        let reason = self.name.clone().unwrap_or_else(|| matched.join(" + "));
        RuleOutcome::Match(reason)
    }
}

impl Condition {
    fn compile(input: ConditionInput) -> Result<Self, globset::Error> {
        Ok(match input {
            ConditionInput::Labels(v) => Condition::Labels(v),
            ConditionInput::Authors(v) => Condition::Authors(v),
            ConditionInput::AuthorAssociations(v) => Condition::AuthorAssociations(v),
            ConditionInput::Milestones(v) => Condition::Milestones(v),
            ConditionInput::Paths(patterns) => {
                let mut builder = GlobSetBuilder::new();
                for pattern in &patterns {
                    builder.add(Glob::new(pattern)?);
                }
                Condition::Paths {
                    set: builder.build()?,
                    patterns,
                }
            }
        })
    }

    fn evaluate(&self, pr: &PrSnapshot) -> CondOutcome {
        match self {
            Condition::Labels(values) => {
                for label in &pr.labels {
                    if values.iter().any(|v| v == label) {
                        return CondOutcome::Match(format!("label {label}"));
                    }
                }
                CondOutcome::NoMatch
            }
            // Logins are compared case-insensitively: GitHub treats `Potiuk` and
            // `potiuk` as one account, so a rule naming either has to match the
            // casing the sweep happened to report.
            Condition::Authors(logins) => {
                if logins.iter().any(|v| v.eq_ignore_ascii_case(&pr.author)) {
                    CondOutcome::Match(format!("author @{}", pr.author))
                } else {
                    CondOutcome::NoMatch
                }
            }
            Condition::AuthorAssociations(values) => {
                if values.iter().any(|v| v == &pr.author_association) {
                    CondOutcome::Match(format!("author {}", pr.author_association))
                } else {
                    CondOutcome::NoMatch
                }
            }
            Condition::Milestones(needles) => match &pr.milestone {
                Some(title) => match needles.iter().find(|n| title.contains(n.as_str())) {
                    Some(needle) => CondOutcome::Match(format!("milestone {needle}")),
                    None => CondOutcome::NoMatch,
                },
                None => CondOutcome::NoMatch,
            },
            Condition::Paths { set, patterns } => match &pr.files {
                None => CondOutcome::NeedsFiles,
                Some(files) => {
                    for path in files {
                        if let Some(&index) = set.matches(path).first() {
                            return CondOutcome::Match(format!("path {}", patterns[index]));
                        }
                    }
                    if pr.files_truncated {
                        CondOutcome::Unknown
                    } else {
                        CondOutcome::NoMatch
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> PrSnapshot {
        PrSnapshot {
            number: 1,
            title: "t".into(),
            author: "octocat".into(),
            author_association: "CONTRIBUTOR".into(),
            head_sha: "abc".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: crate::model::PrState::Open,
            updated_at: "2026-08-05T12:00:00Z".parse().unwrap(),
            created_at: None,
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    fn rule(conditions: Vec<ConditionInput>) -> RuleInput {
        RuleInput {
            name: None,
            conditions,
            after_merge: false,
        }
    }

    fn labels_rule() -> RuleInput {
        rule(vec![ConditionInput::Labels(vec!["area:task-sdk".into()])])
    }

    fn paths_rule() -> RuleInput {
        rule(vec![ConditionInput::Paths(vec!["task-sdk/**".into()])])
    }

    fn interest(rules: Vec<RuleInput>) -> Interest {
        Interest::compile(rules).unwrap()
    }

    #[test]
    fn label_rule_matches_and_renders() {
        let mut pr = pr();
        pr.labels = vec!["area:task-sdk".into()];
        assert_eq!(
            interest(vec![labels_rule()]).evaluate(&pr),
            Evaluation::Match("label area:task-sdk".into())
        );
    }

    #[test]
    fn a_named_rule_reports_its_name() {
        let mut pr = pr();
        pr.labels = vec!["area:task-sdk".into()];
        let rule = RuleInput {
            name: Some("task-sdk".into()),
            ..labels_rule()
        };
        assert_eq!(
            interest(vec![rule]).evaluate(&pr),
            Evaluation::Match("task-sdk".into())
        );
    }

    #[test]
    fn author_and_milestone_rules_match() {
        let mut author_pr = pr();
        author_pr.author_association = "FIRST_TIME_CONTRIBUTOR".into();
        let associations = rule(vec![ConditionInput::AuthorAssociations(vec![
            "FIRST_TIME_CONTRIBUTOR".into(),
        ])]);
        assert_eq!(
            interest(vec![associations]).evaluate(&author_pr),
            Evaluation::Match("author FIRST_TIME_CONTRIBUTOR".into())
        );

        let mut pr = pr();
        pr.milestone = Some("3.2.0".into());
        let milestones = rule(vec![ConditionInput::Milestones(vec!["3.2".into()])]);
        assert_eq!(
            interest(vec![milestones]).evaluate(&pr),
            Evaluation::Match("milestone 3.2".into())
        );
    }

    #[test]
    fn an_author_rule_names_a_login_and_ignores_its_casing() {
        // What `author_associations` cannot express: GitHub's relationship
        // classes say nothing about *which* person opened the PR.
        let mine = rule(vec![ConditionInput::Authors(vec!["OctoCat".into()])]);
        assert_eq!(
            interest(vec![mine.clone()]).evaluate(&pr()),
            Evaluation::Match("author @octocat".into())
        );

        let mut someone_else = pr();
        someone_else.author = "potiuk".into();
        assert_eq!(
            interest(vec![mine]).evaluate(&someone_else),
            Evaluation::NoMatch
        );
    }

    #[test]
    fn disjunction_matches_the_first_rule_that_fires() {
        let mut pr = pr();
        pr.labels = vec!["area:task-sdk".into()];
        pr.files = Some(vec!["task-sdk/x.py".into()]);
        // Both the label and path rule would match; the first is reported.
        assert_eq!(
            interest(vec![labels_rule(), paths_rule()]).evaluate(&pr),
            Evaluation::Match("label area:task-sdk".into())
        );
    }

    #[test]
    fn path_rule_needs_files_when_absent() {
        assert_eq!(
            interest(vec![paths_rule()]).evaluate(&pr()),
            Evaluation::NeedsFiles
        );
    }

    #[test]
    fn path_rule_matches_when_files_present() {
        let mut pr = pr();
        pr.files = Some(vec!["task-sdk/src/x.py".into()]);
        assert_eq!(
            interest(vec![paths_rule()]).evaluate(&pr),
            Evaluation::Match("path task-sdk/**".into())
        );
    }

    #[test]
    fn truncated_non_match_is_unknown() {
        let mut pr = pr();
        pr.files = Some(vec!["docs/x.rst".into()]);
        pr.files_truncated = true;
        assert_eq!(
            interest(vec![paths_rule()]).evaluate(&pr),
            Evaluation::Unknown
        );
    }

    #[test]
    fn only_a_rule_that_asks_for_it_keeps_a_pr_after_it_merges() {
        let keeps = RuleInput {
            after_merge: true,
            ..paths_rule()
        };
        let mut in_task_sdk = pr();
        in_task_sdk.files = Some(vec!["task-sdk/x.py".into()]);

        assert!(interest(vec![keeps.clone()]).keeps_after_merge(&in_task_sdk));
        assert!(
            !interest(vec![paths_rule()]).keeps_after_merge(&in_task_sdk),
            "the same rule without the flag lets it go at merge"
        );

        let mut elsewhere = pr();
        elsewhere.files = Some(vec!["docs/x.rst".into()]);
        assert!(
            !interest(vec![keeps]).keeps_after_merge(&elsewhere),
            "a rule only keeps the PRs it matches"
        );
    }

    #[test]
    fn a_later_rule_can_keep_a_pr_an_earlier_one_named_first() {
        // Interest is a disjunction, so every matching rule is asked. Only
        // consulting the rule that produced the reason would silently ignore the
        // post-merge one whenever a broader rule happened to match too.
        let mut pr = pr();
        pr.labels = vec!["area:task-sdk".into()];
        pr.files = Some(vec!["task-sdk/x.py".into()]);
        let rules = interest(vec![
            labels_rule(),
            RuleInput {
                after_merge: true,
                ..paths_rule()
            },
        ]);

        assert_eq!(
            rules.evaluate(&pr),
            Evaluation::Match("label area:task-sdk".into()),
            "the first rule still names it"
        );
        assert!(rules.keeps_after_merge(&pr));
    }

    #[test]
    fn no_rules_is_never_interesting() {
        let interest = interest(vec![]);
        assert!(interest.is_empty());
        assert_eq!(interest.evaluate(&pr()), Evaluation::NoMatch);
    }

    #[test]
    fn conjunction_requires_every_condition() {
        let both = rule(vec![
            ConditionInput::AuthorAssociations(vec!["FIRST_TIME_CONTRIBUTOR".into()]),
            ConditionInput::Paths(vec!["task-sdk/**".into()]),
        ]);
        let mut pr = pr();
        pr.author_association = "FIRST_TIME_CONTRIBUTOR".into();
        pr.files = Some(vec!["docs/x.rst".into()]);
        // Author matches, path does not → the conjunction fails.
        assert_eq!(
            interest(vec![both.clone()]).evaluate(&pr),
            Evaluation::NoMatch
        );

        pr.files = Some(vec!["task-sdk/x.py".into()]);
        // Both match → reason joins the matched fragments.
        assert_eq!(
            interest(vec![both]).evaluate(&pr),
            Evaluation::Match("author FIRST_TIME_CONTRIBUTOR + path task-sdk/**".into())
        );
    }
}
