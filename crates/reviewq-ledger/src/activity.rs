//! Durable pull-request activity history and lifecycle projection.

use diesel::{
    deserialize::{self, FromSql, FromSqlRow},
    dsl::{count_star, exists},
    expression::AsExpression,
    prelude::*,
    serialize::{self, IsNull, Output, ToSql},
    sql_types::BigInt,
    sqlite::Sqlite,
};
use jiff::Timestamp;
use reviewq_core::model::{
    ActivityKind, ActivityPayload, ActivityRelation, ActivitySource, MyState, PrSnapshot, PrState,
};

use crate::{
    DbTimestamp, Doing as _, Encoding as _, Ledger, LedgerError, RepoId, RepoKey, Result,
    TrackedReason,
    connection::DbConnection,
    existing_row,
    schema::{
        activity_events as e, activity_retention as retention, activity_sync_state as b, attention,
        my_state, prs, repos, sync_meta,
    },
    tracked_reason, upsert_row,
};

/// An activity event row in this ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, AsExpression, FromSqlRow)]
#[diesel(sql_type = BigInt)]
pub struct ActivityEventId(i64);

impl ToSql<BigInt, Sqlite> for ActivityEventId {
    fn to_sql<'b>(&'b self, output: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        output.set_value(self.0);
        Ok(IsNull::No)
    }
}

impl FromSql<BigInt, Sqlite> for ActivityEventId {
    fn from_sql(
        value: <Sqlite as diesel::backend::Backend>::RawValue<'_>,
    ) -> deserialize::Result<Self> {
        i64::from_sql(value).map(Self)
    }
}

/// An activity event ready to be recorded for a known pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewActivityEvent {
    /// The event's relationship to the configured user.
    pub relation: ActivityRelation,
    /// Whether reviewq performed or observed the action.
    pub source: ActivitySource,
    /// The action or lifecycle event that occurred.
    pub kind: ActivityKind,
    /// When the activity occurred.
    pub occurred_at: Timestamp,
    /// When reviewq recorded the activity.
    pub recorded_at: Timestamp,
    /// The actor's login, when the forge supplied one.
    pub actor: Option<String>,
    /// The pull request head associated with the event, when known.
    pub head_sha: Option<String>,
    /// The provider's opaque event identifier, when supplied.
    pub external_id: Option<String>,
    /// The provider's opaque permalink, when supplied.
    pub permalink: Option<String>,
    /// Event-specific details.
    pub payload: ActivityPayload,
}

/// An activity event read from the ledger with its current pull request context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEvent {
    /// The event's relationship to the configured user.
    pub relation: ActivityRelation,
    /// The ledger-assigned event identifier.
    pub id: ActivityEventId,
    /// The repository containing the pull request.
    pub repo: RepoKey,
    /// The repository identifier already resolved by the ledger.
    pub repo_id: RepoId,
    /// The pull request number.
    pub pr_number: u64,
    /// The current pull request title.
    pub pr_title: String,
    /// Whether reviewq performed or observed the action.
    pub source: ActivitySource,
    /// The action or lifecycle event that occurred.
    pub kind: ActivityKind,
    /// When the activity occurred.
    pub occurred_at: Timestamp,
    /// When reviewq recorded the activity.
    pub recorded_at: Timestamp,
    /// The actor's login, when the forge supplied one.
    pub actor: Option<String>,
    /// The pull request head associated with the event, when known.
    pub head_sha: Option<String>,
    /// The provider's opaque event identifier, when supplied.
    pub external_id: Option<String>,
    /// The provider's opaque permalink, when supplied.
    pub permalink: Option<String>,
    /// Event-specific details.
    pub payload: ActivityPayload,
}

/// The part of history a caller wants to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityScope {
    /// All retained events for a single PR, including other people's activity.
    PrAll {
        /// The ledger repository identifier.
        repo_id: RepoId,
        /// The pull request number.
        number: u64,
    },
    /// Events for every repository and pull request.
    All,
    /// Events for one pull request.
    Pr {
        /// The ledger repository identifier.
        repo_id: RepoId,
        /// The pull request number.
        number: u64,
    },
}

/// A stable boundary between adjacent activity pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityCursor {
    /// The last event's activity timestamp.
    pub occurred_at: Timestamp,
    /// The last event's ledger identifier, breaking timestamp ties.
    pub id: ActivityEventId,
}

/// A bounded history read and its continuation cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityPage {
    /// Events in descending `(occurred_at, id)` order.
    pub events: Vec<ActivityEvent>,
    /// The cursor for the next, older page.
    pub next: Option<ActivityCursor>,
}

/// Provider-owned progress for one pull request's initial activity backfill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityBackfill {
    /// The opaque cursor to resume from.
    pub cursor: Option<String>,
    /// The provider budget pool the resumed request will consume, when any.
    pub next_rate_limit: Option<ActivityRateLimitUnit>,
    /// When the provider reported that no page remained.
    pub completed_at: Option<Timestamp>,
}

/// Provider-owned progress for an interrupted incremental activity traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityIncremental {
    /// Revision used to reject a response fetched from a superseded checkpoint.
    pub revision: i64,
    /// Refresh generation covered by this traversal; `None` when idle.
    pub generation: Option<i64>,
    /// The opaque cursor to resume from, or `None` for a fresh traversal.
    pub cursor: Option<String>,
    /// The provider budget pool the resumed request will consume, when any.
    pub next_rate_limit: Option<ActivityRateLimitUnit>,
    /// The captured start time this traversal will establish coverage through.
    pub started_at: Option<Timestamp>,
    /// Previous coverage minus an overlap; finish its entire timestamp group.
    pub stop_at: Option<Timestamp>,
}

/// Whether a page advanced the checkpoint it was fetched from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityPageCommit {
    /// The expected cursor was current and the page committed.
    Applied {
        /// New events stored after deduplication.
        inserted: u64,
    },
    /// Another writer had already advanced or completed this traversal.
    Superseded,
}

/// A provider budget pool saved beside an opaque activity cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityRateLimitUnit {
    /// A count of provider requests.
    Requests,
    /// A provider-computed query point balance.
    Points,
}

/// Counts selected by an explicit activity-retention cutoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupPreview {
    /// The number of events selected.
    pub event_count: u64,
    /// The number of pull requests with at least one selected event.
    pub pr_count: u64,
}

impl Ledger {
    /// Whether a lifecycle event concerns the user's attention or participation.
    /// A tracking rule alone is not personal involvement.
    pub fn lifecycle_affects_me(
        &self,
        repo_id: RepoId,
        number: u64,
        at: Timestamp,
        kind: ActivityKind,
        viewer: &str,
    ) -> Result<bool> {
        let conn = &mut *self.conn.borrow_mut();
        let author = prs::table
            .find((repo_id, number as i64))
            .select(prs::author)
            .first::<String>(conn)
            .doing("reading lifecycle author")?;
        if author.eq_ignore_ascii_case(viewer) {
            return Ok(true);
        }
        let at_wire = activity_timestamp_to_wire(at);
        let retained = diesel::select(exists(
            e::table
                .filter(e::repo_id.eq(repo_id))
                .filter(e::pr_number.eq(number as i64))
                .filter(e::kind.eq(activity_kind_to_wire(kind)))
                .filter(e::relation.ne("context"))
                .filter(e::occurred_at.eq(&at_wire)),
        ))
        .get_result::<bool>(conn)
        .doing("checking retained lifecycle relevance")?;
        Ok(retained || lifecycle_affects_me(conn, repo_id, number, at)?)
    }

    /// Store one successful local action for a pull request already in the ledger.
    pub fn record_activity(
        &self,
        repo_id: RepoId,
        number: u64,
        event: &NewActivityEvent,
    ) -> Result<()> {
        self.conn.borrow_mut().immediate_transaction(|conn| {
            insert_activity(conn, repo_id, number, event, false, false)?;
            Ok(())
        })
    }

    /// Store one forge-observed event, returning whether it was new.
    pub fn record_forge_activity(
        &self,
        repo_id: RepoId,
        number: u64,
        event: &NewActivityEvent,
    ) -> Result<bool> {
        self.conn.borrow_mut().transaction(|conn| {
            let retained = crate::detail_activity::retain_history_review(
                conn,
                repo_id,
                number,
                std::slice::from_ref(event),
            )?;
            let inserted =
                insert_activity(conn, repo_id, number, event, true, false)? || retained > 0;
            reconcile_lifecycle_projection(conn, repo_id, number)?;
            Ok(inserted)
        })
    }

    /// Read one pull request's initial activity-backfill checkpoint.
    pub fn activity_backfill(
        &self,
        repo_id: RepoId,
        number: u64,
    ) -> Result<Option<ActivityBackfill>> {
        let conn = &mut *self.conn.borrow_mut();
        let stored = b::table
            .find((repo_id, number as i64))
            .select((b::cursor, b::next_rate_limit, b::completed_at))
            .first::<(Option<String>, Option<String>, Option<String>)>(conn)
            .optional()
            .doing(format!("reading activity backfill for #{number}"))?;
        stored
            .map(|(cursor, next_rate_limit, completed_at)| {
                let next_rate_limit = match next_rate_limit {
                    Some(value) => Some(value),
                    None => sync_meta::table
                        .find((repo_id, activity_rate_limit_key(number)))
                        .select(sync_meta::value)
                        .first::<String>(conn)
                        .optional()
                        .doing(format!(
                            "reading legacy activity rate-limit progress for #{number}"
                        ))?,
                }
                .map(|value| activity_rate_limit_from_wire(&value))
                .transpose()?;
                Ok(ActivityBackfill {
                    cursor,
                    next_rate_limit,
                    completed_at: completed_at
                        .map(|at| decode_activity_timestamp(at, "activity backfill completed_at"))
                        .transpose()?,
                })
            })
            .transpose()
    }

    /// Read one completed pull request's interrupted incremental traversal.
    pub fn activity_incremental(
        &self,
        repo_id: RepoId,
        number: u64,
    ) -> Result<Option<ActivityIncremental>> {
        read_incremental_activity(&mut self.conn.borrow_mut(), repo_id, number)
    }

    /// Start or resume a traversal, capturing the refresh generation before fetching.
    pub fn begin_incremental_activity(
        &self,
        repo_id: RepoId,
        number: u64,
        now: Timestamp,
    ) -> Result<ActivityIncremental> {
        self.conn.borrow_mut().transaction(|conn| {
            let covered = b::table
                .find((repo_id, number as i64))
                .select(b::covered_through)
                .first::<Option<String>>(conn)
                .optional()
                .doing(format!("reading #{number}'s activity coverage"))?
                .flatten();
            let stop_at = covered
                .map(|at| {
                    decode_activity_timestamp(at, "activity coverage").map(|at| {
                        activity_timestamp_to_wire(at - jiff::SignedDuration::from_mins(5))
                    })
                })
                .transpose()?;
            diesel::update(
                b::table
                    .find((repo_id, number as i64))
                    .filter(b::completed_at.is_not_null())
                    .filter(b::incremental_generation.is_null()),
            )
            .set((
                b::incremental_generation.eq(b::requested_generation.nullable()),
                b::incremental_revision.eq(b::incremental_revision + 1),
                b::incremental_started_at.eq(activity_timestamp_to_wire(now)),
                b::incremental_stop_at.eq(stop_at),
            ))
            .execute(conn)
            .doing(format!("starting incremental activity for #{number}"))?;
            read_incremental_activity(conn, repo_id, number)?
                .ok_or(LedgerError::NotStored { number })
        })
    }

    /// Store one provider page and its resume cursor in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_activity_page(
        &self,
        repo_id: RepoId,
        number: u64,
        expected: Option<&str>,
        events: &[NewActivityEvent],
        next: Option<&str>,
        next_rate_limit: Option<ActivityRateLimitUnit>,
        now: Timestamp,
    ) -> Result<ActivityPageCommit> {
        self.conn.borrow_mut().transaction(|conn| {
            let completed_at = next.is_none().then(|| activity_timestamp_to_wire(now));
            let next_rate_limit = next_rate_limit.map(activity_rate_limit_to_wire);
            let mut checkpointed = diesel::update(
                b::table
                    .find((repo_id, number as i64))
                    .filter(b::cursor.is(expected))
                    .filter(b::completed_at.is_null()),
            )
            .set((
                b::cursor.eq(next),
                b::next_rate_limit.eq(next_rate_limit),
                b::completed_at.eq(&completed_at),
                expected
                    .is_none()
                    .then(|| b::backfill_started_at.eq(activity_timestamp_to_wire(now))),
            ))
            .execute(conn)
            .doing(format!("checkpointing activity backfill for #{number}"))?;
            if checkpointed == 0 && expected.is_none() {
                checkpointed = diesel::insert_into(b::table)
                    .values((
                        b::repo_id.eq(repo_id),
                        b::pr_number.eq(number as i64),
                        b::cursor.eq(next),
                        b::next_rate_limit.eq(next_rate_limit),
                        b::completed_at.eq(&completed_at),
                        b::backfill_started_at.eq(activity_timestamp_to_wire(now)),
                    ))
                    .on_conflict((b::repo_id, b::pr_number))
                    .do_nothing()
                    .execute(conn)
                    .doing(format!("starting activity backfill for #{number}"))?;
            }
            if checkpointed == 0 {
                return Ok(ActivityPageCommit::Superseded);
            }
            if next.is_none() {
                diesel::update(b::table.find((repo_id, number as i64)))
                    .set(b::covered_through.eq(b::backfill_started_at))
                    .execute(conn)
                    .doing("completing initial history coverage")?;
            }
            let mut inserted =
                crate::detail_activity::retain_history_review(conn, repo_id, number, events)?;
            for event in events {
                inserted += u64::from(insert_activity(conn, repo_id, number, event, true, false)?);
            }
            reconcile_lifecycle_projection(conn, repo_id, number)?;
            diesel::delete(sync_meta::table.find((repo_id, activity_rate_limit_key(number))))
                .execute(conn)
                .doing(format!("clearing legacy activity progress for #{number}"))?;
            Ok(ActivityPageCommit::Applied { inserted })
        })
    }

    /// Store one incremental provider page and its resume cursor atomically.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_incremental_activity_page(
        &self,
        repo_id: RepoId,
        number: u64,
        expected: &ActivityIncremental,
        events: &[NewActivityEvent],
        next: Option<&str>,
        next_rate_limit: Option<ActivityRateLimitUnit>,
    ) -> Result<ActivityPageCommit> {
        self.conn.borrow_mut().transaction(|conn| {
            let Some(generation) = expected.generation else {
                return Ok(ActivityPageCommit::Superseded);
            };
            let checkpointed = diesel::update(
                b::table
                    .find((repo_id, number as i64))
                    .filter(b::incremental_revision.eq(expected.revision))
                    .filter(b::incremental_generation.eq(generation))
                    .filter(b::completed_at.is_not_null()),
            )
            .set((
                b::incremental_cursor.eq(next),
                b::incremental_next_rate_limit.eq(next_rate_limit.map(activity_rate_limit_to_wire)),
                b::incremental_stop_at
                    .eq(next.and(expected.stop_at).map(activity_timestamp_to_wire)),
                b::incremental_started_at.eq(next
                    .and(expected.started_at)
                    .map(activity_timestamp_to_wire)),
                b::incremental_revision.eq(b::incremental_revision + 1),
                b::incremental_generation.eq(next.map(|_| generation)),
            ))
            .execute(conn)
            .doing(format!("checkpointing incremental activity for #{number}"))?;
            if checkpointed == 0 {
                return Ok(ActivityPageCommit::Superseded);
            }
            if next.is_none() {
                diesel::update(b::table.find((repo_id, number as i64)))
                    .set(b::covered_through.eq(expected.started_at.map(activity_timestamp_to_wire)))
                    .execute(conn)
                    .doing("advancing incremental history coverage")?;
                diesel::update(
                    b::table
                        .find((repo_id, number as i64))
                        .filter(b::completed_generation.lt(generation)),
                )
                .set(b::completed_generation.eq(generation))
                .execute(conn)
                .doing(format!("acknowledging activity refresh for #{number}"))?;
            }
            let mut inserted =
                crate::detail_activity::retain_history_review(conn, repo_id, number, events)?;
            for event in events {
                inserted += u64::from(insert_activity(conn, repo_id, number, event, true, false)?);
            }
            reconcile_lifecycle_projection(conn, repo_id, number)?;
            Ok(ActivityPageCommit::Applied { inserted })
        })
    }

    /// Whether a provider event identity is already retained for this repository.
    pub fn has_forge_activity(
        &self,
        repo_id: RepoId,
        kind: ActivityKind,
        external_id: &str,
    ) -> Result<bool> {
        diesel::select(exists(
            e::table
                .filter(e::repo_id.eq(repo_id))
                .filter(e::kind.eq(activity_kind_to_wire(kind)))
                .filter(e::external_id.eq(external_id)),
        ))
        .get_result(&mut *self.conn.borrow_mut())
        .doing("checking provider activity identity")
    }

    /// Project a newly observed lifecycle state and record its transition atomically.
    ///
    /// Returns whether a new transition event was inserted. An authoritative
    /// timestamp can correct an earlier observation without creating another event.
    pub fn record_state_transition(
        &self,
        repo_id: RepoId,
        number: u64,
        state: PrState,
        state_changed_at: Option<Timestamp>,
        observed_at: Timestamp,
    ) -> Result<bool> {
        self.conn.borrow_mut().transaction(|conn| {
            record_state_transition_row(conn, repo_id, number, state, state_changed_at, observed_at)
        })
    }

    /// Record `reviewq done` and its local history event atomically.
    pub fn record_done_action(
        &self,
        repo_id: RepoId,
        number: u64,
        done_sha: &str,
        done_at: Timestamp,
        event: &NewActivityEvent,
    ) -> Result<()> {
        self.conn.borrow_mut().transaction(|conn| {
            let prior_attention = crate::attention_activity::load(conn, repo_id, number)?;
            let done_at = done_at.to_string();
            diesel::insert_into(my_state::table)
                .values((
                    my_state::repo_id.eq(repo_id),
                    my_state::number.eq(number as i64),
                    my_state::done_sha.eq(done_sha),
                    my_state::done_at.eq(&done_at),
                ))
                .on_conflict((my_state::repo_id, my_state::number))
                .do_update()
                .set((
                    my_state::done_sha.eq(done_sha),
                    my_state::done_at.eq(&done_at),
                ))
                .execute(conn)
                .doing(format!("recording done for #{number}"))?;
            diesel::delete(
                attention::table
                    .filter(attention::repo_id.eq(repo_id))
                    .filter(attention::pr_number.eq(number as i64))
                    .filter(attention::reason.ne("review_requested")),
            )
            .execute(conn)
            .doing(format!("clearing done attention for #{number}"))?;
            crate::attention_activity::record(
                conn,
                repo_id,
                number,
                prior_attention,
                event.occurred_at,
            )?;
            insert_activity(conn, repo_id, number, event, false, false)?;
            Ok(())
        })
    }

    /// Record `reviewq snooze` and its local history event atomically.
    pub fn record_snooze_action(
        &self,
        repo_id: RepoId,
        number: u64,
        until: Timestamp,
        event: &NewActivityEvent,
    ) -> Result<()> {
        self.conn.borrow_mut().transaction(|conn| {
            let prior_attention = crate::attention_activity::load(conn, repo_id, number)?;
            let until = until.to_string();
            diesel::insert_into(my_state::table)
                .values((
                    my_state::repo_id.eq(repo_id),
                    my_state::number.eq(number as i64),
                    my_state::snoozed_until.eq(&until),
                ))
                .on_conflict((my_state::repo_id, my_state::number))
                .do_update()
                .set(my_state::snoozed_until.eq(&until))
                .execute(conn)
                .doing(format!("snoozing #{number}"))?;
            diesel::delete(
                attention::table
                    .filter(attention::repo_id.eq(repo_id))
                    .filter(attention::pr_number.eq(number as i64)),
            )
            .execute(conn)
            .doing(format!("clearing attention for #{number}"))?;
            crate::attention_activity::record(
                conn,
                repo_id,
                number,
                prior_attention,
                event.occurred_at,
            )?;
            insert_activity(conn, repo_id, number, event, false, false)?;
            Ok(())
        })
    }

    /// Set a PR's mute and record the action if the mute state changed.
    pub fn record_muted_action(
        &self,
        repo_id: RepoId,
        number: u64,
        muted: bool,
        event: &NewActivityEvent,
    ) -> Result<bool> {
        self.conn.borrow_mut().transaction(|conn| {
            if existing_row(conn, repo_id, number)?.is_none() {
                return Err(LedgerError::NotStored { number });
            }
            let current = my_state::table
                .find((repo_id, number as i64))
                .select(my_state::muted)
                .first::<bool>(conn)
                .optional()
                .doing(format!("reading muted state for #{number}"))?
                .unwrap_or(false);
            if current == muted {
                return Ok(false);
            }
            diesel::insert_into(my_state::table)
                .values((
                    my_state::repo_id.eq(repo_id),
                    my_state::number.eq(number as i64),
                    my_state::muted.eq(muted),
                ))
                .on_conflict((my_state::repo_id, my_state::number))
                .do_update()
                .set(my_state::muted.eq(muted))
                .execute(conn)
                .doing(format!("setting muted for #{number}"))?;
            insert_activity(conn, repo_id, number, event, false, false)?;
            Ok(true)
        })
    }

    /// Set a PR's defer state and record the action if it changed.
    pub fn record_deferred_action(
        &self,
        repo_id: RepoId,
        number: u64,
        deferred_at: Option<Timestamp>,
        event: &NewActivityEvent,
    ) -> Result<bool> {
        self.conn.borrow_mut().transaction(|conn| {
            if existing_row(conn, repo_id, number)?.is_none() {
                return Err(LedgerError::NotStored { number });
            }
            let current = my_state::table
                .find((repo_id, number as i64))
                .select(my_state::deferred_at)
                .first::<Option<String>>(conn)
                .optional()
                .doing(format!("reading deferred state for #{number}"))?
                .flatten()
                .map(|at| decode_activity_timestamp(at, "deferred_at"))
                .transpose()?;
            let newest_attention = attention::table
                .filter(attention::repo_id.eq(repo_id))
                .filter(attention::pr_number.eq(number as i64))
                .select(attention::since)
                .load::<DbTimestamp>(conn)
                .doing(format!("reading attention for defer on #{number}"))?
                .into_iter()
                .map(DbTimestamp::into_timestamp)
                .max();
            let mine = MyState {
                deferred_at: current,
                ..MyState::default()
            };
            let unchanged = match deferred_at {
                Some(_) => mine.is_deferred(newest_attention),
                None => current.is_none(),
            };
            if unchanged {
                return Ok(false);
            }
            let deferred_at = deferred_at.map(|at| at.to_string());
            diesel::insert_into(my_state::table)
                .values((
                    my_state::repo_id.eq(repo_id),
                    my_state::number.eq(number as i64),
                    my_state::deferred_at.eq(&deferred_at),
                ))
                .on_conflict((my_state::repo_id, my_state::number))
                .do_update()
                .set(my_state::deferred_at.eq(&deferred_at))
                .execute(conn)
                .doing(format!("setting deferred_at for #{number}"))?;
            insert_activity(conn, repo_id, number, event, false, false)?;
            Ok(true)
        })
    }

    /// Force-track a stored PR and record the action if it changed.
    pub fn record_track_action(
        &self,
        repo_id: RepoId,
        number: u64,
        event: &NewActivityEvent,
    ) -> Result<bool> {
        self.conn.borrow_mut().transaction(|conn| {
            if tracked_reason(conn, repo_id, number)?.is_some() {
                return Ok(false);
            }
            diesel::update(prs::table.find((repo_id, number as i64)))
                .set((
                    prs::tracked_reason.eq(TrackedReason::Involved("manual".into()).render()),
                    prs::untracked_at.eq(None::<String>),
                ))
                .execute(conn)
                .doing(format!("force-tracking #{number}"))?;
            insert_activity(conn, repo_id, number, event, false, false)?;
            Ok(true)
        })
    }

    /// Store a fetched PR, force-track it, and record the action atomically.
    pub fn record_fetched_track_action(
        &self,
        repo_id: RepoId,
        pr: &PrSnapshot,
        now: Timestamp,
        event: &NewActivityEvent,
    ) -> Result<bool> {
        self.conn.borrow_mut().transaction(|conn| {
            upsert_row(conn, repo_id, pr, None, now)?;
            if tracked_reason(conn, repo_id, pr.number)?.is_some() {
                return Ok(false);
            }
            diesel::update(prs::table.find((repo_id, pr.number as i64)))
                .set((
                    prs::tracked_reason.eq(TrackedReason::Involved("manual".into()).render()),
                    prs::untracked_at.eq(None::<String>),
                ))
                .execute(conn)
                .doing(format!("force-tracking #{}", pr.number))?;
            insert_activity(conn, repo_id, pr.number, event, false, false)?;
            Ok(true)
        })
    }

    /// Stop watching a PR and record the action if its tracking state changed.
    pub fn record_untrack_action(
        &self,
        repo_id: RepoId,
        number: u64,
        now: Timestamp,
        event: &NewActivityEvent,
    ) -> Result<bool> {
        self.conn.borrow_mut().transaction(|conn| {
            let prior_attention = crate::attention_activity::load(conn, repo_id, number)?;
            let untracked_at = prs::table
                .find((repo_id, number as i64))
                .select(prs::untracked_at)
                .first::<Option<String>>(conn)
                .optional()
                .doing(format!("reading #{number}'s untrack marker"))?;
            let Some(untracked_at) = untracked_at else {
                return Ok(false);
            };
            if untracked_at.is_some() {
                return Ok(true);
            }
            diesel::update(prs::table.find((repo_id, number as i64)))
                .set((
                    prs::tracked_reason.eq(None::<String>),
                    prs::untracked_at.eq(now.to_string()),
                ))
                .execute(conn)
                .doing(format!("untracking #{number}"))?;
            diesel::delete(
                attention::table
                    .filter(attention::repo_id.eq(repo_id))
                    .filter(attention::pr_number.eq(number as i64)),
            )
            .execute(conn)
            .doing(format!("clearing attention for untracked #{number}"))?;
            crate::attention_activity::record(
                conn,
                repo_id,
                number,
                prior_attention,
                event.occurred_at,
            )?;
            insert_activity(conn, repo_id, number, event, false, false)?;
            Ok(true)
        })
    }

    /// Read one history page in descending `(occurred_at, id)` order.
    pub fn activity_page(
        &self,
        scope: ActivityScope,
        cursor: Option<&ActivityCursor>,
        limit: usize,
    ) -> Result<ActivityPage> {
        if limit == 0 {
            return Ok(ActivityPage {
                events: Vec::new(),
                next: None,
            });
        }
        let limit_plus_one = i64::try_from(limit)
            .ok()
            .and_then(|limit| limit.checked_add(1))
            .unwrap_or(i64::MAX);
        let stored = activity_page_query(scope, cursor, limit_plus_one)
            .load::<StoredActivityEvent>(&mut *self.conn.borrow_mut())
            .doing("reading activity page")?;
        let mut events = stored
            .into_iter()
            .map(activity_from_row)
            .collect::<Result<Vec<_>>>()?;
        let next = if events.len() > limit {
            events.truncate(limit);
            events.last().map(|event| ActivityCursor {
                occurred_at: event.occurred_at,
                id: event.id,
            })
        } else {
            None
        };
        Ok(ActivityPage { events, next })
    }

    /// Read the newest events for one pull request.
    pub fn activity_preview(
        &self,
        repo_id: RepoId,
        number: u64,
        limit: usize,
    ) -> Result<Vec<ActivityEvent>> {
        Ok(self
            .activity_page(ActivityScope::Pr { repo_id, number }, None, limit)?
            .events)
    }

    /// Count events selected by an explicit retention cutoff without writing.
    pub fn preview_activity_cleanup(&self, cutoff: Timestamp) -> Result<CleanupPreview> {
        self.conn
            .borrow_mut()
            .transaction(|conn| preview_activity_cleanup(conn, cutoff))
    }

    /// Delete events older than `cutoff`, retaining current attention evidence.
    pub fn clean_activity(&self, cutoff: Timestamp) -> Result<CleanupPreview> {
        self.conn.borrow_mut().immediate_transaction(|conn| {
            let preview = preview_activity_cleanup(conn, cutoff)?;
            let cutoff = activity_timestamp_to_wire(cutoff);
            diesel::delete(
                e::table
                    .filter(e::occurred_at.lt(&cutoff))
                    .filter(
                        e::id.ne_all(
                            crate::schema::threads::table
                                .filter(crate::schema::threads::resolution_event_id.is_not_null())
                                .select(
                                    crate::schema::threads::resolution_event_id.assume_not_null(),
                                ),
                        ),
                    )
                    .filter(
                        e::id.ne_all(
                            crate::schema::my_state::table
                                .filter(crate::schema::my_state::last_review_event_id.is_not_null())
                                .select(
                                    crate::schema::my_state::last_review_event_id.assume_not_null(),
                                ),
                        ),
                    ),
            )
            .execute(conn)
            .doing("cleaning activity")?;
            diesel::update(
                retention::table
                    .filter(retention::singleton.eq(1_i64))
                    .filter(retention::cutoff.lt(&cutoff)),
            )
            .set(retention::cutoff.eq(&cutoff))
            .execute(conn)
            .doing("advancing activity retention")?;
            diesel::insert_into(retention::table)
                .values((
                    retention::singleton.eq(1_i64),
                    retention::cutoff.eq(&cutoff),
                ))
                .on_conflict(retention::singleton)
                .do_nothing()
                .execute(conn)
                .doing("initializing activity retention")?;
            Ok(preview)
        })
    }

    /// PR numbers whose forge activity should stay current.
    pub fn activity_candidates(&self, repo_id: RepoId) -> Result<Vec<u64>> {
        prs::table
            .filter(prs::repo_id.eq(repo_id))
            .filter(
                prs::tracked_reason.is_not_null().or(exists(
                    attention::table
                        .filter(attention::repo_id.eq(prs::repo_id))
                        .filter(attention::pr_number.eq(prs::number)),
                )),
            )
            .select(prs::number)
            .order(prs::number)
            .load::<i64>(&mut *self.conn.borrow_mut())
            .map(|numbers| numbers.into_iter().map(|number| number as u64).collect())
            .doing("reading activity candidates")
    }

    /// Completed histories needing a current refresh or an interrupted traversal resumed.
    pub fn activity_refresh_candidates(&self, repo_id: RepoId) -> Result<Vec<u64>> {
        b::table
            .filter(b::repo_id.eq(repo_id))
            .filter(b::completed_at.is_not_null())
            .filter(
                b::requested_generation
                    .gt(b::completed_generation)
                    .or(b::incremental_generation.is_not_null())
                    .or(exists(
                        attention::table
                            .filter(attention::repo_id.eq(b::repo_id))
                            .filter(attention::pr_number.eq(b::pr_number)),
                    )),
            )
            .select(b::pr_number)
            .order(b::pr_number)
            .load::<i64>(&mut *self.conn.borrow_mut())
            .map(|numbers| numbers.into_iter().map(|number| number as u64).collect())
            .doing("reading pending activity refreshes")
    }
}

fn read_incremental_activity(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
) -> Result<Option<ActivityIncremental>> {
    b::table
        .find((repo_id, number as i64))
        .filter(b::completed_at.is_not_null())
        .select((
            b::incremental_revision,
            b::incremental_generation,
            b::incremental_cursor,
            b::incremental_next_rate_limit,
            b::incremental_stop_at,
            b::incremental_started_at,
        ))
        .first::<(
            i64,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        )>(conn)
        .optional()
        .doing(format!(
            "reading incremental activity progress for #{number}"
        ))?
        .map(
            |(revision, generation, cursor, next_rate_limit, stop_at, started_at)| {
                Ok(ActivityIncremental {
                    revision,
                    generation,
                    cursor,
                    next_rate_limit: next_rate_limit
                        .map(|value| activity_rate_limit_from_wire(&value))
                        .transpose()?,
                    started_at: started_at
                        .map(|at| decode_activity_timestamp(at, "incremental start"))
                        .transpose()?,
                    stop_at: stop_at
                        .map(|at| decode_activity_timestamp(at, "incremental activity boundary"))
                        .transpose()?,
                })
            },
        )
        .transpose()
}

pub(super) fn request_activity_refresh(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
) -> Result<()> {
    diesel::insert_into(b::table)
        .values((
            b::repo_id.eq(repo_id),
            b::pr_number.eq(number as i64),
            b::requested_generation.eq(1_i64),
        ))
        .on_conflict((b::repo_id, b::pr_number))
        .do_update()
        .set(b::requested_generation.eq(b::requested_generation + 1))
        .execute(conn)
        .doing(format!("requesting activity refresh for #{number}"))?;
    Ok(())
}

fn activity_relation_to_wire(relation: ActivityRelation) -> &'static str {
    match relation {
        ActivityRelation::Own => "own",
        ActivityRelation::Relevant => "relevant",
        ActivityRelation::Context => "context",
    }
}

fn activity_relation_from_wire(value: String) -> Result<ActivityRelation> {
    match value.as_str() {
        "own" => Ok(ActivityRelation::Own),
        "relevant" => Ok(ActivityRelation::Relevant),
        "context" => Ok(ActivityRelation::Context),
        _ => Err(LedgerError::Corrupt {
            what: "activity relation".into(),
            source: std::io::Error::other(format!("unknown activity relation {value:?}")).into(),
        }),
    }
}

fn activity_source_to_wire(source: ActivitySource) -> &'static str {
    match source {
        ActivitySource::Local => "local",
        ActivitySource::Forge => "forge",
    }
}

fn activity_source_from_wire(value: String) -> Result<ActivitySource> {
    match value.as_str() {
        "local" => Ok(ActivitySource::Local),
        "forge" => Ok(ActivitySource::Forge),
        _ => Err(LedgerError::Corrupt {
            what: "activity source".to_string(),
            source: std::io::Error::other(format!("unknown activity source {value:?}")).into(),
        }),
    }
}

fn activity_rate_limit_key(number: u64) -> String {
    format!("activity_backfill_rate_limit:{number}")
}

fn activity_rate_limit_to_wire(unit: ActivityRateLimitUnit) -> &'static str {
    match unit {
        ActivityRateLimitUnit::Requests => "requests",
        ActivityRateLimitUnit::Points => "points",
    }
}

fn activity_rate_limit_from_wire(value: &str) -> Result<ActivityRateLimitUnit> {
    match value {
        "requests" => Ok(ActivityRateLimitUnit::Requests),
        "points" => Ok(ActivityRateLimitUnit::Points),
        _ => Err(LedgerError::Corrupt {
            what: "activity rate-limit unit".to_string(),
            source: std::io::Error::other(format!("unknown activity rate-limit unit {value:?}"))
                .into(),
        }),
    }
}

fn activity_kind_to_wire(kind: ActivityKind) -> &'static str {
    match kind {
        ActivityKind::AttentionChanged => "attention_changed",
        ActivityKind::Done => "done",
        ActivityKind::Snoozed => "snoozed",
        ActivityKind::Muted => "muted",
        ActivityKind::Unmuted => "unmuted",
        ActivityKind::Deferred => "deferred",
        ActivityKind::Undeferred => "undeferred",
        ActivityKind::Tracked => "tracked",
        ActivityKind::Untracked => "untracked",
        ActivityKind::ReviewStarted => "review_started",
        ActivityKind::ReviewSubmitted => "review_submitted",
        ActivityKind::Commented => "commented",
        ActivityKind::ReviewThreadCommented => "review_thread_commented",
        ActivityKind::PrClosed => "pr_closed",
        ActivityKind::PrReopened => "pr_reopened",
        ActivityKind::PrMerged => "pr_merged",
        ActivityKind::ThreadResolved => "thread_resolved",
        ActivityKind::ThreadReopened => "thread_reopened",
    }
}

fn activity_kind_from_wire(value: String) -> Result<ActivityKind> {
    let kind = match value.as_str() {
        "attention_changed" => ActivityKind::AttentionChanged,
        "done" => ActivityKind::Done,
        "snoozed" => ActivityKind::Snoozed,
        "muted" => ActivityKind::Muted,
        "unmuted" => ActivityKind::Unmuted,
        "deferred" => ActivityKind::Deferred,
        "undeferred" => ActivityKind::Undeferred,
        "tracked" => ActivityKind::Tracked,
        "untracked" => ActivityKind::Untracked,
        "review_started" => ActivityKind::ReviewStarted,
        "review_submitted" => ActivityKind::ReviewSubmitted,
        "commented" => ActivityKind::Commented,
        "review_thread_commented" => ActivityKind::ReviewThreadCommented,
        "pr_closed" => ActivityKind::PrClosed,
        "pr_reopened" => ActivityKind::PrReopened,
        "pr_merged" => ActivityKind::PrMerged,
        "thread_resolved" => ActivityKind::ThreadResolved,
        "thread_reopened" => ActivityKind::ThreadReopened,
        _ => {
            return Err(LedgerError::Corrupt {
                what: "activity kind".to_string(),
                source: std::io::Error::other(format!("unknown activity kind {value:?}")).into(),
            });
        }
    };
    Ok(kind)
}

pub(super) fn decode_activity_timestamp(value: String, what: &str) -> Result<Timestamp> {
    value
        .parse()
        .map_err(|source: jiff::Error| LedgerError::Corrupt {
            what: what.to_string(),
            source: Box::new(source),
        })
}

fn activity_timestamp_to_wire(timestamp: Timestamp) -> String {
    timestamp.strftime("%Y-%m-%dT%H:%M:%S.%NZ").to_string()
}

#[derive(Queryable)]
struct StoredActivityEvent {
    id: ActivityEventId,
    repo_id: RepoId,
    host: String,
    owner: String,
    name: String,
    pr_number: i64,
    pr_title: String,
    source: String,
    kind: String,
    occurred_at: String,
    recorded_at: String,
    actor: Option<String>,
    head_sha: Option<String>,
    external_id: Option<String>,
    permalink: Option<String>,
    payload: String,
    relation: String,
}

fn activity_page_query(
    scope: ActivityScope,
    cursor: Option<&ActivityCursor>,
    limit: i64,
) -> impl diesel::query_dsl::LoadQuery<'static, DbConnection, StoredActivityEvent>
+ diesel::query_builder::QueryFragment<Sqlite> {
    let mut query = e::table
        .inner_join(
            prs::table.on(prs::repo_id
                .eq(e::repo_id)
                .and(prs::number.eq(e::pr_number))),
        )
        .inner_join(repos::table.on(repos::id.eq(e::repo_id)))
        .select((
            e::id,
            e::repo_id,
            repos::host,
            repos::owner,
            repos::name,
            e::pr_number,
            prs::title,
            e::source,
            e::kind,
            e::occurred_at,
            e::recorded_at,
            e::actor,
            e::head_sha,
            e::external_id,
            e::permalink,
            e::payload,
            e::relation,
        ))
        .into_boxed::<Sqlite>();
    if !matches!(scope, ActivityScope::PrAll { .. }) {
        query = query.filter(e::relation.ne("context"));
    }
    if let ActivityScope::Pr { repo_id, number } | ActivityScope::PrAll { repo_id, number } = scope
    {
        query = query
            .filter(e::repo_id.eq(repo_id))
            .filter(e::pr_number.eq(number as i64));
    }
    if let Some(cursor) = cursor {
        let occurred_at = activity_timestamp_to_wire(cursor.occurred_at);
        query = query.filter(
            e::occurred_at
                .lt(occurred_at.clone())
                .or(e::occurred_at.eq(occurred_at).and(e::id.lt(cursor.id))),
        );
    }
    query
        .order((e::occurred_at.desc(), e::id.desc()))
        .limit(limit)
}

fn activity_from_row(row: StoredActivityEvent) -> Result<ActivityEvent> {
    Ok(ActivityEvent {
        relation: activity_relation_from_wire(row.relation)?,
        id: row.id,
        repo: RepoKey {
            host: row.host,
            owner: row.owner,
            name: row.name,
        },
        repo_id: row.repo_id,
        pr_number: row.pr_number as u64,
        pr_title: row.pr_title,
        source: activity_source_from_wire(row.source)?,
        kind: activity_kind_from_wire(row.kind)?,
        occurred_at: decode_activity_timestamp(row.occurred_at, "activity occurred_at")?,
        recorded_at: decode_activity_timestamp(row.recorded_at, "activity recorded_at")?,
        actor: row.actor,
        head_sha: row.head_sha,
        external_id: row.external_id,
        permalink: row.permalink,
        payload: serde_json::from_str(&row.payload).map_err(|source| LedgerError::Corrupt {
            what: "activity payload".to_string(),
            source: Box::new(source),
        })?,
    })
}

fn lifecycle_kind(state: PrState) -> ActivityKind {
    match state {
        PrState::Open => ActivityKind::PrReopened,
        PrState::Closed => ActivityKind::PrClosed,
        PrState::Merged => ActivityKind::PrMerged,
    }
}

fn lifecycle_state(kind: ActivityKind) -> Option<PrState> {
    match kind {
        ActivityKind::PrReopened => Some(PrState::Open),
        ActivityKind::PrClosed => Some(PrState::Closed),
        ActivityKind::PrMerged => Some(PrState::Merged),
        _ => None,
    }
}

pub(super) fn record_state_transition_row(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    state: PrState,
    state_changed_at: Option<Timestamp>,
    observed_at: Timestamp,
) -> Result<bool> {
    let stored = prs::table
        .find((repo_id, number as i64))
        .select((prs::state, prs::state_changed_at))
        .first::<(String, Option<String>)>(conn)
        .optional()
        .doing(format!("reading #{number}'s lifecycle state"))?;
    let Some((stored_state, stored_changed_at)) = stored else {
        return Err(LedgerError::NotStored { number });
    };
    let stored_state = PrState::from_wire(&stored_state).ok_or_else(|| LedgerError::Corrupt {
        what: format!("state for #{number}"),
        source: std::io::Error::other(format!("unknown PR state {stored_state:?}")).into(),
    })?;
    if stored_state == state {
        let Some(authoritative) = state_changed_at else {
            return Ok(false);
        };
        let authoritative = activity_timestamp_to_wire(authoritative);
        if stored_changed_at.as_deref() == Some(authoritative.as_str()) {
            return Ok(false);
        }
        diesel::update(prs::table.find((repo_id, number as i64)))
            .set(prs::state_changed_at.eq(&authoritative))
            .execute(conn)
            .doing(format!("correcting #{number}'s lifecycle timestamp"))?;
        if let Some(observed) = stored_changed_at {
            correct_observed_lifecycle_event(
                conn,
                repo_id,
                number,
                lifecycle_kind(state),
                &observed,
                &authoritative,
            )?;
        }
        return Ok(false);
    }
    let occurred_at = state_changed_at.unwrap_or(observed_at);
    let relevant = lifecycle_affects_me(conn, repo_id, number, occurred_at)?;
    diesel::update(prs::table.find((repo_id, number as i64)))
        .set((
            prs::state.eq(state.as_str()),
            prs::state_changed_at.eq(activity_timestamp_to_wire(occurred_at)),
        ))
        .execute(conn)
        .doing(format!("recording #{number}'s lifecycle projection"))?;
    insert_activity(
        conn,
        repo_id,
        number,
        &NewActivityEvent {
            relation: if relevant {
                ActivityRelation::Relevant
            } else {
                ActivityRelation::Context
            },
            source: ActivitySource::Forge,
            kind: lifecycle_kind(state),
            occurred_at,
            recorded_at: observed_at,
            actor: None,
            head_sha: None,
            external_id: None,
            permalink: None,
            payload: ActivityPayload::StateChanged {
                from: stored_state,
                to: state,
            },
        },
        false,
        state_changed_at.is_none(),
    )
}

const LIFECYCLE_PARTICIPATION_KINDS: &[&str] = &[
    "done",
    "snoozed",
    "muted",
    "unmuted",
    "deferred",
    "undeferred",
    "attention_changed",
    "tracked",
    "review_started",
    "review_submitted",
    "commented",
    "review_thread_commented",
];

fn lifecycle_affects_me(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    at: Timestamp,
) -> Result<bool> {
    let Some(reason) = tracked_reason(conn, repo_id, number)? else {
        return Ok(false);
    };
    if reason == TrackedReason::Involved("author".into()).render() {
        return Ok(true);
    }
    let activity_at = activity_timestamp_to_wire(at);
    let at = crate::db_types::DbTimestamp::from(at);
    let has_attention = diesel::select(exists(
        attention::table
            .filter(attention::repo_id.eq(repo_id))
            .filter(attention::pr_number.eq(number as i64))
            .filter(attention::since.le(&at)),
    ))
    .get_result::<bool>(conn)
    .doing("checking lifecycle attention")?;
    let participated = diesel::select(exists(
        my_state::table
            .filter(my_state::repo_id.eq(repo_id))
            .filter(my_state::number.eq(number as i64))
            .filter(
                my_state::last_action_at
                    .le(&at)
                    .or(my_state::done_at.le(&at)),
            ),
    ))
    .get_result::<bool>(conn)
    .doing("checking lifecycle participation")?;
    let acted = diesel::select(exists(
        e::table
            .filter(e::repo_id.eq(repo_id))
            .filter(e::pr_number.eq(number as i64))
            .filter(e::relation.ne("context"))
            .filter(e::occurred_at.le(activity_at))
            .filter(e::kind.eq_any(LIFECYCLE_PARTICIPATION_KINDS)),
    ))
    .get_result::<bool>(conn)
    .doing("checking retained lifecycle participation")?;
    Ok(has_attention || participated || acted)
}

fn promote_lifecycle_after_participation(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    since: &str,
) -> Result<()> {
    let events = e::table
        .filter(e::repo_id.eq(repo_id))
        .filter(e::pr_number.eq(number as i64))
        .filter(e::relation.eq("context"))
        .filter(e::kind.eq_any(["pr_closed", "pr_reopened", "pr_merged"]))
        .filter(e::occurred_at.ge(since))
        .select((e::id, e::occurred_at))
        .load::<(ActivityEventId, String)>(conn)
        .doing("finding lifecycle activity affected by older participation")?;
    for (id, occurred_at) in events {
        let at = decode_activity_timestamp(occurred_at, "lifecycle occurred_at")?;
        if lifecycle_affects_me(conn, repo_id, number, at)? {
            diesel::update(e::table.find(id))
                .set(e::relation.eq("relevant"))
                .execute(conn)
                .doing("promoting lifecycle activity after older participation")?;
        }
    }
    Ok(())
}

pub(super) fn insert_activity(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    event: &NewActivityEvent,
    ignore_duplicate: bool,
    observed_transition: bool,
) -> Result<bool> {
    insert_activity_inner(
        conn,
        repo_id,
        number,
        event,
        ignore_duplicate,
        observed_transition,
        false,
    )
}

pub(super) fn insert_evidence(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    event: &NewActivityEvent,
) -> Result<bool> {
    insert_activity_inner(conn, repo_id, number, event, true, false, true)
}

#[allow(clippy::too_many_arguments)]
fn insert_activity_inner(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    event: &NewActivityEvent,
    ignore_duplicate: bool,
    observed_transition: bool,
    retain: bool,
) -> Result<bool> {
    let source = activity_source_to_wire(event.source);
    let kind = activity_kind_to_wire(event.kind);
    let occurred_at = activity_timestamp_to_wire(event.occurred_at);
    let before_retention = !retain && before_activity_retention(conn, &occurred_at)?;
    let recorded_at = activity_timestamp_to_wire(event.recorded_at);
    match event.kind {
        ActivityKind::PrMerged => {
            diesel::delete(
                e::table
                    .filter(e::repo_id.eq(repo_id))
                    .filter(e::pr_number.eq(number as i64))
                    .filter(e::kind.eq("pr_closed"))
                    .filter(e::occurred_at.eq(&occurred_at)),
            )
            .execute(conn)
            .doing("removing the close accompanying a merge")?;
        }
        ActivityKind::PrClosed => {
            let merged: bool = diesel::select(exists(
                e::table
                    .filter(e::repo_id.eq(repo_id))
                    .filter(e::pr_number.eq(number as i64))
                    .filter(e::kind.eq("pr_merged"))
                    .filter(e::occurred_at.eq(&occurred_at)),
            ))
            .get_result(conn)
            .doing("checking whether a close accompanied a merge")?;
            if merged {
                return Ok(false);
            }
        }
        _ => {}
    }
    let payload = match (&event.payload, lifecycle_state(event.kind)) {
        (ActivityPayload::None, Some(to)) => ActivityPayload::StateChanged {
            from: match to {
                PrState::Open => PrState::Closed,
                PrState::Closed | PrState::Merged => PrState::Open,
            },
            to,
        },
        (payload, _) => payload.clone(),
    };
    let payload = serde_json::to_string(&payload).encoding("activity payload")?;
    if ignore_duplicate && let Some(state) = lifecycle_state(event.kind) {
        if let Some(external_id) = event.external_id.as_deref() {
            let already_stored: bool = diesel::select(exists(
                e::table
                    .filter(e::repo_id.eq(repo_id))
                    .filter(e::kind.eq(kind))
                    .filter(e::external_id.eq(external_id)),
            ))
            .get_result(conn)
            .doing("checking lifecycle provider identity")?;
            if already_stored {
                return Ok(false);
            }
        }
        let mut exact_query = e::table
            .filter(e::repo_id.eq(repo_id))
            .filter(e::pr_number.eq(number as i64))
            .filter(e::kind.eq(kind))
            .filter(e::occurred_at.eq(&occurred_at))
            .filter(e::observed_transition.eq(false))
            .into_boxed::<Sqlite>();
        if event.external_id.is_some() {
            exact_query = exact_query.filter(e::external_id.is_null());
        }
        let exact_authoritative = exact_query
            .select((e::id, e::external_id))
            .order(e::id.desc())
            .first::<(ActivityEventId, Option<String>)>(conn)
            .optional()
            .doing(format!(
                "checking #{number}'s lifecycle transition identity"
            ))?;
        if let Some((id, stored_external_id)) = exact_authoritative {
            if stored_external_id.is_none() && event.external_id.is_some() {
                update_authoritative_lifecycle(
                    conn,
                    id,
                    event,
                    source,
                    &occurred_at,
                    &recorded_at,
                    &payload,
                )?;
            }
            return Ok(false);
        }
        let observed = e::table
            .filter(e::repo_id.eq(repo_id))
            .filter(e::pr_number.eq(number as i64))
            .filter(e::kind.eq(kind))
            .filter(e::observed_transition.eq(true))
            .filter(e::occurred_at.ge(&occurred_at))
            .select((e::id, e::occurred_at))
            .order((e::occurred_at.asc(), e::id.desc()))
            .first::<(ActivityEventId, String)>(conn)
            .optional()
            .doing(format!("finding #{number}'s observed lifecycle transition"))?;
        if let Some((id, observed_at)) = observed {
            update_authoritative_lifecycle(
                conn,
                id,
                event,
                source,
                &occurred_at,
                &recorded_at,
                &payload,
            )?;
            diesel::update(
                prs::table
                    .find((repo_id, number as i64))
                    .filter(prs::state.eq(state.as_str()))
                    .filter(prs::state_changed_at.eq(observed_at)),
            )
            .set(prs::state_changed_at.eq(&occurred_at))
            .execute(conn)
            .doing(format!("reconciling #{number}'s lifecycle timestamp"))?;
            return Ok(false);
        }
    }
    if before_retention {
        return Ok(false);
    }
    let values = (
        e::repo_id.eq(repo_id),
        e::pr_number.eq(number as i64),
        e::source.eq(source),
        e::kind.eq(kind),
        e::occurred_at.eq(&occurred_at),
        e::recorded_at.eq(&recorded_at),
        e::actor.eq(event.actor.as_deref()),
        e::head_sha.eq(event.head_sha.as_deref()),
        e::external_id.eq(event.external_id.as_deref()),
        e::permalink.eq(event.permalink.as_deref()),
        e::payload.eq(&payload),
        e::observed_transition.eq(observed_transition),
        e::relation.eq(activity_relation_to_wire(event.relation)),
    );
    let insert = diesel::insert_into(e::table).values(values);
    let changed = if ignore_duplicate {
        insert.on_conflict_do_nothing().execute(conn)
    } else {
        insert.execute(conn)
    }
    .doing(format!("recording activity for #{number}"))?;
    if event.relation != ActivityRelation::Context && LIFECYCLE_PARTICIPATION_KINDS.contains(&kind)
    {
        promote_lifecycle_after_participation(conn, repo_id, number, &occurred_at)?;
    }
    Ok(changed == 1)
}

#[allow(clippy::too_many_arguments)]
fn update_authoritative_lifecycle(
    conn: &mut DbConnection,
    id: ActivityEventId,
    event: &NewActivityEvent,
    source: &str,
    occurred_at: &str,
    recorded_at: &str,
    payload: &str,
) -> Result<()> {
    if before_activity_retention(conn, occurred_at)? {
        diesel::delete(e::table.find(id))
            .execute(conn)
            .doing("discarding authoritative lifecycle activity before retention")?;
        return Ok(());
    }
    diesel::update(e::table.find(id))
        .set((
            e::source.eq(source),
            e::occurred_at.eq(occurred_at),
            e::recorded_at.eq(recorded_at),
            e::actor.eq(event.actor.as_deref()),
            e::head_sha.eq(event.head_sha.as_deref()),
            e::external_id.eq(event.external_id.as_deref()),
            e::permalink.eq(event.permalink.as_deref()),
            e::payload.eq(payload),
            e::observed_transition.eq(false),
            e::relation.eq(activity_relation_to_wire(event.relation)),
        ))
        .execute(conn)
        .doing("reconciling authoritative lifecycle activity")?;
    Ok(())
}

fn correct_observed_lifecycle_event(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    kind: ActivityKind,
    observed_at: &str,
    authoritative_at: &str,
) -> Result<()> {
    let before_retention = before_activity_retention(conn, authoritative_at)?;
    let observed = e::table
        .filter(e::repo_id.eq(repo_id))
        .filter(e::pr_number.eq(number as i64))
        .filter(e::kind.eq(activity_kind_to_wire(kind)))
        .filter(e::observed_transition.eq(true))
        .filter(e::occurred_at.eq(observed_at))
        .order(e::id.desc())
        .select(e::id)
        .first::<ActivityEventId>(conn)
        .optional()
        .doing(format!("finding #{number}'s lifecycle event"))?;
    let Some(id) = observed else {
        return Ok(());
    };
    if before_retention {
        diesel::delete(e::table.find(id))
            .execute(conn)
            .doing(format!(
                "discarding #{number}'s lifecycle event before retention"
            ))?;
    } else {
        diesel::update(e::table.find(id))
            .set((
                e::occurred_at.eq(authoritative_at),
                e::observed_transition.eq(false),
            ))
            .execute(conn)
            .doing(format!("correcting #{number}'s lifecycle event timestamp"))?;
    }
    Ok(())
}

fn before_activity_retention(conn: &mut DbConnection, occurred_at: &str) -> Result<bool> {
    diesel::select(exists(
        retention::table.filter(retention::cutoff.gt(occurred_at)),
    ))
    .get_result(conn)
    .doing("checking activity retention")
}

fn reconcile_lifecycle_projection(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
) -> Result<()> {
    let latest = e::table
        .filter(e::repo_id.eq(repo_id))
        .filter(e::pr_number.eq(number as i64))
        .filter(e::observed_transition.eq(false))
        .filter(e::kind.eq_any(["pr_closed", "pr_reopened", "pr_merged"]))
        .select((e::kind, e::occurred_at))
        .order((e::occurred_at.desc(), e::id.desc()))
        .first::<(String, String)>(conn)
        .optional()
        .doing(format!("reading #{number}'s latest lifecycle activity"))?;
    let Some((kind, occurred_at)) = latest else {
        return Ok(());
    };
    let kind = activity_kind_from_wire(kind)?;
    let Some(latest_state) = lifecycle_state(kind) else {
        return Ok(());
    };
    let current = prs::table
        .find((repo_id, number as i64))
        .select((prs::state, prs::state_changed_at))
        .first::<(String, Option<String>)>(conn)
        .optional()
        .doing(format!("reading #{number}'s lifecycle projection"))?;
    let Some((current_state, current_at)) = current else {
        return Err(LedgerError::NotStored { number });
    };
    let current_state = PrState::from_wire(&current_state).ok_or_else(|| LedgerError::Corrupt {
        what: format!("state for #{number}"),
        source: std::io::Error::other(format!("unknown PR state {current_state:?}")).into(),
    })?;
    let latest_at = decode_activity_timestamp(occurred_at.clone(), "lifecycle occurred_at")?;
    let should_apply = match current_at {
        Some(current_at) => {
            let current_at = decode_activity_timestamp(current_at, "state_changed_at")?;
            latest_at > current_at
                || (latest_at == current_at
                    && latest_state == PrState::Merged
                    && current_state == PrState::Closed)
        }
        None => latest_state == current_state,
    };
    if should_apply {
        diesel::update(prs::table.find((repo_id, number as i64)))
            .set((
                prs::state.eq(latest_state.as_str()),
                prs::state_changed_at.eq(occurred_at),
            ))
            .execute(conn)
            .doing(format!("applying #{number}'s latest lifecycle activity"))?;
    }
    Ok(())
}

fn preview_activity_cleanup(conn: &mut DbConnection, cutoff: Timestamp) -> Result<CleanupPreview> {
    let cutoff = activity_timestamp_to_wire(cutoff);
    let event_count = e::table
        .filter(e::occurred_at.lt(&cutoff))
        .filter(
            e::id.ne_all(
                crate::schema::threads::table
                    .filter(crate::schema::threads::resolution_event_id.is_not_null())
                    .select(crate::schema::threads::resolution_event_id.assume_not_null()),
            ),
        )
        .filter(
            e::id.ne_all(
                crate::schema::my_state::table
                    .filter(crate::schema::my_state::last_review_event_id.is_not_null())
                    .select(crate::schema::my_state::last_review_event_id.assume_not_null()),
            ),
        )
        .select(count_star())
        .first::<i64>(conn)
        .doing("previewing activity cleanup event count")?;
    let pr_count = prs::table
        .filter(exists(
            e::table
                .filter(e::repo_id.eq(prs::repo_id))
                .filter(e::pr_number.eq(prs::number))
                .filter(e::occurred_at.lt(&cutoff))
                .filter(
                    e::id.ne_all(
                        crate::schema::threads::table
                            .filter(crate::schema::threads::resolution_event_id.is_not_null())
                            .select(crate::schema::threads::resolution_event_id.assume_not_null()),
                    ),
                )
                .filter(
                    e::id.ne_all(
                        crate::schema::my_state::table
                            .filter(crate::schema::my_state::last_review_event_id.is_not_null())
                            .select(
                                crate::schema::my_state::last_review_event_id.assume_not_null(),
                            ),
                    ),
                ),
        ))
        .select(count_star())
        .first::<i64>(conn)
        .doing("previewing activity cleanup PR count")?;
    Ok(CleanupPreview {
        event_count: event_count as u64,
        pr_count: pr_count as u64,
    })
}

/// Truncate a timestamp to whole seconds, dropping sub-second precision so
/// stored stamps compare lexicographically against GitHub's whole-second form.
pub(super) fn whole_second(ts: Timestamp) -> Timestamp {
    Timestamp::from_second(ts.as_second()).unwrap_or(ts)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use diesel::connection::{InstrumentationEvent, SimpleConnection as _};
    use diesel::{Connection as _, QueryableByName};

    use crate::schema::{activity_events, activity_retention, prs};

    use super::*;
    use reviewq_core::model::{ClassifyCtx, ReviewResult, ThreadState};

    fn repo() -> RepoKey {
        RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        }
    }

    fn pr(number: u64) -> PrSnapshot {
        PrSnapshot {
            number,
            title: format!("PR {number}"),
            author: "octocat".into(),
            author_association: "CONTRIBUTOR".into(),
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: "2026-08-05T12:00:00Z".parse().unwrap(),
            created_at: None,
            state_changed_at: None,
            labels: vec!["area:task-sdk".into()],
            milestone: Some("3.2.0".into()),
            files: None,
            files_truncated: false,
        }
    }

    fn now() -> Timestamp {
        "2026-08-05T12:00:00Z".parse().unwrap()
    }

    fn interest(rule: &str) -> TrackedReason {
        TrackedReason::Interest {
            rule: rule.into(),
            after_merge: false,
        }
    }

    fn ledger_with_repo() -> (Ledger, RepoId) {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo()).unwrap();
        (ledger, repo_id)
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn track(ledger: &Ledger, repo_id: RepoId, pr: &PrSnapshot) {
        ledger
            .upsert_pr(repo_id, pr, Some(interest("label area:task-sdk")))
            .unwrap();
    }

    fn ledger_with_pr(number: u64) -> (Ledger, RepoId) {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(number));
        (ledger, repo_id)
    }

    #[test]
    fn relevant_reviews_by_other_people_never_acknowledge_my_attention() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let mut review = forge_review("someone-else", ReviewResult::Approved);
        review.actor = Some("alice".into());
        review.relation = ActivityRelation::Relevant;
        ledger.record_forge_activity(repo_id, 1, &review).unwrap();
        let ack = my_state::table
            .find((repo_id, 1_i64))
            .select(my_state::last_review_event_id)
            .first::<Option<ActivityEventId>>(&mut *ledger.conn.borrow_mut())
            .optional()
            .unwrap()
            .flatten();
        assert_eq!(ack, None);
        assert_eq!(all_activity(&ledger).len(), 1);
    }

    #[test]
    fn contextual_events_only_appear_in_full_pr_history_and_do_not_break_pagination() {
        let (ledger, repo_id) = ledger_with_pr(1);
        for (at, relation) in [
            ("2026-08-01T00:00:00Z", ActivityRelation::Own),
            ("2026-08-02T00:00:00Z", ActivityRelation::Context),
            ("2026-08-03T00:00:00Z", ActivityRelation::Relevant),
        ] {
            let mut event = local_activity(ActivityKind::Commented, at);
            event.relation = relation;
            ledger.record_activity(repo_id, 1, &event).unwrap();
        }
        for scope in [ActivityScope::All, ActivityScope::Pr { repo_id, number: 1 }] {
            let first = ledger.activity_page(scope, None, 1).unwrap();
            assert_eq!(first.events[0].relation, ActivityRelation::Relevant);
            let second = ledger.activity_page(scope, first.next.as_ref(), 1).unwrap();
            assert_eq!(second.events[0].relation, ActivityRelation::Own);
            assert!(second.next.is_none());
        }
        let all = ledger
            .activity_page(ActivityScope::PrAll { repo_id, number: 1 }, None, 10)
            .unwrap();
        assert_eq!(all.events.len(), 3);
        assert_eq!(all.events[1].relation, ActivityRelation::Context);
    }

    #[test]
    fn older_participation_promotes_lifecycle_across_backfill_pages() {
        for (relation, participation_at, expected) in [
            (
                ActivityRelation::Own,
                "2026-08-03T12:00:00Z",
                ActivityRelation::Relevant,
            ),
            (
                ActivityRelation::Relevant,
                "2026-08-03T12:00:00Z",
                ActivityRelation::Relevant,
            ),
            (
                ActivityRelation::Context,
                "2026-08-03T12:00:00Z",
                ActivityRelation::Context,
            ),
            (
                ActivityRelation::Own,
                "2026-08-05T12:00:00Z",
                ActivityRelation::Context,
            ),
        ] {
            let (ledger, repo_id) = ledger_with_pr(1);
            let mut close =
                forge_lifecycle(ActivityKind::PrClosed, "closed", "2026-08-04T12:00:00Z");
            close.relation = ActivityRelation::Context;
            close.actor = Some("someone-else".into());
            ledger
                .commit_activity_page(repo_id, 1, None, &[close], Some("older"), None, now())
                .unwrap();
            assert!(all_activity(&ledger).is_empty());
            let mut comment = local_activity(ActivityKind::Commented, participation_at);
            comment.relation = relation;
            comment.source = ActivitySource::Forge;
            comment.external_id = Some("comment".into());
            ledger
                .commit_activity_page(repo_id, 1, Some("older"), &[comment], None, None, now())
                .unwrap();
            let page = ledger
                .activity_page(ActivityScope::PrAll { repo_id, number: 1 }, None, 10)
                .unwrap();
            let close = page
                .events
                .iter()
                .find(|event| event.kind == ActivityKind::PrClosed)
                .unwrap();
            assert_eq!(
                close.relation, expected,
                "participation: {relation:?} at {participation_at}"
            );
            assert_eq!(
                all_activity(&ledger)
                    .iter()
                    .any(|event| event.kind == ActivityKind::PrClosed),
                expected == ActivityRelation::Relevant
            );
        }
    }

    #[test]
    fn coverage_advances_only_after_a_complete_traversal_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = Ledger::open(&path).unwrap();
        let repo_id = ledger.ensure_repo(&repo()).unwrap();
        track(&ledger, repo_id, &pr(1));
        ledger
            .commit_activity_page(repo_id, 1, None, &[], Some("older"), None, now())
            .unwrap();
        let later = ts("2026-08-25T12:00:00Z");
        ledger
            .commit_activity_page(repo_id, 1, Some("older"), &[], None, None, later)
            .unwrap();
        let checkpoint = ledger
            .begin_incremental_activity(repo_id, 1, later)
            .unwrap();
        assert_eq!(
            checkpoint.stop_at,
            Some(now() - jiff::SignedDuration::from_mins(5))
        );
        assert_eq!(checkpoint.started_at, Some(later));
        ledger
            .commit_incremental_activity_page(repo_id, 1, &checkpoint, &[], Some("resume"), None)
            .unwrap();
        let mut inserted = forge_review("detail", ReviewResult::Approved);
        inserted.occurred_at = ts("2026-08-26T12:00:00Z");
        ledger.record_forge_activity(repo_id, 1, &inserted).unwrap();
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        let resumed = ledger
            .begin_incremental_activity(repo_id, 1, inserted.occurred_at)
            .unwrap();
        assert_eq!(resumed.started_at, checkpoint.started_at);
        assert_eq!(resumed.stop_at, checkpoint.stop_at);
        assert_eq!(resumed.cursor.as_deref(), Some("resume"));
        let covered = b::table
            .find((repo_id, 1_i64))
            .select(b::covered_through)
            .first::<Option<String>>(&mut *ledger.conn.borrow_mut())
            .unwrap();
        assert_eq!(covered, Some(activity_timestamp_to_wire(now())));
        ledger
            .commit_incremental_activity_page(repo_id, 1, &resumed, &[], None, None)
            .unwrap();
        let next = ledger
            .begin_incremental_activity(repo_id, 1, inserted.occurred_at)
            .unwrap();
        assert_eq!(
            next.stop_at,
            Some(later - jiff::SignedDuration::from_mins(5))
        );
    }

    #[test]
    fn cleanup_retains_review_evidence_without_null_references_blocking_other_deletions() {
        let (ledger, repo_id) = ledger_with_pr(1);
        track(&ledger, repo_id, &pr(2));
        ledger.set_muted(repo_id, 2, true).unwrap();
        let review = forge_review("retained", ReviewResult::Approved);
        ledger.record_forge_activity(repo_id, 1, &review).unwrap();
        ledger.set_done(repo_id, 1, "head", now()).unwrap();
        ledger
            .record_activity(
                repo_id,
                2,
                &local_activity(ActivityKind::Muted, "2026-08-01T00:00:00Z"),
            )
            .unwrap();
        let preview = ledger.clean_activity(now()).unwrap();
        assert_eq!(preview.event_count, 1);
        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].external_id.as_deref(), Some("retained"));
        assert_eq!(
            ledger.my_state(repo_id, 1).unwrap().done_sha.as_deref(),
            Some("head")
        );
        assert!(ledger.my_state(repo_id, 2).unwrap().muted);
    }

    #[test]
    fn only_the_latest_review_can_bypass_retention_and_acknowledgment_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let ledger = Ledger::open(&path).unwrap();
        let repo_id = ledger.ensure_repo(&repo()).unwrap();
        track(&ledger, repo_id, &pr(1));
        ledger.clean_activity(now()).unwrap();
        let older = forge_review("older", ReviewResult::Commented);
        let mut latest = forge_review("latest", ReviewResult::Approved);
        latest.occurred_at = ts("2026-08-05T11:00:00Z");
        let _ = ledger
            .commit_detail_with_activity(
                repo_id,
                &pr(1),
                &MyState::default(),
                &[],
                &[],
                "",
                None,
                &[older.clone(), latest.clone()],
                &ClassifyCtx::default(),
                now(),
            )
            .unwrap();
        assert_eq!(all_activity(&ledger).len(), 1);
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        let _ = ledger
            .commit_detail_with_activity(
                repo_id,
                &pr(1),
                &MyState::default(),
                &[],
                &[],
                "",
                None,
                &[older],
                &ClassifyCtx::default(),
                now(),
            )
            .unwrap();
        let (resolutions, reviewed_at) = crate::detail_activity::observe_detail(
            &mut ledger.conn.borrow_mut(),
            repo_id,
            1,
            &[],
            &[],
            None,
            now(),
        )
        .unwrap();
        assert!(resolutions.is_empty());
        assert_eq!(reviewed_at, Some(latest.occurred_at));
        assert_eq!(all_activity(&ledger).len(), 1);
    }

    #[test]
    fn failed_detail_evidence_rolls_back_the_watermark_and_attention() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger.conn.borrow_mut().batch_execute("CREATE TRIGGER reject_review BEFORE INSERT ON activity_events BEGIN SELECT RAISE(ABORT, 'rejected'); END;").unwrap();
        let review = forge_review("review", ReviewResult::Approved);
        assert!(
            ledger
                .commit_detail_with_activity(
                    repo_id,
                    &pr(1),
                    &MyState::default(),
                    &[],
                    &[],
                    "new body",
                    None,
                    &[review],
                    &ClassifyCtx::default(),
                    now()
                )
                .is_err()
        );
        assert!(all_activity(&ledger).is_empty());
        let watermark = prs::table
            .find((repo_id, 1_i64))
            .select(prs::detail_synced_at)
            .first::<Option<String>>(&mut *ledger.conn.borrow_mut())
            .unwrap();
        assert_eq!(watermark, None);
    }

    #[test]
    fn history_reviews_acknowledge_only_earlier_resolutions_and_are_retained() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let first = ThreadState {
            thread_id: "first".into(),
            i_own: true,
            is_resolved: true,
            resolved_by: Some("author".into()),
            last_comment_author: Some("octocat".into()),
            last_comment_at: Some(now()),
            my_last_comment_at: Some(now()),
        };
        let _ = ledger
            .commit_detail_with_activity(
                repo_id,
                &pr(1),
                &MyState::default(),
                std::slice::from_ref(&first),
                &[],
                "",
                None,
                &[],
                &ClassifyCtx::default(),
                now(),
            )
            .unwrap();
        let second_time = ts("2026-08-07T12:00:00Z");
        let threads = [
            first.clone(),
            ThreadState {
                thread_id: "second".into(),
                ..first
            },
        ];
        let _ = ledger
            .commit_detail_with_activity(
                repo_id,
                &pr(1),
                &MyState::default(),
                &threads,
                &[],
                "",
                None,
                &[],
                &ClassifyCtx::default(),
                second_time,
            )
            .unwrap();
        let mut review = forge_review("between", ReviewResult::Commented);
        review.occurred_at = ts("2026-08-06T12:00:00Z");
        assert_eq!(
            ledger
                .commit_activity_page(repo_id, 1, None, &[review.clone()], None, None, second_time)
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 1 }
        );
        let attention = ledger.show(repo_id, 1).unwrap().unwrap().attention;
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].since, second_time);
        assert!(matches!(
            attention[0].reason,
            reviewq_core::model::AttentionReason::ResolvedUnanswered { threads: 1, .. }
        ));
        review.external_id = Some("after-both".into());
        review.occurred_at = ts("2026-08-08T12:00:00Z");
        let checkpoint = ledger
            .begin_incremental_activity(repo_id, 1, review.occurred_at)
            .unwrap();
        assert_eq!(
            ledger
                .commit_incremental_activity_page(repo_id, 1, &checkpoint, &[review], None, None)
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 1 }
        );
        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .attention
                .is_empty()
        );
        let _ = ledger
            .commit_detail_with_activity(
                repo_id,
                &pr(1),
                &MyState::default(),
                &threads,
                &[],
                "",
                None,
                &[],
                &ClassifyCtx::default(),
                ts("2026-08-09T12:00:00Z"),
            )
            .unwrap();
        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .attention
                .is_empty()
        );
        assert_eq!(
            ledger
                .clean_activity(ts("2026-08-10T12:00:00Z"))
                .unwrap()
                .event_count,
            5
        );
        assert_eq!(all_activity(&ledger).len(), 3);
        assert!(
            all_activity(&ledger)
                .iter()
                .any(|event| event.external_id.as_deref() == Some("after-both"))
        );
    }

    #[test]
    fn detail_reviews_acknowledge_observed_resolutions_and_survive_retention() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let mut snapshot = pr(1);
        let mut thread = ThreadState {
            thread_id: "thread".into(),
            i_own: true,
            is_resolved: true,
            resolved_by: Some("alice".into()),
            last_comment_author: Some("octocat".into()),
            last_comment_at: Some(now()),
            my_last_comment_at: Some(now()),
        };
        let resolution_id = || {
            crate::schema::threads::table
                .find("thread")
                .select(crate::schema::threads::resolution_event_id)
                .first::<Option<ActivityEventId>>(&mut *ledger.conn.borrow_mut())
                .unwrap()
        };
        let mut mine = MyState::default();
        let ctx = ClassifyCtx::default();
        let commit = |snapshot: &PrSnapshot,
                      mine: &MyState,
                      thread: &ThreadState,
                      events: &[NewActivityEvent],
                      at| {
            ledger
                .commit_detail_with_activity(
                    repo_id,
                    snapshot,
                    mine,
                    std::slice::from_ref(thread),
                    &[],
                    "",
                    None,
                    events,
                    &ctx,
                    at,
                )
                .unwrap()
        };
        let _ = commit(&snapshot, &mine, &thread, &[], now());
        let original_resolution = resolution_id().unwrap();
        let attention = ledger.show(repo_id, 1).unwrap().unwrap().attention;
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].since, now());

        let later = ts("2026-08-06T12:00:00Z");
        snapshot.updated_at = later;
        mine.last_action_at = Some(later);
        let _ = commit(&snapshot, &mine, &thread, &[], later);
        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().attention[0].since,
            now()
        );
        assert_eq!(all_activity(&ledger).len(), 2);
        assert_eq!(resolution_id(), Some(original_resolution));

        let mut review = forge_review("review", ReviewResult::Commented);
        review.occurred_at = later;
        let _ = commit(&snapshot, &mine, &thread, &[review.clone()], later);
        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .attention
                .is_empty()
        );
        assert_eq!(
            ledger
                .clean_activity(ts("2026-08-07T00:00:00Z"))
                .unwrap()
                .event_count,
            2
        );
        let _ = commit(&snapshot, &mine, &thread, &[], later);
        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .attention
                .is_empty()
        );

        thread.is_resolved = false;
        thread.resolved_by = None;
        let _ = commit(&snapshot, &mine, &thread, &[], ts("2026-08-08T00:00:00Z"));
        assert_eq!(resolution_id(), None);
        thread.is_resolved = true;
        thread.resolved_by = Some("carol".into());
        let resolved_again = ts("2026-08-09T00:00:00Z");
        let _ = commit(&snapshot, &mine, &thread, &[], resolved_again);
        assert_ne!(resolution_id(), Some(original_resolution));
        let events = all_activity(&ledger);
        let transitions: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    ActivityKind::ThreadResolved | ActivityKind::ThreadReopened
                )
            })
            .map(|event| (event.kind, event.actor.as_deref()))
            .collect();
        assert_eq!(
            transitions,
            vec![
                (ActivityKind::ThreadResolved, Some("carol")),
                (ActivityKind::ThreadReopened, None),
                (ActivityKind::ThreadResolved, Some("alice")),
            ]
        );

        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().attention[0].since,
            resolved_again
        );
        assert_eq!(
            ledger
                .clean_activity(ts("2026-08-07T00:00:00Z"))
                .unwrap()
                .event_count,
            1
        );
        assert_eq!(all_activity(&ledger).len(), 4);

        review.external_id = Some("stale-review".into());
        let count = all_activity(&ledger).len();
        assert!(matches!(
            commit(&snapshot, &mine, &thread, &[review], later),
            crate::Committed::Superseded { .. }
        ));
        assert_eq!(all_activity(&ledger).len(), count);
        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().attention[0].since,
            resolved_again
        );
    }

    #[test]
    fn unrelated_thread_transitions_remain_context_without_retention_pins() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let mut thread = ThreadState {
            thread_id: "unrelated".into(),
            i_own: false,
            is_resolved: true,
            resolved_by: Some("alice".into()),
            last_comment_author: Some("bob".into()),
            last_comment_at: Some(now()),
            my_last_comment_at: None,
        };
        for (resolved, at, expected_count) in [
            (true, "2026-08-05T12:00:00Z", 1),
            (true, "2026-08-05T12:01:00Z", 1),
            (false, "2026-08-05T12:02:00Z", 2),
        ] {
            thread.is_resolved = resolved;
            thread.resolved_by = resolved.then(|| "alice".into());
            let _ = ledger
                .commit_detail_with_activity(
                    repo_id,
                    &pr(1),
                    &MyState::default(),
                    std::slice::from_ref(&thread),
                    &[],
                    "",
                    None,
                    &[],
                    &ClassifyCtx::default(),
                    ts(at),
                )
                .unwrap();
            assert!(all_activity(&ledger).is_empty());
            let page = ledger
                .activity_page(ActivityScope::PrAll { repo_id, number: 1 }, None, 10)
                .unwrap();
            assert_eq!(page.events.len(), expected_count);
            assert!(
                page.events
                    .iter()
                    .all(|event| event.relation == ActivityRelation::Context)
            );
            if !resolved {
                assert_eq!(page.events[0].kind, ActivityKind::ThreadReopened);
            }
            let reference = crate::schema::threads::table
                .find("unrelated")
                .select(crate::schema::threads::resolution_event_id)
                .first::<Option<ActivityEventId>>(&mut *ledger.conn.borrow_mut())
                .unwrap();
            assert_eq!(reference, None);
        }
        assert_eq!(ledger.show(repo_id, 1).unwrap().unwrap().threads.len(), 1);
        assert_eq!(
            ledger
                .clean_activity(ts("2026-08-06T00:00:00Z"))
                .unwrap()
                .event_count,
            2
        );
    }

    #[test]
    fn unrelated_lifecycle_updates_the_snapshot_without_history() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let mut snapshot = pr(1);
        snapshot.state = PrState::Merged;
        snapshot.updated_at = ts("2026-08-06T12:00:00Z");
        ledger.upsert_pr(repo_id, &snapshot, None).unwrap();
        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().pr.state,
            PrState::Merged
        );
        assert!(all_activity(&ledger).is_empty());
    }

    #[test]
    fn lifecycle_relevance_survives_cleared_attention_and_authored_prs_need_no_review() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let reason = reviewq_core::model::Attention {
            priority: false,
            reason: reviewq_core::model::AttentionReason::Mention { by: "alice".into() },
            since: now(),
        };
        let _ = ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[reason],
                None,
                now(),
            )
            .unwrap();
        let closed_at = ts("2026-08-06T12:00:00Z");
        ledger
            .record_state_transition(repo_id, 1, PrState::Closed, None, closed_at)
            .unwrap();
        ledger.clear_attention(repo_id, 1).unwrap();
        assert!(
            !ledger
                .lifecycle_affects_me(
                    repo_id,
                    1,
                    ts("2026-07-01T00:00:00Z"),
                    ActivityKind::PrClosed,
                    "me"
                )
                .unwrap()
        );
        assert!(
            ledger
                .lifecycle_affects_me(repo_id, 1, closed_at, ActivityKind::PrClosed, "me")
                .unwrap()
        );
        assert!(
            !ledger
                .lifecycle_affects_me(repo_id, 1, closed_at, ActivityKind::PrReopened, "me")
                .unwrap()
        );

        track(&ledger, repo_id, &pr(2));
        assert!(
            ledger
                .lifecycle_affects_me(repo_id, 2, closed_at, ActivityKind::PrClosed, "OCTOCAT")
                .unwrap()
        );
        ledger
            .upsert_pr(
                repo_id,
                &pr(2),
                Some(TrackedReason::Involved("author".into())),
            )
            .unwrap();
        assert!(
            ledger
                .record_state_transition(repo_id, 2, PrState::Closed, None, closed_at)
                .unwrap()
        );
    }

    #[test]
    fn participation_or_my_resolution_makes_a_thread_transition_relevant() {
        for (participated, resolver, expected) in
            [(true, "alice", 1), (false, "ME", 1), (false, "alice", 0)]
        {
            let (ledger, repo_id) = ledger_with_pr(1);
            let thread = ThreadState {
                thread_id: "thread".into(),
                i_own: false,
                is_resolved: true,
                resolved_by: Some(resolver.into()),
                last_comment_author: Some("alice".into()),
                last_comment_at: Some(now()),
                my_last_comment_at: participated.then_some(now()),
            };
            let ctx = ClassifyCtx {
                viewer: Some("me"),
                ..Default::default()
            };
            for _ in 0..2 {
                let _ = ledger
                    .commit_detail_with_activity(
                        repo_id,
                        &pr(1),
                        &MyState::default(),
                        std::slice::from_ref(&thread),
                        &[],
                        "",
                        None,
                        &[],
                        &ctx,
                        now(),
                    )
                    .unwrap();
            }
            assert_eq!(all_activity(&ledger).len(), expected);
            assert!(
                ledger
                    .show(repo_id, 1)
                    .unwrap()
                    .unwrap()
                    .attention
                    .is_empty()
            );
        }
    }

    #[test]
    fn unrelated_pr_updates_do_not_repeat_a_first_look_history_entry() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let mut snapshot = pr(1);
        let ctx = ClassifyCtx {
            interest: Some("label area:task-sdk"),
            ..Default::default()
        };
        for at in [now(), ts("2026-08-06T12:00:00Z")] {
            snapshot.updated_at = at;
            let _ = ledger
                .commit_detail_with_activity(
                    repo_id,
                    &snapshot,
                    &MyState::default(),
                    &[],
                    &[],
                    "",
                    None,
                    &[],
                    &ctx,
                    at,
                )
                .unwrap();
        }
        assert_eq!(all_activity(&ledger).len(), 1);
    }

    #[test]
    fn attention_changes_remain_explainable_after_done() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let mentions = [reviewq_core::model::Mention {
            by: "alice".into(),
            at: now(),
        }];
        let ctx = ClassifyCtx {
            mentions: &mentions,
            review_request: Some(reviewq_core::model::ReviewRequest {
                team: None,
                requested_by: None,
                requested_at: Some(now()),
            }),
            ..Default::default()
        };
        for _ in 0..2 {
            let _ = ledger
                .commit_detail_with_activity(
                    repo_id,
                    &pr(1),
                    &MyState::default(),
                    &[],
                    &[],
                    "",
                    None,
                    &[],
                    &ctx,
                    now(),
                )
                .unwrap();
        }
        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1, "one observation, not one per sync");
        let payload = serde_json::to_value(&events[0].payload).unwrap();
        assert_eq!(
            payload["attention_changed"]["after"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let done = local_activity(ActivityKind::Done, "2026-08-06T12:00:00Z");
        ledger
            .record_done_action(repo_id, 1, "head", done.occurred_at, &done)
            .unwrap();
        let events = all_activity(&ledger);
        let change = events
            .iter()
            .find(|event| event.occurred_at == done.occurred_at && event.kind != ActivityKind::Done)
            .unwrap();
        let payload = serde_json::to_value(&change.payload).unwrap();
        assert_eq!(
            payload["attention_changed"]["before"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            payload["attention_changed"]["after"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(ledger.show(repo_id, 1).unwrap().unwrap().attention.len(), 1);
    }

    #[test]
    fn failed_attention_history_write_rolls_back_the_detail_observation() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_attention_history BEFORE INSERT ON activity_events
             WHEN NEW.kind = 'attention_changed'
             BEGIN SELECT RAISE(ABORT, 'rejected history'); END;",
            )
            .unwrap();
        let ctx = ClassifyCtx {
            review_request: Some(reviewq_core::model::ReviewRequest {
                team: None,
                requested_by: None,
                requested_at: Some(now()),
            }),
            ..Default::default()
        };
        assert!(
            ledger
                .commit_detail_with_activity(
                    repo_id,
                    &pr(1),
                    &MyState::default(),
                    &[],
                    &[],
                    "new body",
                    None,
                    &[],
                    &ctx,
                    now()
                )
                .is_err()
        );
        assert!(all_activity(&ledger).is_empty());
        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .attention
                .is_empty()
        );
        let watermark = prs::table
            .find((repo_id, 1_i64))
            .select(prs::detail_synced_at)
            .first::<Option<String>>(&mut *ledger.conn.borrow_mut())
            .unwrap();
        assert_eq!(watermark, None);
    }

    #[test]
    fn explicitly_untracking_an_unmatched_pr_prevents_later_rule_tracking() {
        let (ledger, repo_id) = ledger_with_repo();
        let snapshot = pr(1);
        ledger.upsert_pr(repo_id, &snapshot, None).unwrap();
        let event = local_activity(ActivityKind::Untracked, "2026-08-05T13:00:00Z");

        assert!(
            ledger
                .record_untrack_action(repo_id, 1, event.occurred_at, &event)
                .unwrap()
        );
        assert!(
            ledger
                .record_untrack_action(repo_id, 1, event.occurred_at, &event)
                .unwrap()
        );
        track(&ledger, repo_id, &snapshot);

        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .tracked_reason
                .is_none()
        );
        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ActivityKind::Untracked);
    }

    fn local_activity(kind: ActivityKind, occurred_at: &str) -> NewActivityEvent {
        NewActivityEvent {
            relation: reviewq_core::model::ActivityRelation::Own,
            source: ActivitySource::Local,
            kind,
            occurred_at: ts(occurred_at),
            recorded_at: now(),
            actor: None,
            head_sha: None,
            external_id: None,
            permalink: None,
            payload: ActivityPayload::None,
        }
    }

    fn forge_review(id: &str, result: ReviewResult) -> NewActivityEvent {
        NewActivityEvent {
            relation: reviewq_core::model::ActivityRelation::Own,
            source: ActivitySource::Forge,
            kind: ActivityKind::ReviewSubmitted,
            occurred_at: ts("2026-08-04T12:00:00Z"),
            recorded_at: now(),
            actor: Some("octocat".into()),
            head_sha: Some("reviewed-sha".into()),
            external_id: Some(id.into()),
            permalink: Some("https://forge.example/reviews/42".into()),
            payload: ActivityPayload::ReviewSubmitted {
                result,
                reviewed_sha: Some("reviewed-sha".into()),
            },
        }
    }

    #[test]
    fn a_merge_suppresses_its_companion_close_in_either_arrival_order() {
        for merge_first in [false, true] {
            let (ledger, repo_id) = ledger_with_pr(1);
            let earlier =
                forge_lifecycle(ActivityKind::PrClosed, "earlier", "2026-08-04T12:00:00Z");
            ledger.record_forge_activity(repo_id, 1, &earlier).unwrap();
            let close = forge_lifecycle(ActivityKind::PrClosed, "close", "2026-08-05T12:00:00Z");
            let merge = forge_lifecycle(ActivityKind::PrMerged, "merge", "2026-08-05T12:00:00Z");
            ledger
                .record_state_transition(
                    repo_id,
                    1,
                    PrState::Closed,
                    Some(close.occurred_at),
                    now(),
                )
                .unwrap();
            let events = if merge_first {
                [&merge, &close]
            } else {
                [&close, &merge]
            };
            for event in events {
                ledger.record_forge_activity(repo_id, 1, event).unwrap();
            }
            let history = all_activity(&ledger);
            assert_eq!(
                history.iter().map(|event| event.kind).collect::<Vec<_>>(),
                [ActivityKind::PrMerged, ActivityKind::PrClosed]
            );
            assert_eq!(history[1].external_id.as_deref(), Some("earlier"));
            assert_eq!(
                ledger.show(repo_id, 1).unwrap().unwrap().pr.state,
                PrState::Merged
            );
        }
    }

    fn forge_lifecycle(kind: ActivityKind, id: &str, occurred_at: &str) -> NewActivityEvent {
        NewActivityEvent {
            relation: reviewq_core::model::ActivityRelation::Own,
            source: ActivitySource::Forge,
            kind,
            occurred_at: ts(occurred_at),
            recorded_at: now(),
            actor: Some("octocat".into()),
            head_sha: None,
            external_id: Some(id.into()),
            permalink: Some("https://forge.example/pulls/1".into()),
            payload: ActivityPayload::None,
        }
    }

    fn all_activity(ledger: &Ledger) -> Vec<ActivityEvent> {
        ledger
            .activity_page(ActivityScope::All, None, 100)
            .unwrap()
            .events
    }

    fn ledger_with_retained_observed_close() -> (Ledger, RepoId) {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        ledger
            .record_state_transition(
                repo_id,
                1,
                PrState::Closed,
                None,
                ts("2026-08-20T00:00:00Z"),
            )
            .unwrap();
        ledger.clean_activity(ts("2026-08-10T00:00:00Z")).unwrap();
        (ledger, repo_id)
    }

    #[test]
    fn rejected_local_action_event_rolls_back_the_state_change() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_local_activity
                 BEFORE INSERT ON activity_events
                 BEGIN
                   SELECT RAISE(ABORT, 'reject local activity');
                 END;",
            )
            .unwrap();

        assert!(
            ledger
                .record_done_action(
                    repo_id,
                    1,
                    "new-head",
                    now(),
                    &local_activity(ActivityKind::Done, "2026-08-04T12:00:00Z"),
                )
                .is_err()
        );

        assert_eq!(ledger.my_state(repo_id, 1).unwrap().done_sha, None);
        assert!(all_activity(&ledger).is_empty());
    }

    #[test]
    fn rejected_fetched_track_event_does_not_store_the_pr() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_fetched_track_activity
                 BEFORE INSERT ON activity_events
                 BEGIN
                   SELECT RAISE(ABORT, 'reject fetched track activity');
                 END;",
            )
            .unwrap();

        assert!(
            ledger
                .record_fetched_track_action(
                    repo_id,
                    &pr(1),
                    now(),
                    &local_activity(ActivityKind::Tracked, "2026-08-04T12:00:00Z"),
                )
                .is_err()
        );

        assert!(ledger.show(repo_id, 1).unwrap().is_none());
        assert!(all_activity(&ledger).is_empty());
    }

    #[test]
    fn local_activity_remains_readable_after_untracking_and_restricts_pr_deletion() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let event = local_activity(ActivityKind::Done, "2026-08-04T12:00:00Z");

        ledger.record_activity(repo_id, 1, &event).unwrap();
        assert!(ledger.untrack(repo_id, 1, now()).unwrap());

        let events = ledger
            .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 10)
            .unwrap()
            .events;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ActivityKind::Done);
        assert_eq!(events[0].pr_title, "PR 1");
        assert!(
            diesel::delete(prs::table.find((repo_id, 1_i64)))
                .execute(&mut *ledger.conn.borrow_mut())
                .is_err()
        );
    }

    #[test]
    fn forge_activity_is_idempotent_by_pr_kind_and_external_id() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let event = forge_review("review-42", ReviewResult::Approved);

        assert!(ledger.record_forge_activity(repo_id, 1, &event).unwrap());
        assert!(!ledger.record_forge_activity(repo_id, 1, &event).unwrap());
        assert_eq!(all_activity(&ledger).len(), 1);
    }

    #[test]
    fn forge_activity_preserves_provider_specific_review_results() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let event = forge_review(
            "review-other",
            ReviewResult::Other("needs-security-signoff".into()),
        );

        assert!(ledger.record_forge_activity(repo_id, 1, &event).unwrap());

        let stored = all_activity(&ledger).pop().unwrap();
        assert_eq!(stored.head_sha.as_deref(), Some("reviewed-sha"));
        assert_eq!(
            stored.payload,
            ActivityPayload::ReviewSubmitted {
                result: ReviewResult::Other("needs-security-signoff".into()),
                reviewed_sha: Some("reviewed-sha".into()),
            }
        );
    }

    #[test]
    fn activity_page_and_backfill_cursor_commit_atomically() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let first = forge_review("review-first", ReviewResult::Approved);

        assert_eq!(
            ledger
                .commit_activity_page(
                    repo_id,
                    1,
                    None,
                    &[first],
                    Some("provider / cursor"),
                    Some(ActivityRateLimitUnit::Requests),
                    now(),
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 1 }
        );
        assert_eq!(
            ledger.activity_backfill(repo_id, 1).unwrap(),
            Some(ActivityBackfill {
                cursor: Some("provider / cursor".into()),
                next_rate_limit: Some(ActivityRateLimitUnit::Requests),
                completed_at: None,
            })
        );

        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_backfill_event
                 BEFORE INSERT ON activity_events
                 WHEN NEW.external_id = 'review-second'
                 BEGIN
                   SELECT RAISE(ABORT, 'reject backfill event');
                 END;",
            )
            .unwrap();
        let second = forge_review("review-second", ReviewResult::ChangesRequested);

        assert!(
            ledger
                .commit_activity_page(
                    repo_id,
                    1,
                    Some("provider / cursor"),
                    &[second],
                    None,
                    None,
                    now(),
                )
                .is_err()
        );
        assert_eq!(
            all_activity(&ledger).len(),
            1,
            "the event insert rolled back"
        );
        assert_eq!(
            ledger
                .activity_backfill(repo_id, 1)
                .unwrap()
                .unwrap()
                .cursor,
            Some("provider / cursor".into()),
            "the prior checkpoint survived"
        );
    }

    #[test]
    fn stale_backfill_writer_cannot_overwrite_a_newer_or_completed_checkpoint() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let newer = forge_review("review-newer", ReviewResult::Approved);

        assert_eq!(
            ledger
                .commit_activity_page(
                    repo_id,
                    1,
                    None,
                    &[newer],
                    Some("newer cursor"),
                    Some(ActivityRateLimitUnit::Requests),
                    now(),
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 1 }
        );

        let stale = forge_review("review-stale", ReviewResult::ChangesRequested);
        assert_eq!(
            ledger
                .commit_activity_page(
                    repo_id,
                    1,
                    None,
                    &[stale],
                    Some("stale cursor"),
                    Some(ActivityRateLimitUnit::Points),
                    now(),
                )
                .unwrap(),
            ActivityPageCommit::Superseded
        );
        assert_eq!(
            ledger.activity_backfill(repo_id, 1).unwrap().unwrap(),
            ActivityBackfill {
                cursor: Some("newer cursor".into()),
                next_rate_limit: Some(ActivityRateLimitUnit::Requests),
                completed_at: None,
            }
        );
        assert_eq!(all_activity(&ledger).len(), 1);

        assert_eq!(
            ledger
                .commit_activity_page(repo_id, 1, Some("newer cursor"), &[], None, None, now(),)
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        assert_eq!(
            ledger
                .commit_activity_page(
                    repo_id,
                    1,
                    Some("newer cursor"),
                    &[],
                    Some("regressed cursor"),
                    Some(ActivityRateLimitUnit::Points),
                    now(),
                )
                .unwrap(),
            ActivityPageCommit::Superseded
        );
        let completed = ledger.activity_backfill(repo_id, 1).unwrap().unwrap();
        assert_eq!(completed.cursor, None);
        assert_eq!(completed.completed_at, Some(now()));
    }

    #[test]
    fn finishing_incremental_activity_leaves_a_newer_refresh_pending() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .commit_activity_page(repo_id, 1, None, &[], None, None, now())
            .unwrap();
        request_activity_refresh(&mut ledger.conn.borrow_mut(), repo_id, 1).unwrap();
        let first = ledger
            .begin_incremental_activity(repo_id, 1, now())
            .unwrap();
        request_activity_refresh(&mut ledger.conn.borrow_mut(), repo_id, 1).unwrap();

        assert_eq!(
            ledger
                .commit_incremental_activity_page(repo_id, 1, &first, &[], None, None)
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );

        assert_eq!(ledger.activity_refresh_candidates(repo_id).unwrap(), [1]);
        let next = ledger
            .begin_incremental_activity(repo_id, 1, now())
            .unwrap();
        assert_eq!(
            ledger
                .commit_incremental_activity_page(repo_id, 1, &next, &[], None, None)
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        assert!(
            ledger
                .activity_refresh_candidates(repo_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_completed_traversals_response_cannot_write_into_a_new_traversal() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .commit_activity_page(repo_id, 1, None, &[], None, None, now())
            .unwrap();
        let first = ledger
            .begin_incremental_activity(repo_id, 1, now())
            .unwrap();
        assert_eq!(
            ledger
                .commit_incremental_activity_page(repo_id, 1, &first, &[], None, None)
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        let next = ledger
            .begin_incremental_activity(repo_id, 1, now())
            .unwrap();
        let stale = forge_review("stale", ReviewResult::Approved);

        assert_eq!(
            ledger
                .commit_incremental_activity_page(repo_id, 1, &first, &[stale], None, None)
                .unwrap(),
            ActivityPageCommit::Superseded
        );

        assert_eq!(
            ledger.activity_incremental(repo_id, 1).unwrap().unwrap(),
            next
        );
        assert!(all_activity(&ledger).is_empty());
    }

    #[test]
    fn completing_backfill_keeps_refreshes_requested_during_its_traversal() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .commit_activity_page(repo_id, 1, None, &[], Some("last page"), None, now())
            .unwrap();
        request_activity_refresh(&mut ledger.conn.borrow_mut(), repo_id, 1).unwrap();

        ledger
            .commit_activity_page(repo_id, 1, Some("last page"), &[], None, None, now())
            .unwrap();

        assert_eq!(ledger.activity_refresh_candidates(repo_id).unwrap(), [1]);
        let checkpoint = ledger
            .begin_incremental_activity(repo_id, 1, now())
            .unwrap();
        assert_eq!(
            ledger
                .commit_incremental_activity_page(repo_id, 1, &checkpoint, &[], None, None)
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        assert!(
            ledger
                .activity_refresh_candidates(repo_id)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn incremental_activity_page_and_resume_cursor_commit_atomically() {
        let (ledger, repo_id) = ledger_with_pr(1);
        assert_eq!(
            ledger
                .commit_activity_page(repo_id, 1, None, &[], None, None, now())
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        let first = forge_review("incremental-first", ReviewResult::Approved);
        assert_eq!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    1,
                    &ledger
                        .begin_incremental_activity(repo_id, 1, now())
                        .unwrap(),
                    &[first],
                    Some("incremental cursor"),
                    Some(ActivityRateLimitUnit::Requests)
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 1 }
        );
        assert_eq!(
            ledger.activity_incremental(repo_id, 1).unwrap(),
            Some(ActivityIncremental {
                started_at: Some(now()),
                revision: 2,
                generation: Some(0),
                stop_at: Some(ts("2026-08-05T11:55:00Z")),
                cursor: Some("incremental cursor".into()),
                next_rate_limit: Some(ActivityRateLimitUnit::Requests),
            })
        );

        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_incremental_event
                 BEFORE INSERT ON activity_events
                 WHEN NEW.external_id = 'incremental-second'
                 BEGIN
                   SELECT RAISE(ABORT, 'reject incremental event');
                 END;",
            )
            .unwrap();
        let second = forge_review("incremental-second", ReviewResult::ChangesRequested);

        assert!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    1,
                    &ledger
                        .begin_incremental_activity(repo_id, 1, now())
                        .unwrap(),
                    &[second],
                    None,
                    None
                )
                .is_err()
        );
        assert_eq!(all_activity(&ledger).len(), 1);
        assert_eq!(
            ledger
                .activity_incremental(repo_id, 1)
                .unwrap()
                .unwrap()
                .cursor
                .as_deref(),
            Some("incremental cursor")
        );
    }

    #[test]
    fn stale_incremental_writer_cannot_replace_a_newer_cursor() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .commit_activity_page(repo_id, 1, None, &[], None, None, now())
            .unwrap();
        let expected = ledger
            .begin_incremental_activity(repo_id, 1, now())
            .unwrap();
        ledger
            .commit_incremental_activity_page(
                repo_id,
                1,
                &expected,
                &[],
                Some("newer incremental cursor"),
                Some(ActivityRateLimitUnit::Requests),
            )
            .unwrap();

        let stale = forge_review("stale-incremental", ReviewResult::Approved);
        assert_eq!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    1,
                    &expected,
                    &[stale],
                    Some("stale incremental cursor"),
                    Some(ActivityRateLimitUnit::Points)
                )
                .unwrap(),
            ActivityPageCommit::Superseded
        );
        assert!(all_activity(&ledger).is_empty());
        assert_eq!(
            ledger
                .activity_incremental(repo_id, 1)
                .unwrap()
                .unwrap()
                .cursor
                .as_deref(),
            Some("newer incremental cursor")
        );
    }

    #[test]
    fn a_finished_backfill_stays_complete_after_untracking() {
        let (ledger, repo_id) = ledger_with_pr(1);

        ledger
            .commit_activity_page(repo_id, 1, None, &[], None, None, now())
            .unwrap();
        ledger.untrack(repo_id, 1, now()).unwrap();

        assert_eq!(
            ledger.activity_backfill(repo_id, 1).unwrap(),
            Some(ActivityBackfill {
                cursor: None,
                next_rate_limit: None,
                completed_at: Some(now()),
            })
        );
    }

    #[test]
    fn state_transitions_update_the_projection_and_history_as_one_step() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        let observed_close = ts("2026-08-05T13:00:00Z");
        let authoritative_close = ts("2026-08-05T12:45:00Z");

        assert!(
            ledger
                .record_state_transition(repo_id, 1, PrState::Closed, None, observed_close)
                .unwrap()
        );
        assert!(
            !ledger
                .record_state_transition(
                    repo_id,
                    1,
                    PrState::Closed,
                    None,
                    ts("2026-08-05T14:00:00Z")
                )
                .unwrap()
        );
        assert!(
            !ledger
                .record_state_transition(
                    repo_id,
                    1,
                    PrState::Closed,
                    Some(authoritative_close),
                    ts("2026-08-05T14:00:00Z"),
                )
                .unwrap()
        );
        assert!(
            ledger
                .record_state_transition(
                    repo_id,
                    1,
                    PrState::Open,
                    Some(ts("2026-08-05T15:00:00Z")),
                    ts("2026-08-05T15:01:00Z"),
                )
                .unwrap()
        );
        assert!(
            ledger
                .record_state_transition(
                    repo_id,
                    1,
                    PrState::Merged,
                    Some(ts("2026-08-05T16:00:00Z")),
                    ts("2026-08-05T16:01:00Z"),
                )
                .unwrap()
        );

        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(show.pr.state, PrState::Merged);
        assert_eq!(show.pr.state_changed_at, Some(ts("2026-08-05T16:00:00Z")));
        let events = all_activity(&ledger);
        assert_eq!(
            events
                .iter()
                .map(|event| (event.kind, event.occurred_at, event.payload.clone()))
                .collect::<Vec<_>>(),
            vec![
                (
                    ActivityKind::PrMerged,
                    ts("2026-08-05T16:00:00Z"),
                    ActivityPayload::StateChanged {
                        from: PrState::Open,
                        to: PrState::Merged,
                    },
                ),
                (
                    ActivityKind::PrReopened,
                    ts("2026-08-05T15:00:00Z"),
                    ActivityPayload::StateChanged {
                        from: PrState::Closed,
                        to: PrState::Open,
                    },
                ),
                (
                    ActivityKind::PrClosed,
                    authoritative_close,
                    ActivityPayload::StateChanged {
                        from: PrState::Open,
                        to: PrState::Closed,
                    },
                ),
            ]
        );
    }

    #[test]
    fn rejected_state_transition_rolls_back_the_projection() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_lifecycle_activity
                 BEFORE INSERT ON activity_events
                 WHEN NEW.kind = 'pr_closed'
                 BEGIN
                   SELECT RAISE(ABORT, 'reject lifecycle activity');
                 END;",
            )
            .unwrap();

        assert!(
            ledger
                .record_state_transition(repo_id, 1, PrState::Closed, None, now())
                .is_err()
        );

        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(show.pr.state, PrState::Open);
        assert_eq!(show.pr.state_changed_at, None);
        assert!(all_activity(&ledger).is_empty());
    }

    #[test]
    fn historical_lifecycle_pages_do_not_roll_the_current_state_backward() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .record_state_transition(
                repo_id,
                1,
                PrState::Merged,
                Some(ts("2026-08-05T16:00:00Z")),
                now(),
            )
            .unwrap();

        ledger
            .commit_activity_page(
                repo_id,
                1,
                None,
                &[forge_lifecycle(
                    ActivityKind::PrClosed,
                    "historical-close",
                    "2026-08-04T12:00:00Z",
                )],
                None,
                None,
                now(),
            )
            .unwrap();

        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(show.pr.state, PrState::Merged);
        assert_eq!(show.pr.state_changed_at, Some(ts("2026-08-05T16:00:00Z")));
    }

    #[test]
    fn authoritative_lifecycle_history_replaces_the_observed_step() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        ledger
            .record_state_transition(
                repo_id,
                1,
                PrState::Closed,
                None,
                ts("2026-08-05T13:00:00Z"),
            )
            .unwrap();

        ledger
            .commit_activity_page(
                repo_id,
                1,
                None,
                &[forge_lifecycle(
                    ActivityKind::PrClosed,
                    "provider-close",
                    "2026-08-05T12:45:00Z",
                )],
                None,
                None,
                now(),
            )
            .unwrap();

        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].external_id.as_deref(), Some("provider-close"));
        assert_eq!(events[0].occurred_at, ts("2026-08-05T12:45:00Z"));
        assert_eq!(
            events[0].payload,
            ActivityPayload::StateChanged {
                from: PrState::Open,
                to: PrState::Closed,
            }
        );
        assert_eq!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .pr
                .state_changed_at,
            Some(ts("2026-08-05T12:45:00Z"))
        );
    }

    #[test]
    fn authoritative_close_reopen_close_history_reconciles_each_observed_transition() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        for (state, observed_at) in [
            (PrState::Closed, "2026-08-05T13:05:00Z"),
            (PrState::Open, "2026-08-05T14:05:00Z"),
            (PrState::Closed, "2026-08-05T15:05:00Z"),
        ] {
            ledger
                .record_state_transition(repo_id, 1, state, None, ts(observed_at))
                .unwrap();
        }

        let history = [
            forge_lifecycle(
                ActivityKind::PrClosed,
                "provider-close-latest",
                "2026-08-05T15:00:00Z",
            ),
            forge_lifecycle(
                ActivityKind::PrReopened,
                "provider-reopen",
                "2026-08-05T14:00:00Z",
            ),
            forge_lifecycle(
                ActivityKind::PrClosed,
                "provider-close-first",
                "2026-08-05T13:00:00Z",
            ),
        ];
        ledger
            .commit_activity_page(repo_id, 1, None, &history, None, None, now())
            .unwrap();

        let events = all_activity(&ledger);
        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .map(|event| (event.kind, event.occurred_at, event.external_id.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                (
                    ActivityKind::PrClosed,
                    ts("2026-08-05T15:00:00Z"),
                    Some("provider-close-latest"),
                ),
                (
                    ActivityKind::PrReopened,
                    ts("2026-08-05T14:00:00Z"),
                    Some("provider-reopen"),
                ),
                (
                    ActivityKind::PrClosed,
                    ts("2026-08-05T13:00:00Z"),
                    Some("provider-close-first"),
                ),
            ]
        );
        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(show.pr.state, PrState::Closed);
        assert_eq!(show.pr.state_changed_at, Some(ts("2026-08-05T15:00:00Z")));
    }

    #[test]
    fn lifecycle_events_without_external_ids_are_idempotent_across_incremental_syncs() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .commit_activity_page(repo_id, 1, None, &[], None, None, now())
            .unwrap();
        let mut closed = forge_lifecycle(
            ActivityKind::PrClosed,
            "discarded-close-id",
            "2026-08-05T13:00:00Z",
        );
        closed.external_id = None;
        let mut reopened = forge_lifecycle(
            ActivityKind::PrReopened,
            "discarded-reopen-id",
            "2026-08-05T14:00:00Z",
        );
        reopened.external_id = None;
        let history = [reopened, closed];

        assert_eq!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    1,
                    &ledger
                        .begin_incremental_activity(repo_id, 1, now())
                        .unwrap(),
                    &history,
                    None,
                    None
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 2 }
        );
        assert_eq!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    1,
                    &ledger
                        .begin_incremental_activity(repo_id, 1, now())
                        .unwrap(),
                    &history,
                    None,
                    None
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );

        assert_eq!(all_activity(&ledger).len(), 2);
    }

    #[test]
    fn sweep_pages_record_authoritative_state_transitions_without_repeating_them() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        let mut closed = pr(1);
        closed.state = PrState::Closed;
        closed.updated_at = ts("2026-08-05T13:00:00Z");
        closed.state_changed_at = Some(ts("2026-08-05T13:00:00Z"));

        ledger
            .commit_sweep_page(repo_id, &[(closed.clone(), None)], "cursor", "closed")
            .unwrap();
        ledger
            .commit_sweep_page(repo_id, &[(closed, None)], "cursor", "same")
            .unwrap();
        let mut reopened = pr(1);
        reopened.updated_at = ts("2026-08-05T15:00:00Z");
        reopened.state_changed_at = Some(ts("2026-08-05T15:00:00Z"));
        ledger
            .commit_sweep_page(repo_id, &[(reopened, None)], "cursor", "reopened")
            .unwrap();

        assert_eq!(
            all_activity(&ledger)
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![ActivityKind::PrReopened, ActivityKind::PrClosed]
        );
        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(show.pr.state, PrState::Open);
        assert_eq!(show.pr.state_changed_at, Some(ts("2026-08-05T15:00:00Z")));
    }

    #[test]
    fn older_and_equal_overlapping_sweeps_cannot_reopen_a_newer_closed_projection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let first = Ledger::open(&path).unwrap();
        let repo_id = first.ensure_repo(&repo()).unwrap();
        track(&first, repo_id, &pr(1));
        first
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        let second = Ledger::open(&path).unwrap();

        let mut closed = pr(1);
        closed.title = "new closed title".into();
        closed.head_sha = "new-closed-head".into();
        closed.state = PrState::Closed;
        closed.updated_at = ts("2026-08-05T14:00:00Z");
        closed.state_changed_at = Some(ts("2026-08-05T13:45:00Z"));
        second
            .commit_sweep_page(repo_id, &[(closed, None)], "cursor", "new")
            .unwrap();

        for (updated_at, title) in [
            ("2026-08-05T13:00:00Z", "stale open title"),
            ("2026-08-05T14:00:00Z", "equal open title"),
        ] {
            let mut open = pr(1);
            open.title = title.into();
            open.head_sha = "stale-open-head".into();
            open.updated_at = ts(updated_at);
            open.state_changed_at = Some(ts("2026-08-05T13:55:00Z"));
            first
                .commit_sweep_page(repo_id, &[(open, None)], "cursor", title)
                .unwrap();
        }

        let shown = first.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(shown.pr.state, PrState::Closed);
        assert_eq!(shown.pr.updated_at, ts("2026-08-05T14:00:00Z"));
        assert_eq!(shown.pr.title, "new closed title");
        assert_eq!(shown.pr.head_sha, "new-closed-head");
        assert_eq!(shown.pr.state_changed_at, Some(ts("2026-08-05T13:45:00Z")));
        let events = all_activity(&first);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ActivityKind::PrClosed);
        assert_eq!(
            events[0].payload,
            ActivityPayload::StateChanged {
                from: PrState::Open,
                to: PrState::Closed,
            }
        );
    }

    #[test]
    fn activity_page_reports_unknown_stored_source_as_corruption() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Done, "2026-08-04T12:00:00Z"),
            )
            .unwrap();
        diesel::update(activity_events::table)
            .set(activity_events::source.eq("unknown"))
            .execute(&mut *ledger.conn.borrow_mut())
            .unwrap();

        let error = ledger
            .activity_page(ActivityScope::All, None, 10)
            .unwrap_err();

        assert!(matches!(error, LedgerError::Corrupt { what, .. } if what == "activity source"));
    }

    #[test]
    fn activity_page_reports_malformed_stored_payload_as_corruption() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Done, "2026-08-04T12:00:00Z"),
            )
            .unwrap();
        diesel::update(activity_events::table)
            .set(activity_events::payload.eq("{not json}"))
            .execute(&mut *ledger.conn.borrow_mut())
            .unwrap();

        let error = ledger
            .activity_page(ActivityScope::All, None, 10)
            .unwrap_err();

        assert!(matches!(error, LedgerError::Corrupt { what, .. } if what == "activity payload"));
    }

    #[test]
    fn activity_pages_hold_a_stable_global_and_per_pr_traversal() {
        let (ledger, repo_id) = ledger_with_pr(1);
        track(&ledger, repo_id, &pr(2));
        for (number, actor, occurred_at) in [
            (1, "old", "2026-08-04T08:00:00Z"),
            (2, "second", "2026-08-04T09:00:00Z"),
            (1, "same-earlier", "2026-08-04T10:00:00Z"),
            (2, "same-later", "2026-08-04T10:00:00Z"),
        ] {
            let mut event = local_activity(ActivityKind::Done, occurred_at);
            event.actor = Some(actor.into());
            ledger.record_activity(repo_id, number, &event).unwrap();
        }

        let first = ledger.activity_page(ActivityScope::All, None, 2).unwrap();
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.actor.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("same-later"), Some("same-earlier")]
        );
        let cursor = first.next.clone().unwrap();

        let mut newest = local_activity(ActivityKind::Done, "2026-08-04T11:00:00Z");
        newest.actor = Some("newest".into());
        ledger.record_activity(repo_id, 1, &newest).unwrap();

        let second = ledger
            .activity_page(ActivityScope::All, Some(&cursor), 2)
            .unwrap();
        assert_eq!(
            second
                .events
                .iter()
                .map(|event| event.actor.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("second"), Some("old")]
        );
        assert!(second.next.is_none());

        assert_eq!(
            ledger
                .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 10)
                .unwrap()
                .events
                .iter()
                .map(|event| event.actor.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("newest"), Some("same-earlier"), Some("old")]
        );
    }

    #[test]
    fn activity_pages_order_fractional_second_timestamps_chronologically() {
        let (ledger, repo_id) = ledger_with_pr(1);
        for (actor, occurred_at) in [
            ("whole-second", "2026-08-04T10:00:00Z"),
            ("half-second", "2026-08-04T10:00:00.5Z"),
        ] {
            let mut event = local_activity(ActivityKind::Done, occurred_at);
            event.actor = Some(actor.into());
            ledger.record_activity(repo_id, 1, &event).unwrap();
        }

        assert_eq!(
            all_activity(&ledger)
                .iter()
                .map(|event| event.actor.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("half-second"), Some("whole-second")]
        );
    }

    #[test]
    fn per_pr_activity_reads_use_the_per_pr_time_index() {
        let (ledger, repo_id) = ledger_with_pr(1);
        #[derive(QueryableByName)]
        struct QueryPlan {
            #[diesel(sql_type = diesel::sql_types::Text)]
            detail: String,
        }

        let query = activity_page_query(ActivityScope::Pr { repo_id, number: 1 }, None, 6);
        let explain = format!(
            "EXPLAIN QUERY PLAN {}",
            diesel::debug_query::<diesel::sqlite::Sqlite, _>(&query)
        );
        let details = diesel::sql_query(explain)
            .bind::<diesel::sql_types::BigInt, _>(repo_id)
            .bind::<diesel::sql_types::BigInt, _>(1_i64)
            .bind::<diesel::sql_types::BigInt, _>(6_i64)
            .load::<QueryPlan>(&mut *ledger.conn.borrow_mut())
            .unwrap()
            .into_iter()
            .map(|row| row.detail)
            .collect::<Vec<_>>();

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("activity_events_by_pr_time")),
            "expected the per-PR index, got {details:?}"
        );
    }

    #[test]
    fn cleanup_previews_and_deletes_only_events_strictly_before_the_cutoff() {
        let (ledger, repo_id) = ledger_with_pr(1);
        track(&ledger, repo_id, &pr(2));
        for (number, actor, occurred_at) in [
            (1, "old-one", "2025-08-19T23:59:59Z"),
            (2, "old-two", "2025-08-01T12:00:00Z"),
            (1, "cutoff", "2025-08-20T00:00:00Z"),
            (2, "new", "2026-08-20T00:00:00Z"),
        ] {
            let mut event = local_activity(ActivityKind::Done, occurred_at);
            event.actor = Some(actor.into());
            ledger.record_activity(repo_id, number, &event).unwrap();
        }
        let cutoff = ts("2025-08-20T00:00:00Z");

        assert_eq!(
            ledger.preview_activity_cleanup(cutoff).unwrap(),
            CleanupPreview {
                event_count: 2,
                pr_count: 2,
            }
        );
        assert_eq!(
            ledger.clean_activity(cutoff).unwrap(),
            CleanupPreview {
                event_count: 2,
                pr_count: 2,
            }
        );
        assert_eq!(
            all_activity(&ledger)
                .iter()
                .map(|event| event.actor.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("new"), Some("cutoff")]
        );
    }

    #[test]
    fn cleanup_boundary_rejects_backfill_incremental_and_local_events_strictly_before_it() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let cutoff = ts("2026-08-20T00:00:00Z");
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Done, "2025-08-01T12:00:00Z"),
            )
            .unwrap();
        ledger.clean_activity(cutoff).unwrap();

        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Muted, "2026-08-19T23:59:59Z"),
            )
            .unwrap();
        let backfill_old = NewActivityEvent {
            relation: reviewq_core::model::ActivityRelation::Own,
            kind: ActivityKind::Commented,
            payload: ActivityPayload::None,
            ..forge_review("backfill-old", ReviewResult::Approved)
        };
        assert_eq!(
            ledger
                .commit_activity_page(repo_id, 1, None, &[backfill_old], None, None, now())
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        let incremental_old = NewActivityEvent {
            relation: reviewq_core::model::ActivityRelation::Own,
            kind: ActivityKind::Commented,
            payload: ActivityPayload::None,
            ..forge_review("incremental-old", ReviewResult::Commented)
        };
        assert_eq!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    1,
                    &ledger
                        .begin_incremental_activity(repo_id, 1, now())
                        .unwrap(),
                    &[incremental_old],
                    None,
                    None
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Done, "2026-08-20T00:00:00Z"),
            )
            .unwrap();

        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].occurred_at, cutoff);
        assert_eq!(events[0].kind, ActivityKind::Done);
    }

    #[test]
    fn concurrent_cleanup_serializes_with_standalone_local_activity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let cleaner = Ledger::open(&path).unwrap();
        let repo_id = cleaner.ensure_repo(&repo()).unwrap();
        track(&cleaner, repo_id, &pr(1));
        let recorder = Ledger::open(&path).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        recorder
            .conn
            .borrow_mut()
            .set_instrumentation(move |event: InstrumentationEvent<'_>| {
                if matches!(event, InstrumentationEvent::BeginTransaction { .. }) {
                    started_tx.send(()).unwrap();
                }
            });
        let cutoff = activity_timestamp_to_wire(ts("2026-08-10T00:00:00Z"));
        let (finished_tx, finished_rx) = mpsc::channel();

        std::thread::scope(|scope| {
            let recording = cleaner
                .conn
                .borrow_mut()
                .immediate_transaction::<_, LedgerError, _>(|conn| {
                    diesel::delete(
                        activity_events::table.filter(activity_events::occurred_at.lt(&cutoff)),
                    )
                    .execute(conn)?;
                    diesel::insert_into(activity_retention::table)
                        .values((
                            activity_retention::singleton.eq(1),
                            activity_retention::cutoff.eq(&cutoff),
                        ))
                        .execute(conn)?;
                    let recording = scope.spawn(move || {
                        let result = recorder.record_activity(
                            repo_id,
                            1,
                            &local_activity(ActivityKind::ReviewStarted, "2026-08-01T00:00:00Z"),
                        );
                        finished_tx.send(()).unwrap();
                        result
                    });
                    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    assert!(matches!(
                        finished_rx.recv_timeout(Duration::from_millis(100)),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ));
                    Ok(recording)
                })
                .unwrap();
            recording.join().unwrap().unwrap();
        });

        assert!(all_activity(&cleaner).is_empty());
    }

    #[test]
    fn cleanup_boundary_drops_an_old_lifecycle_event_while_retaining_its_current_projection() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger.clean_activity(ts("2026-08-20T00:00:00Z")).unwrap();

        assert!(
            !ledger
                .record_state_transition(
                    repo_id,
                    1,
                    PrState::Closed,
                    Some(ts("2026-08-19T12:00:00Z")),
                    ts("2026-08-21T12:00:00Z"),
                )
                .unwrap()
        );

        assert!(all_activity(&ledger).is_empty());
        let shown = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(shown.pr.state, PrState::Closed);
        assert_eq!(shown.pr.state_changed_at, Some(ts("2026-08-19T12:00:00Z")));
    }

    #[test]
    fn lifecycle_correction_before_the_cleanup_boundary_removes_the_observed_event() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        ledger
            .record_state_transition(
                repo_id,
                1,
                PrState::Closed,
                None,
                ts("2026-08-20T00:00:00Z"),
            )
            .unwrap();
        ledger.clean_activity(ts("2026-08-10T00:00:00Z")).unwrap();

        ledger
            .record_state_transition(
                repo_id,
                1,
                PrState::Closed,
                Some(ts("2026-08-01T00:00:00Z")),
                ts("2026-08-21T00:00:00Z"),
            )
            .unwrap();

        assert!(all_activity(&ledger).is_empty());
        let shown = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(shown.pr.state, PrState::Closed);
        assert_eq!(shown.pr.state_changed_at, Some(ts("2026-08-01T00:00:00Z")));
    }

    #[test]
    fn lifecycle_correction_at_or_after_the_cleanup_boundary_retains_the_event() {
        for authoritative in ["2026-08-10T00:00:00Z", "2026-08-11T00:00:00Z"] {
            let (ledger, repo_id) = ledger_with_pr(1);
            ledger
                .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
                .unwrap();
            ledger
                .record_state_transition(
                    repo_id,
                    1,
                    PrState::Closed,
                    None,
                    ts("2026-08-20T00:00:00Z"),
                )
                .unwrap();
            ledger.clean_activity(ts("2026-08-10T00:00:00Z")).unwrap();

            ledger
                .record_state_transition(
                    repo_id,
                    1,
                    PrState::Closed,
                    Some(ts(authoritative)),
                    ts("2026-08-21T00:00:00Z"),
                )
                .unwrap();

            let events = all_activity(&ledger);
            assert_eq!(events.len(), 1, "authoritative {authoritative}");
            assert_eq!(events[0].occurred_at, ts(authoritative));
            let shown = ledger.show(repo_id, 1).unwrap().unwrap();
            assert_eq!(shown.pr.state, PrState::Closed);
            assert_eq!(shown.pr.state_changed_at, Some(ts(authoritative)));
        }
    }

    #[test]
    fn provider_lifecycle_before_cleanup_reconciles_the_observation_then_removes_it() {
        let (ledger, repo_id) = ledger_with_retained_observed_close();

        assert!(
            !ledger
                .record_forge_activity(
                    repo_id,
                    1,
                    &forge_lifecycle(
                        ActivityKind::PrClosed,
                        "provider-close",
                        "2026-08-01T00:00:00Z",
                    ),
                )
                .unwrap()
        );

        assert!(all_activity(&ledger).is_empty());
        let shown = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(shown.pr.state, PrState::Closed);
        assert_eq!(shown.pr.state_changed_at, Some(ts("2026-08-01T00:00:00Z")));
    }

    #[test]
    fn provider_lifecycle_at_cleanup_is_retained_through_backfill() {
        let (ledger, repo_id) = ledger_with_retained_observed_close();

        assert_eq!(
            ledger
                .commit_activity_page(
                    repo_id,
                    1,
                    None,
                    &[forge_lifecycle(
                        ActivityKind::PrClosed,
                        "provider-close",
                        "2026-08-10T00:00:00Z",
                    )],
                    None,
                    None,
                    now(),
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );

        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].external_id.as_deref(), Some("provider-close"));
        assert_eq!(events[0].occurred_at, ts("2026-08-10T00:00:00Z"));
        assert_eq!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .pr
                .state_changed_at,
            Some(ts("2026-08-10T00:00:00Z"))
        );
    }

    #[test]
    fn provider_lifecycle_after_cleanup_is_retained_through_incremental_sync() {
        let (ledger, repo_id) = ledger_with_retained_observed_close();
        ledger
            .commit_activity_page(repo_id, 1, None, &[], None, None, now())
            .unwrap();

        assert_eq!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    1,
                    &ledger
                        .begin_incremental_activity(repo_id, 1, now())
                        .unwrap(),
                    &[forge_lifecycle(
                        ActivityKind::PrClosed,
                        "provider-close",
                        "2026-08-11T00:00:00Z",
                    )],
                    None,
                    None
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );

        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].external_id.as_deref(), Some("provider-close"));
        assert_eq!(events[0].occurred_at, ts("2026-08-11T00:00:00Z"));
        assert_eq!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .pr
                .state_changed_at,
            Some(ts("2026-08-11T00:00:00Z"))
        );
    }

    #[test]
    fn unmatched_provider_lifecycle_before_cleanup_does_not_roll_back_projection() {
        let (ledger, repo_id) = ledger_with_pr(1);
        ledger
            .set_done(repo_id, 1, "head", ts("2026-07-01T00:00:00Z"))
            .unwrap();
        ledger
            .record_state_transition(
                repo_id,
                1,
                PrState::Merged,
                Some(ts("2026-08-20T00:00:00Z")),
                now(),
            )
            .unwrap();
        ledger.clean_activity(ts("2026-08-10T00:00:00Z")).unwrap();

        assert!(
            !ledger
                .record_forge_activity(
                    repo_id,
                    1,
                    &forge_lifecycle(
                        ActivityKind::PrClosed,
                        "historical-close",
                        "2026-08-01T00:00:00Z",
                    ),
                )
                .unwrap()
        );

        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ActivityKind::PrMerged);
        let shown = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(shown.pr.state, PrState::Merged);
        assert_eq!(shown.pr.state_changed_at, Some(ts("2026-08-20T00:00:00Z")));
    }

    #[test]
    fn cleanup_boundary_advances_monotonically_and_dry_run_does_not_advance_it() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let first = ts("2025-08-20T00:00:00Z");
        let second = ts("2026-01-01T00:00:00Z");

        ledger.preview_activity_cleanup(first).unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Done, "2025-08-01T00:00:00Z"),
            )
            .unwrap();
        assert_eq!(all_activity(&ledger).len(), 1, "dry-run changed retention");

        ledger.clean_activity(first).unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Muted, "2025-08-19T00:00:00Z"),
            )
            .unwrap();
        ledger.clean_activity(ts("2025-01-01T00:00:00Z")).unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Unmuted, "2025-08-19T00:00:00Z"),
            )
            .unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Done, "2025-12-01T00:00:00Z"),
            )
            .unwrap();
        assert_eq!(all_activity(&ledger).len(), 1);

        ledger.clean_activity(second).unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Done, "2025-12-01T00:00:00Z"),
            )
            .unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Done, "2026-01-01T00:00:00Z"),
            )
            .unwrap();

        let events = all_activity(&ledger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].occurred_at, second);
    }

    #[test]
    fn failed_cleanup_does_not_advance_the_boundary() {
        let (ledger, repo_id) = ledger_with_pr(1);
        let cutoff = ts("2025-08-20T00:00:00Z");
        let mut rejected = local_activity(ActivityKind::Done, "2025-08-01T12:00:00Z");
        rejected.actor = Some("reject".into());
        ledger.record_activity(repo_id, 1, &rejected).unwrap();
        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_cleanup_boundary_test
                 BEFORE DELETE ON activity_events
                 BEGIN
                   SELECT RAISE(ABORT, 'reject cleanup');
                 END;",
            )
            .unwrap();

        assert!(ledger.clean_activity(cutoff).is_err());
        ledger
            .conn
            .borrow_mut()
            .batch_execute("DROP TRIGGER reject_cleanup_boundary_test")
            .unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &local_activity(ActivityKind::Muted, "2025-08-02T12:00:00Z"),
            )
            .unwrap();

        assert_eq!(all_activity(&ledger).len(), 2);
    }

    #[test]
    fn cleanup_rolls_back_when_sqlite_rejects_one_of_the_deletions() {
        let (ledger, repo_id) = ledger_with_pr(1);
        for actor in ["safe", "reject"] {
            let mut event = local_activity(ActivityKind::Done, "2025-08-01T12:00:00Z");
            event.actor = Some(actor.into());
            ledger.record_activity(repo_id, 1, &event).unwrap();
        }
        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_activity_cleanup
                 BEFORE DELETE ON activity_events
                 WHEN OLD.actor = 'reject'
                 BEGIN
                   SELECT RAISE(ABORT, 'reject activity cleanup');
                 END;",
            )
            .unwrap();

        assert!(ledger.clean_activity(ts("2025-08-20T00:00:00Z")).is_err());

        assert_eq!(
            ledger
                .preview_activity_cleanup(ts("2025-08-20T00:00:00Z"))
                .unwrap(),
            CleanupPreview {
                event_count: 2,
                pr_count: 1,
            }
        );
    }
}
