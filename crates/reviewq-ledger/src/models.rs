//! Private database records loaded through Diesel.

use diesel::prelude::*;
use reviewq_core::model::PrSnapshot;

use crate::{
    LedgerError, RepoId, Result,
    db_types::{DbPrState, DbTimestamp},
    schema,
};

#[derive(Insertable)]
#[diesel(table_name = schema::labels)]
#[diesel(treat_none_as_default_value = false)]
pub(super) struct LabelRecord {
    pub(super) repo_id: RepoId,
    pub(super) name: String,
    pub(super) color: String,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = schema::prs)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
pub(super) struct Pr {
    pub(super) number: i64,
    pub(super) title: String,
    pub(super) author: String,
    pub(super) author_association: String,
    pub(super) head_sha: String,
    pub(super) is_draft: bool,
    pub(super) state: DbPrState,
    pub(super) updated_at: DbTimestamp,
    pub(super) labels: String,
    pub(super) milestone: Option<String>,
    pub(super) files: Option<String>,
    pub(super) files_truncated: bool,
    pub(super) base_ref: String,
    pub(super) created_at: Option<DbTimestamp>,
    pub(super) state_changed_at: Option<DbTimestamp>,
}

impl TryFrom<&PrSnapshot> for Pr {
    type Error = LedgerError;

    fn try_from(pr: &PrSnapshot) -> Result<Self> {
        use crate::Encoding as _;

        Ok(Self {
            number: pr.number as i64,
            title: pr.title.clone(),
            author: pr.author.clone(),
            author_association: pr.author_association.clone(),
            head_sha: pr.head_sha.clone(),
            is_draft: pr.is_draft,
            state: DbPrState::from(pr.state),
            updated_at: DbTimestamp::from(pr.updated_at),
            labels: serde_json::to_string(&pr.labels).encoding("a label list")?,
            milestone: pr.milestone.clone(),
            files: pr
                .files
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .encoding("a file list")?,
            files_truncated: pr.files_truncated,
            base_ref: pr.base_ref.clone(),
            created_at: pr.created_at.map(DbTimestamp::from),
            state_changed_at: pr.state_changed_at.map(DbTimestamp::from),
        })
    }
}

#[derive(Insertable)]
#[diesel(table_name = schema::prs)]
pub(super) struct NewPr {
    #[diesel(embed)]
    pub(super) pr: Pr,
    pub(super) repo_id: RepoId,
    pub(super) tracked_reason: Option<String>,
    pub(super) first_seen_at: DbTimestamp,
    pub(super) detail_synced_at: Option<DbTimestamp>,
    pub(super) body: Option<String>,
    pub(super) after_merge: bool,
    pub(super) untracked_at: Option<DbTimestamp>,
}

#[derive(AsChangeset)]
#[diesel(table_name = schema::prs)]
pub(super) struct PrSummary<'a> {
    pub(super) title: &'a str,
    pub(super) author: &'a str,
    pub(super) author_association: &'a str,
    pub(super) head_sha: &'a str,
    pub(super) is_draft: bool,
    pub(super) updated_at: &'a DbTimestamp,
    pub(super) labels: &'a str,
    #[diesel(treat_none_as_null = true)]
    pub(super) milestone: Option<&'a str>,
    #[diesel(treat_none_as_null = true)]
    pub(super) files: Option<&'a str>,
    pub(super) files_truncated: bool,
    pub(super) base_ref: &'a str,
    pub(super) created_at: Option<&'a DbTimestamp>,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = schema::my_state)]
#[diesel(treat_none_as_default_value = false)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
pub(super) struct MyStateRecord {
    pub(super) repo_id: RepoId,
    pub(super) number: i64,
    pub(super) last_reviewed_sha: Option<String>,
    pub(super) last_verdict: Option<String>,
    pub(super) last_action_at: Option<DbTimestamp>,
    pub(super) done_sha: Option<String>,
    pub(super) snoozed_until: Option<DbTimestamp>,
    pub(super) muted: bool,
    pub(super) deferred_at: Option<DbTimestamp>,
    pub(super) done_at: Option<DbTimestamp>,
}

#[derive(AsChangeset, Insertable)]
#[diesel(table_name = schema::my_state)]
#[diesel(primary_key(repo_id, number))]
#[diesel(treat_none_as_default_value = false)]
#[diesel(treat_none_as_null = true)]
pub(super) struct ForgeState<'a> {
    pub(super) repo_id: RepoId,
    pub(super) number: i64,
    pub(super) last_reviewed_sha: Option<&'a str>,
    pub(super) last_verdict: Option<&'a str>,
    pub(super) last_action_at: Option<&'a DbTimestamp>,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = schema::reviewers)]
#[diesel(treat_none_as_default_value = false)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
pub(super) struct ReviewerRecord {
    pub(super) repo_id: RepoId,
    pub(super) pr_number: i64,
    pub(super) login: String,
    pub(super) verdict: String,
    pub(super) submitted_at: DbTimestamp,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = schema::threads)]
#[diesel(treat_none_as_default_value = false)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
pub(super) struct ThreadRecord {
    pub(super) thread_id: String,
    pub(super) repo_id: RepoId,
    pub(super) pr_number: i64,
    pub(super) i_own: bool,
    pub(super) is_resolved: bool,
    pub(super) resolved_by: Option<String>,
    pub(super) last_comment_author: Option<String>,
    pub(super) last_comment_at: Option<DbTimestamp>,
    pub(super) my_last_comment_at: Option<DbTimestamp>,
    pub(super) resolution_event_id: Option<crate::ActivityEventId>,
}

#[derive(Insertable, Queryable, Selectable)]
#[diesel(table_name = schema::attention)]
#[diesel(treat_none_as_default_value = false)]
#[diesel(check_for_backend(diesel::sqlite::Sqlite))]
pub(super) struct AttentionRecord {
    pub(super) repo_id: RepoId,
    pub(super) pr_number: i64,
    pub(super) reason: String,
    pub(super) since: DbTimestamp,
    pub(super) payload: String,
    pub(super) priority: bool,
}
