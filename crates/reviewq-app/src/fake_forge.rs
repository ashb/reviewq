//! A [`Forge`] that serves scripted responses, for the tests in this crate.
//!
//! One fake, shared: `sync`'s engine tests drive whole passes through it, and
//! `review`'s tests use it so that working out a handoff never resolves a real
//! token — resolution runs whatever the host configures, up to a credential
//! helper that can block on an interactive unlock, which `cargo test` must not be
//! able to trigger.

use std::sync::Mutex;

use anyhow::{Result, bail};
use jiff::Timestamp;
use reviewq_core::model::{PrSnapshot, PrState};
use reviewq_forge::{Forge, PrDetail, RateLimit, SweepPage, Viewer};

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
}

/// A forge that serves scripted pages and details, and records its calls.
pub(crate) struct FakeForge {
    /// Search pages, served in order; the last is reused if asked again.
    pages: Mutex<std::collections::VecDeque<Page>>,
    /// Per-PR detail. A number absent from here is a PR the forge no longer
    /// has, which is the deleted-PR path.
    details: Mutex<std::collections::HashMap<u64, PrDetail>>,
    /// Numbers whose detail fetch should fail outright.
    detail_errors: Mutex<std::collections::HashSet<u64>>,
    asked: Mutex<Asked>,
}

impl FakeForge {
    pub(crate) fn new(pages: Vec<Page>) -> Self {
        Self {
            pages: Mutex::new(pages.into()),
            details: Mutex::new(std::collections::HashMap::new()),
            detail_errors: Mutex::new(std::collections::HashSet::new()),
            asked: Mutex::new(Asked::default()),
        }
    }

    /// Give `number` a detail response that holds nothing of interest.
    pub(crate) fn with_detail(self, number: u64, remaining: u32) -> Self {
        self.details.lock().expect("lock").insert(
            number,
            PrDetail {
                number,
                head_sha: format!("sha{number}"),
                body: String::new(),
                last_reviewed_sha: None,
                last_verdict: None,
                last_action_at: None,
                threads: vec![],
                reviewers: vec![],
                mentions: vec![],
                new_commits: 0,
                review_request: None,
                cost: 1,
                remaining,
            },
        );
        self
    }

    /// Give `number` a detail response that puts it on the queue: someone
    /// asked me to review it.
    pub(crate) fn with_review_request(self, number: u64, remaining: u32) -> Self {
        let this = self.with_detail(number, remaining);
        if let Some(detail) = this.details.lock().expect("lock").get_mut(&number) {
            detail.review_request = Some(reviewq_core::model::ReviewRequest { team: None });
        }
        this
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
}

#[async_trait::async_trait]
impl Forge for FakeForge {
    async fn viewer(&self) -> Result<Viewer> {
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

    async fn fetch_pr(&self, _owner: &str, _name: &str, number: u64) -> Result<Option<PrSnapshot>> {
        Ok(Some(pr(number, "2026-08-11T09:00:00Z")))
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
            bail!("the forge fell over on #{number}");
        }
        Ok(self.details.lock().expect("lock").get(&number).cloned())
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
