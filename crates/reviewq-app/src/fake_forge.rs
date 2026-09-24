//! A [`Forge`] that serves scripted responses, for the tests in this crate.
//!
//! One fake, shared: `sync`'s engine tests drive whole passes through it, and
//! `review`'s tests use it so that working out a handoff never resolves a real
//! token — resolution runs whatever the host configures, up to a credential
//! helper that can block on an interactive unlock, which `cargo test` must not be
//! able to trigger.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use jiff::Timestamp;
use reviewq_core::model::{PrSnapshot, PrState};
use reviewq_forge::{
    ActivityRateLimit, FetchedPr, Forge, ForgeActivity, ForgeActivityPage, ForgeError, PrDetail,
    RateLimit, RateLimitUnit, Result, SweepPage, Viewer,
};

/// Parse a timestamp, for the fixtures below and the tests that build on them.
pub(crate) fn ts(s: &str) -> Timestamp {
    s.parse().expect("timestamp")
}

/// A swept PR: tracked by the label rule the test configs use, with files, so
/// classification is never `NeedsFiles`.
pub(crate) fn pr(number: u64, updated: &str) -> PrSnapshot {
    PrSnapshot {
        number,
        title: format!("PR {number}"),
        author: "potiuk".into(),
        author_association: "MEMBER".into(),
        head_sha: format!("sha{number}"),
        base_ref: "main".into(),
        is_draft: false,
        state: PrState::Open,
        updated_at: ts(updated),
        created_at: Some(ts("2026-07-28T11:00:00Z")),
        state_changed_at: None,
        labels: vec!["area:task-sdk".into()],
        milestone: None,
        files: Some(vec!["task-sdk/src/thing.py".into()]),
        files_truncated: false,
    }
}

fn rate_limit(remaining: u32) -> RateLimit {
    RateLimit {
        limit: 5000,
        cost: 1,
        remaining,
        reset_at: ts("2026-08-11T13:00:00Z"),
    }
}

/// One page a scripted forge will serve.
#[derive(Clone)]
pub(crate) struct Page {
    prs: Vec<PrSnapshot>,
    next: Option<String>,
    total_count: u32,
    remaining: u32,
}

impl Page {
    pub(crate) fn of(prs: Vec<PrSnapshot>) -> Self {
        let total_count = prs.len() as u32;
        Self {
            prs,
            next: None,
            total_count,
            remaining: 4900,
        }
    }

    pub(crate) fn then(mut self, cursor: &str) -> Self {
        self.next = Some(cursor.to_string());
        self
    }

    /// Claim more matches than the page carries, as a truncated window does.
    pub(crate) fn of_total(mut self, total: u32) -> Self {
        self.total_count = total;
        self
    }
}

/// What the fake was asked, so a test can assert on the questions as well as
/// the answers.
#[derive(Default)]
pub(crate) struct Asked {
    searches: Vec<(String, Option<String>)>,
    details: Vec<u64>,
    activities: Vec<(u64, Option<String>)>,
}

/// A forge that serves scripted pages and details, and records its calls.
pub(crate) struct FakeForge {
    /// Search pages, served in order; the last is reused if asked again.
    pages: Mutex<std::collections::VecDeque<Page>>,
    /// Per-PR detail. A number absent from here is a PR the forge no longer
    /// has, which is the deleted-PR path.
    details: Mutex<std::collections::HashMap<u64, PrDetail>>,
    /// The colours a direct fetch reports.
    fetched_labels: Mutex<Vec<reviewq_forge::LabelColour>>,
    /// Numbers a direct fetch reports as absent.
    missing_prs: Mutex<std::collections::HashSet<u64>>,
    /// The repo's whole palette, as `fetch_labels` reports it.
    repo_labels: Mutex<Vec<reviewq_forge::LabelColour>>,
    /// Numbers whose detail fetch should fail outright.
    detail_errors: Mutex<std::collections::HashSet<u64>>,
    /// Activity pages selected by pull request and their opaque cursor.
    activity_pages: Mutex<std::collections::HashMap<(u64, Option<String>), ForgeActivityPage>>,
    /// Activity requests that should fail outright.
    activity_errors: Mutex<std::collections::HashSet<(u64, Option<String>)>>,
    activity_delays: Mutex<std::collections::HashMap<(u64, Option<String>), std::time::Duration>>,
    initial_activity_rate_limit: RateLimitUnit,
    point_budget_error: bool,
    activity_in_flight: AtomicUsize,
    max_activity_in_flight: AtomicUsize,
    asked: Mutex<Asked>,
    teams: Mutex<std::collections::HashMap<(String, String), Vec<String>>>,
    team_calls: AtomicUsize,
}

impl FakeForge {
    pub(crate) fn with_team(self, org: &str, team: &str, members: &[&str]) -> Self {
        self.teams.lock().unwrap().insert(
            (org.into(), team.into()),
            members.iter().map(|login| (*login).into()).collect(),
        );
        self
    }

    pub(crate) fn team_calls(&self) -> usize {
        self.team_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn new(pages: Vec<Page>) -> Self {
        Self {
            pages: Mutex::new(pages.into()),
            details: Mutex::new(std::collections::HashMap::new()),
            fetched_labels: Mutex::new(Vec::new()),
            missing_prs: Mutex::new(std::collections::HashSet::new()),
            repo_labels: Mutex::new(Vec::new()),
            detail_errors: Mutex::new(std::collections::HashSet::new()),
            activity_pages: Mutex::new(std::collections::HashMap::new()),
            activity_errors: Mutex::new(std::collections::HashSet::new()),
            activity_delays: Mutex::new(std::collections::HashMap::new()),
            initial_activity_rate_limit: RateLimitUnit::Points,
            point_budget_error: false,
            activity_in_flight: AtomicUsize::new(0),
            max_activity_in_flight: AtomicUsize::new(0),
            asked: Mutex::new(Asked::default()),
            teams: Mutex::new(std::collections::HashMap::new()),
            team_calls: AtomicUsize::new(0),
        }
    }

    /// Give `number` a detail response that holds nothing of interest.
    pub(crate) fn with_detail(self, number: u64, remaining: u32) -> Self {
        self.details.lock().expect("lock").insert(
            number,
            PrDetail {
                activities: Vec::new(),
                number,
                state: reviewq_core::model::PrState::Open,
                state_changed_at: None,
                head_sha: format!("sha{number}"),
                body: String::new(),
                last_reviewed_sha: None,
                last_verdict: None,
                last_action_at: None,
                threads: vec![],
                reviewers: vec![],
                mentions: vec![],
                said: vec![],
                invited: vec![],
                new_commits: 0,
                review_requests: vec![],
                cost: 1,
                remaining,
            },
        );
        self
    }

    /// Say that `number`'s detail finds it in `state` — what the forge reports
    /// after somebody closes or merges a PR the ledger still has as open.
    pub(crate) fn with_detail_transition(
        self,
        number: u64,
        state: reviewq_core::model::PrState,
        state_changed_at: Option<Timestamp>,
    ) -> Self {
        if let Some(detail) = self.details.lock().expect("lock").get_mut(&number) {
            detail.state = state;
            detail.state_changed_at = state_changed_at;
        }
        self
    }

    /// The palette the repo defines, as `fetch_labels` reports it.
    pub(crate) fn with_repo_labels(self, labels: &[(&str, &str)]) -> Self {
        *self.repo_labels.lock().expect("lock") = labels
            .iter()
            .map(|(name, color)| reviewq_forge::LabelColour {
                name: (*name).to_string(),
                color: (*color).to_string(),
            })
            .collect();
        self
    }

    /// Paint the labels a direct fetch reports, as the forge does.
    pub(crate) fn with_fetched_labels(self, labels: &[(&str, &str)]) -> Self {
        *self.fetched_labels.lock().expect("lock") = labels
            .iter()
            .map(|(name, color)| reviewq_forge::LabelColour {
                name: (*name).to_string(),
                color: (*color).to_string(),
            })
            .collect();
        self
    }

    pub(crate) fn missing_pr(self, number: u64) -> Self {
        self.missing_prs.lock().expect("lock").insert(number);
        self
    }

    /// Give `number` a detail response that puts it on the queue: someone
    /// asked me to review it.
    pub(crate) fn with_review_request(self, number: u64, remaining: u32) -> Self {
        let this = self.with_detail(number, remaining);
        if let Some(detail) = this.details.lock().expect("lock").get_mut(&number) {
            detail
                .review_requests
                .push(reviewq_core::model::ReviewRequest {
                    team: None,
                    requested_by: None,
                    requested_at: Some("2026-08-09T09:00:00Z".parse().expect("valid timestamp")),
                });
        }
        this
    }

    pub(crate) fn with_requester(self, number: u64, requester: &str) -> Self {
        self.details
            .lock()
            .unwrap()
            .get_mut(&number)
            .unwrap()
            .review_requests[0]
            .requested_by = Some(requester.into());
        self
    }

    pub(crate) fn with_current_head_review(self, number: u64) -> Self {
        if let Some(detail) = self.details.lock().expect("lock").get_mut(&number) {
            detail.last_reviewed_sha = Some(format!("sha{number}"));
            detail.last_verdict = Some(reviewq_core::model::Verdict::Commented);
        }
        self
    }

    pub(crate) fn with_detail_activity(
        self,
        number: u64,
        threads: Vec<reviewq_core::model::ThreadState>,
        activities: Vec<reviewq_forge::ForgeActivity>,
    ) -> Self {
        let mut details = self.details.lock().expect("lock");
        let detail = details.get_mut(&number).expect("detail");
        detail.threads = threads;
        detail.activities = activities;
        drop(details);
        self
    }

    pub(crate) fn failing_detail(self, number: u64) -> Self {
        self.detail_errors.lock().expect("lock").insert(number);
        self
    }

    pub(crate) fn searches(&self) -> Vec<(String, Option<String>)> {
        self.asked.lock().expect("lock").searches.clone()
    }

    pub(crate) fn details_asked(&self) -> Vec<u64> {
        self.asked.lock().expect("lock").details.clone()
    }

    /// Script one activity response for a PR and provider cursor.
    pub(crate) fn with_activity_page(
        self,
        number: u64,
        cursor: Option<&str>,
        page: ForgeActivityPage,
    ) -> Self {
        self.activity_pages
            .lock()
            .expect("lock")
            .insert((number, cursor.map(str::to_string)), page);
        self
    }

    pub(crate) fn activities_asked(&self) -> Vec<(u64, Option<String>)> {
        self.asked.lock().expect("lock").activities.clone()
    }

    pub(crate) fn failing_activity_page(self, number: u64, cursor: Option<&str>) -> Self {
        self.activity_errors
            .lock()
            .expect("lock")
            .insert((number, cursor.map(str::to_string)));
        self
    }

    pub(crate) fn delaying_activity_page(
        self,
        number: u64,
        cursor: Option<&str>,
        delay: std::time::Duration,
    ) -> Self {
        self.activity_delays
            .lock()
            .expect("lock")
            .insert((number, cursor.map(str::to_string)), delay);
        self
    }

    pub(crate) fn with_initial_activity_rate_limit(mut self, unit: RateLimitUnit) -> Self {
        self.initial_activity_rate_limit = unit;
        self
    }

    pub(crate) fn failing_point_budget(mut self) -> Self {
        self.point_budget_error = true;
        self
    }

    pub(crate) fn max_activity_in_flight(&self) -> usize {
        self.max_activity_in_flight.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Forge for FakeForge {
    async fn viewer(&self) -> Result<Viewer> {
        if self.point_budget_error {
            return Err(ForgeError::Unreachable {
                doing: "reading point budget".into(),
                source: "the fake was told to fail".into(),
            });
        }
        Ok(Viewer {
            login: "ashb".into(),
            rate_limit: rate_limit(4900),
        })
    }

    async fn rest_core_remaining(&self) -> Result<(u32, u32)> {
        Ok((5000, 5000))
    }

    async fn search_prs_page(
        &self,
        query: &str,
        _page_size: u32,
        after: Option<&str>,
    ) -> Result<SweepPage> {
        self.asked
            .lock()
            .expect("lock")
            .searches
            .push((query.to_string(), after.map(str::to_string)));
        let mut pages = self.pages.lock().expect("lock");
        let page = if pages.len() > 1 {
            pages.pop_front().expect("a page")
        } else {
            pages.front().cloned().unwrap_or_else(|| Page::of(vec![]))
        };
        Ok(SweepPage {
            prs: page.prs,
            next: page.next,
            total_count: page.total_count,
            cost: 1,
            remaining: page.remaining,
        })
    }

    async fn fetch_pr(&self, _owner: &str, _name: &str, number: u64) -> Result<Option<FetchedPr>> {
        if self.missing_prs.lock().expect("lock").contains(&number) {
            return Ok(None);
        }
        Ok(Some(FetchedPr {
            pr: pr(number, "2026-08-11T09:00:00Z"),
            labels: self.fetched_labels.lock().expect("lock").clone(),
        }))
    }

    async fn fetch_pr_detail(
        &self,
        _owner: &str,
        _name: &str,
        number: u64,
        _login: &str,
    ) -> Result<Option<PrDetail>> {
        self.asked.lock().expect("lock").details.push(number);
        if self.detail_errors.lock().expect("lock").contains(&number) {
            return Err(ForgeError::Unreachable {
                doing: format!("fetching detail for #{number}"),
                source: "the fake was told to fail".into(),
            });
        }
        Ok(self.details.lock().expect("lock").get(&number).cloned())
    }

    fn initial_activity_rate_limit(&self) -> RateLimitUnit {
        self.initial_activity_rate_limit
    }

    async fn fetch_pr_activity(
        &self,
        _owner: &str,
        _name: &str,
        number: u64,
        _actor: &str,
        cursor: Option<&str>,
    ) -> Result<ForgeActivityPage> {
        let in_flight = self.activity_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_activity_in_flight
            .fetch_max(in_flight, Ordering::SeqCst);
        let key = (number, cursor.map(str::to_string));
        let delay = self
            .activity_delays
            .lock()
            .expect("lock")
            .get(&key)
            .copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        } else {
            tokio::task::yield_now().await;
        }
        self.activity_in_flight.fetch_sub(1, Ordering::SeqCst);
        let cursor = key.1;
        self.asked
            .lock()
            .expect("lock")
            .activities
            .push((number, cursor.clone()));
        if self
            .activity_errors
            .lock()
            .expect("lock")
            .contains(&(number, cursor.clone()))
        {
            return Err(ForgeError::Unreachable {
                doing: format!("fetching activity for #{number}"),
                source: "the fake was told to fail".into(),
            });
        }
        Ok(self
            .activity_pages
            .lock()
            .expect("lock")
            .get(&(number, cursor))
            .cloned()
            .unwrap_or(ForgeActivityPage {
                activities: vec![],
                next: None,
                rate_limit: None,
                next_rate_limit: None,
            }))
    }

    async fn fetch_team_members(&self, org: &str, team: &str) -> Result<Vec<String>> {
        self.team_calls.fetch_add(1, Ordering::SeqCst);
        self.teams
            .lock()
            .unwrap()
            .get(&(org.into(), team.into()))
            .cloned()
            .ok_or_else(|| ForgeError::Unreachable {
                doing: format!("fetching {org}/{team}"),
                source: Box::new(std::io::Error::other("team unavailable")),
            })
    }

    async fn fetch_labels(
        &self,
        _owner: &str,
        _name: &str,
    ) -> Result<Vec<reviewq_forge::LabelColour>> {
        Ok(self.repo_labels.lock().expect("lock").clone())
    }

    async fn mark_pr_notifications_read(
        &self,
        _owner: &str,
        _name: &str,
        _number: u64,
    ) -> Result<()> {
        Ok(())
    }

    fn web_url(&self, owner: &str, name: &str, number: u64) -> String {
        format!("https://github.com/{owner}/{name}/pull/{number}")
    }

    fn handoff_credentials(&self) -> Result<(&str, &str)> {
        Ok(("GITHUB_TOKEN", "fake"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn activity_pages_keep_provider_cursors_and_rate_limits_opaque() {
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                17,
                None,
                ForgeActivityPage {
                    activities: vec![],
                    next: Some("provider cursor / one".into()),
                    rate_limit: Some(ActivityRateLimit {
                        unit: RateLimitUnit::Points,
                        cost: 7,
                        remaining: 4987,
                    }),
                    next_rate_limit: Some(RateLimitUnit::Requests),
                },
            )
            .with_activity_page(
                17,
                Some("provider cursor / one"),
                ForgeActivityPage {
                    activities: vec![ForgeActivity {
                        relation: reviewq_core::model::ActivityRelation::Own,
                        kind: reviewq_core::model::ActivityKind::Commented,
                        occurred_at: "2026-08-11T10:00:00Z".parse().unwrap(),
                        actor: Some("ashb".into()),
                        head_sha: None,
                        external_id: Some("comment node id / two".into()),
                        permalink: Some("https://forge.example/comments/two".into()),
                        payload: reviewq_core::model::ActivityPayload::None,
                    }],
                    next: None,
                    rate_limit: Some(ActivityRateLimit {
                        unit: RateLimitUnit::Requests,
                        cost: 3,
                        remaining: 4984,
                    }),
                    next_rate_limit: None,
                },
            );

        let first = forge
            .fetch_pr_activity("apache", "airflow", 17, "ashb", None)
            .await
            .unwrap();
        let second = forge
            .fetch_pr_activity("apache", "airflow", 17, "ashb", first.next.as_deref())
            .await
            .unwrap();

        assert_eq!(first.next.as_deref(), Some("provider cursor / one"));
        assert_eq!(
            first.rate_limit,
            Some(ActivityRateLimit {
                unit: RateLimitUnit::Points,
                cost: 7,
                remaining: 4987,
            })
        );
        assert_eq!(first.next_rate_limit, Some(RateLimitUnit::Requests));
        assert_eq!(second.next, None);
        assert_eq!(
            second.rate_limit,
            Some(ActivityRateLimit {
                unit: RateLimitUnit::Requests,
                cost: 3,
                remaining: 4984,
            })
        );
        assert_eq!(second.next_rate_limit, None);
        assert_eq!(
            forge.activities_asked(),
            vec![(17, None), (17, Some("provider cursor / one".into())),]
        );
    }

    #[test]
    fn activity_pages_expose_the_provider_neutral_fresh_cursor_rate_pool() {
        let forge =
            FakeForge::new(vec![]).with_initial_activity_rate_limit(RateLimitUnit::Requests);

        assert_eq!(forge.initial_activity_rate_limit(), RateLimitUnit::Requests);
    }

    #[test]
    fn activity_page_rejects_a_user_event_without_a_provider_id() {
        let page = ForgeActivityPage {
            activities: vec![ForgeActivity {
                relation: reviewq_core::model::ActivityRelation::Own,
                kind: reviewq_core::model::ActivityKind::Commented,
                occurred_at: ts("2026-08-11T10:00:00Z"),
                actor: Some("ashb".into()),
                head_sha: None,
                external_id: None,
                permalink: Some("https://forge.example/comments/missing-id".into()),
                payload: reviewq_core::model::ActivityPayload::None,
            }],
            next: None,
            rate_limit: None,
            next_rate_limit: None,
        };

        let err = page
            .validate()
            .expect_err("user activity needs a stable ID");

        assert!(err.to_string().contains("external ID"), "{err}");
    }
}
