use diesel::{prelude::*, upsert::excluded};
use jiff::Timestamp;

use crate::models::AttentionRecord;
use crate::schema::{attention, prs, team_memberships};
use crate::{DbTimestamp, Doing, Encoding, Ledger, RepoId, Result, attention_from_stored, corrupt};

/// A complete cached membership snapshot of one team on one forge host.
#[derive(Debug)]
pub struct TeamMembership {
    /// Member logins, including inherited membership returned by the forge.
    pub members: Vec<String>,
    /// When this snapshot was fetched successfully.
    pub refreshed_at: Timestamp,
}

impl Ledger {
    /// Read one team's last successful membership snapshot.
    pub fn team_members(
        &self,
        host: &str,
        organization: &str,
        team: &str,
    ) -> Result<Option<TeamMembership>> {
        let row = team_memberships::table
            .find((
                host.to_ascii_lowercase(),
                organization.to_ascii_lowercase(),
                team.to_ascii_lowercase(),
            ))
            .select((team_memberships::members, team_memberships::refreshed_at))
            .first::<(String, DbTimestamp)>(&mut *self.conn.borrow_mut())
            .optional()
            .doing("reading cached team membership")?;
        row.map(|(members, refreshed_at)| {
            Ok(TeamMembership {
                members: serde_json::from_str(&members)
                    .map_err(|source| corrupt("team membership", source))?,
                refreshed_at: refreshed_at.into_timestamp(),
            })
        })
        .transpose()
    }

    /// Atomically replace a team's complete membership, preserving newer snapshots.
    pub fn set_team_members(
        &self,
        host: &str,
        organization: &str,
        team: &str,
        members: &[String],
        refreshed_at: Timestamp,
    ) -> Result<()> {
        use diesel::query_dsl::methods::FilterDsl as _;
        diesel::insert_into(team_memberships::table)
            .values((
                team_memberships::host.eq(host.to_ascii_lowercase()),
                team_memberships::organization.eq(organization.to_ascii_lowercase()),
                team_memberships::team.eq(team.to_ascii_lowercase()),
                team_memberships::members
                    .eq(serde_json::to_string(members).encoding("team membership")?),
                team_memberships::refreshed_at.eq(DbTimestamp::from(refreshed_at)),
            ))
            .on_conflict((
                team_memberships::host,
                team_memberships::organization,
                team_memberships::team,
            ))
            .do_update()
            .set((
                team_memberships::members.eq(excluded(team_memberships::members)),
                team_memberships::refreshed_at.eq(excluded(team_memberships::refreshed_at)),
            ))
            .filter(team_memberships::refreshed_at.le(excluded(team_memberships::refreshed_at)))
            .execute(&mut *self.conn.borrow_mut())
            .doing("storing team membership")?;
        Ok(())
    }

    /// Rerank existing attention without fetching PR detail or creating activity.
    pub fn rank_attention(
        &self,
        repo_id: RepoId,
        authors: &[String],
        requesters: &[String],
    ) -> Result<()> {
        self.conn
            .borrow_mut()
            .immediate_transaction::<_, crate::LedgerError, _>(|conn| {
                let rows = attention::table
                    .inner_join(
                        prs::table.on(prs::repo_id
                            .eq(attention::repo_id)
                            .and(prs::number.eq(attention::pr_number))),
                    )
                    .filter(attention::repo_id.eq(repo_id))
                    .select((AttentionRecord::as_select(), prs::author))
                    .load::<(AttentionRecord, String)>(conn)
                    .doing("reading attention to rank")?;
                for (row, author) in rows {
                    let key = (row.repo_id, row.pr_number, row.reason.clone());
                    let mut item = attention_from_stored(row)?;
                    let previous = item.priority;
                    item.rank(&author, authors, requesters);
                    if item.priority != previous {
                        diesel::update(attention::table.find(key))
                            .set(attention::priority.eq(item.priority))
                            .execute(conn)
                            .doing("ranking attention")?;
                    }
                }
                Ok(())
            })
    }
}

#[cfg(test)]
mod tests {
    use crate::Ledger;
    use diesel::prelude::*;

    #[test]
    fn a_team_snapshot_replaces_members_and_is_scoped_to_its_host_and_org() {
        let ledger = Ledger::open_in_memory().unwrap();
        let first = "2026-09-17T10:00:00Z".parse().unwrap();
        let later = "2026-09-18T10:00:00Z".parse().unwrap();
        ledger
            .set_team_members(
                "github.com",
                "apache",
                "airflow-committers",
                &["ashb".into()],
                first,
            )
            .unwrap();
        ledger
            .set_team_members(
                "github.com",
                "apache",
                "airflow-committers",
                &["alice".into()],
                later,
            )
            .unwrap();
        ledger
            .set_team_members(
                "GITHUB.COM",
                "Apache",
                "Airflow-Committers",
                &["stale".into()],
                first,
            )
            .unwrap();
        assert_eq!(
            crate::schema::team_memberships::table
                .count()
                .get_result::<i64>(&mut *ledger.conn.borrow_mut())
                .unwrap(),
            1
        );
        let cached = ledger
            .team_members("github.com", "apache", "airflow-committers")
            .unwrap()
            .unwrap();
        assert_eq!(cached.members, ["alice"]);
        assert_eq!(cached.refreshed_at, later);
        assert!(
            ledger
                .team_members("elsewhere.example", "apache", "airflow-committers")
                .unwrap()
                .is_none()
        );
        assert!(
            ledger
                .team_members("github.com", "other", "airflow-committers")
                .unwrap()
                .is_none()
        );
        ledger
            .set_team_members("github.com", "apache", "airflow-committers", &[], later)
            .unwrap();
        assert!(
            ledger
                .team_members("github.com", "apache", "airflow-committers")
                .unwrap()
                .unwrap()
                .members
                .is_empty()
        );
    }
}
