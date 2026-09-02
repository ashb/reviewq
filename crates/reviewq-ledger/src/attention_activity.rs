use diesel::prelude::*;
use jiff::Timestamp;
use reviewq_core::model::{ActivityKind, ActivityPayload, ActivitySource, Attention};

use crate::{
    Doing, NewActivityEvent, RepoId, Result, connection::DbConnection, models::AttentionRecord,
    schema::attention,
};

pub(super) fn load(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
) -> Result<Vec<Attention>> {
    attention::table
        .filter(attention::repo_id.eq(repo_id))
        .filter(attention::pr_number.eq(number as i64))
        .order(attention::reason)
        .load::<AttentionRecord>(conn)
        .doing("reading attention history evidence")?
        .into_iter()
        .map(|row| {
            let row = crate::attention_from_stored(row)?;
            Ok(Attention {
                reason: row.reason,
                since: row.since,
            })
        })
        .collect()
}

pub(super) fn record(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    before: Vec<Attention>,
    observed_at: Timestamp,
) -> Result<()> {
    let after = load(conn, repo_id, number)?;
    let unchanged = before.len() == after.len()
        && before
            .iter()
            .all(|old| after.iter().any(|new| old.same_evidence(new)));
    if unchanged {
        return Ok(());
    }
    crate::activity::insert_activity(
        conn,
        repo_id,
        number,
        &NewActivityEvent {
            relation: reviewq_core::model::ActivityRelation::Relevant,
            source: ActivitySource::Local,
            kind: ActivityKind::AttentionChanged,
            occurred_at: observed_at,
            recorded_at: observed_at,
            actor: None,
            head_sha: None,
            external_id: None,
            permalink: None,
            payload: ActivityPayload::AttentionChanged { before, after },
        },
        false,
        false,
    )?;
    Ok(())
}
