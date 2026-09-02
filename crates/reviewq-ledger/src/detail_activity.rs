use std::collections::BTreeMap;

use diesel::prelude::*;
use jiff::Timestamp;
use reviewq_core::model::{
    ActivityKind, ActivityPayload, ActivityRelation, ActivitySource, ClassifyCtx, PrSnapshot,
    Resolution, ThreadState,
};

use crate::{
    ActivityEventId, Doing, Encoding, NewActivityEvent, RepoId, Result,
    activity::{decode_activity_timestamp, insert_activity, insert_evidence},
    connection::DbConnection,
    schema::{activity_events as e, attention, my_state as a, threads},
};

pub(super) type DetailActivity<'a> = (&'a PrSnapshot, &'a [NewActivityEvent], &'a ClassifyCtx<'a>);

pub(super) fn observe_detail(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    threads: &[ThreadState],
    events: &[NewActivityEvent],
    viewer: Option<&str>,
    now: Timestamp,
) -> Result<(Vec<Resolution>, Option<Timestamp>)> {
    let (_, reviewed_at) = retain_latest_review(conn, repo_id, number, events)?;
    for event in events {
        insert_activity(conn, repo_id, number, event, true, false)?;
    }
    let previous_states = threads::table
        .filter(threads::repo_id.eq(repo_id))
        .filter(threads::pr_number.eq(number as i64))
        .select((threads::thread_id, threads::is_resolved))
        .load::<(String, bool)>(conn)
        .doing("reading previous thread states")?;
    let previous = threads::table
        .inner_join(e::table)
        .filter(threads::repo_id.eq(repo_id))
        .filter(threads::pr_number.eq(number as i64))
        .select((threads::thread_id, e::occurred_at, e::id))
        .load::<(String, String, ActivityEventId)>(conn)
        .doing("reading resolution evidence")?;
    let mut resolutions = Vec::new();
    let mut resolution_events = BTreeMap::new();
    for thread in threads {
        let prior = previous.iter().find(|(id, _, _)| id == &thread.thread_id);
        let participated = thread.i_own || thread.my_last_comment_at.is_some();
        let resolved_by_me = viewer.is_some_and(|viewer| {
            thread
                .resolved_by
                .as_deref()
                .is_some_and(|by| by.eq_ignore_ascii_case(viewer))
        });
        let relation = if resolved_by_me && thread.is_resolved {
            ActivityRelation::Own
        } else if participated {
            ActivityRelation::Relevant
        } else {
            ActivityRelation::Context
        };
        if relation == ActivityRelation::Context {
            let was_resolved = previous_states
                .iter()
                .find(|(id, _)| id == &thread.thread_id)
                .is_some_and(|(_, resolved)| *resolved);
            if thread.is_resolved != was_resolved {
                let mut event = thread_event(thread, thread.is_resolved, now);
                event.relation = relation;
                insert_activity(conn, repo_id, number, &event, true, false)?;
            }
            continue;
        }
        if thread.is_resolved {
            let at = if let Some((_, at, event_id)) = prior {
                resolution_events.insert(thread.thread_id.clone(), *event_id);
                decode_activity_timestamp(at.clone(), "resolution time")?
            } else {
                let mut event = thread_event(thread, true, now);
                event.relation = relation;
                insert_evidence(conn, repo_id, number, &event)?;
                let id = e::table
                    .filter(e::repo_id.eq(repo_id))
                    .filter(e::pr_number.eq(number as i64))
                    .filter(e::kind.eq("thread_resolved"))
                    .filter(e::external_id.eq(event.external_id.as_deref()))
                    .select(e::id)
                    .first::<ActivityEventId>(conn)
                    .doing("finding resolution evidence")?;
                resolution_events.insert(thread.thread_id.clone(), id);
                now
            };
            resolutions.push(Resolution {
                thread_id: thread.thread_id.clone(),
                at,
            });
        } else if prior.is_some() {
            insert_activity(
                conn,
                repo_id,
                number,
                &thread_event(thread, false, now),
                true,
                false,
            )?;
        }
    }
    crate::replace_threads(conn, repo_id, number, threads, Some(&resolution_events))?;
    Ok((resolutions, reviewed_at))
}

fn thread_event(thread: &ThreadState, resolved: bool, now: Timestamp) -> NewActivityEvent {
    NewActivityEvent {
        relation: ActivityRelation::Relevant,
        source: ActivitySource::Forge,
        kind: if resolved {
            ActivityKind::ThreadResolved
        } else {
            ActivityKind::ThreadReopened
        },
        occurred_at: now,
        recorded_at: now,
        actor: if resolved {
            thread.resolved_by.clone()
        } else {
            None
        },
        head_sha: None,
        external_id: Some(format!(
            "observed-thread:{}:{resolved}:{now}",
            thread.thread_id
        )),
        permalink: None,
        payload: ActivityPayload::ThreadStateChanged {
            thread_id: thread.thread_id.clone(),
            resolved,
            observed: true,
        },
    }
}

pub(super) fn retain_latest_review(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    events: &[NewActivityEvent],
) -> Result<(u64, Option<Timestamp>)> {
    let reviewed_at = a::table
        .inner_join(e::table)
        .filter(a::repo_id.eq(repo_id))
        .filter(a::number.eq(number as i64))
        .select(e::occurred_at)
        .first::<String>(conn)
        .optional()
        .doing("reading review acknowledgment")?
        .map(|at| decode_activity_timestamp(at, "review acknowledgment time"))
        .transpose()?;
    let latest = events
        .iter()
        .filter(|event| {
            event.relation == ActivityRelation::Own
                && event.kind == ActivityKind::ReviewSubmitted
                && event.external_id.is_some()
        })
        .max_by_key(|event| event.occurred_at);
    if let Some(event) = latest.filter(|event| reviewed_at.is_none_or(|at| at < event.occurred_at))
    {
        let inserted = insert_evidence(conn, repo_id, number, event)?;
        let id = e::table
            .filter(e::repo_id.eq(repo_id))
            .filter(e::pr_number.eq(number as i64))
            .filter(e::kind.eq("review_submitted"))
            .filter(e::external_id.eq(event.external_id.as_deref()))
            .select(e::id)
            .first::<ActivityEventId>(conn)
            .doing("finding review evidence")?;
        diesel::insert_into(a::table)
            .values((
                a::repo_id.eq(repo_id),
                a::number.eq(number as i64),
                a::last_review_event_id.eq(id),
            ))
            .on_conflict((a::repo_id, a::number))
            .do_update()
            .set(a::last_review_event_id.eq(id))
            .execute(conn)
            .doing("acknowledging prior activity")?;
        return Ok((u64::from(inserted), Some(event.occurred_at)));
    }
    Ok((0, reviewed_at))
}

pub(super) fn retain_history_review(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    events: &[NewActivityEvent],
) -> Result<u64> {
    let Some(observed_at) = events.iter().map(|event| event.recorded_at).max() else {
        return Ok(0);
    };
    let prior_attention = crate::attention_activity::load(conn, repo_id, number)?;
    let (inserted, reviewed_at) = retain_latest_review(conn, repo_id, number, events)?;
    let Some(reviewed_at) = reviewed_at else {
        return Ok(inserted);
    };
    let current = attention::table
        .filter(attention::repo_id.eq(repo_id))
        .filter(attention::pr_number.eq(number as i64))
        .filter(attention::reason.eq("resolved_unanswered"));
    if !diesel::select(diesel::dsl::exists(current))
        .get_result::<bool>(conn)
        .doing("checking resolution attention")?
    {
        return Ok(inserted);
    }
    let stored = threads::table
        .filter(threads::repo_id.eq(repo_id))
        .filter(threads::pr_number.eq(number as i64))
        .load::<crate::models::ThreadRecord>(conn)
        .doing("reading threads for acknowledgment")?;
    let threads = stored
        .into_iter()
        .map(crate::thread_from_stored)
        .collect::<Result<Vec<_>>>()?;
    let resolutions = threads::table
        .inner_join(e::table)
        .filter(threads::repo_id.eq(repo_id))
        .filter(threads::pr_number.eq(number as i64))
        .select((threads::thread_id, e::occurred_at))
        .load::<(String, String)>(conn)
        .doing("reading current resolutions")?
        .into_iter()
        .map(|(thread_id, at)| {
            Ok(Resolution {
                thread_id,
                at: decode_activity_timestamp(at, "resolution time")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mine = crate::load_my_state(conn, repo_id, number)?
        .map(crate::my_state_from_stored)
        .transpose()?
        .unwrap_or_default();
    let reason = reviewq_core::model::resolved_unanswered_attention(
        &threads,
        &mine,
        &ClassifyCtx {
            resolutions: &resolutions,
            reviewed_at: Some(reviewed_at),
            ..Default::default()
        },
    );
    if let Some(reason) = reason {
        diesel::update(current)
            .set((
                attention::since.eq(crate::db_types::DbTimestamp::from(reason.since)),
                attention::payload
                    .eq(serde_json::to_string(&reason.reason).encoding("resolution attention")?),
            ))
            .execute(conn)
            .doing("updating resolution attention after review")?;
    } else {
        diesel::delete(current)
            .execute(conn)
            .doing("clearing acknowledged resolution attention")?;
    }
    crate::attention_activity::record(conn, repo_id, number, prior_attention, observed_at)?;
    Ok(inserted)
}
