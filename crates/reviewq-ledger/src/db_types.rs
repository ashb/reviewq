use diesel::{
    deserialize::{self, FromSql, FromSqlRow},
    expression::AsExpression,
    serialize::{self, IsNull, Output, ToSql},
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
};
use jiff::Timestamp;
use reviewq_core::model::PrState;

use crate::RepoId;

#[derive(Debug, Clone, Copy, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
pub(super) struct DbPrState(PrState);

impl DbPrState {
    pub(super) fn into_state(self) -> PrState {
        self.0
    }
}

impl From<PrState> for DbPrState {
    fn from(state: PrState) -> Self {
        Self(state)
    }
}

impl FromSql<Text, Sqlite> for DbPrState {
    fn from_sql(
        value: <Sqlite as diesel::backend::Backend>::RawValue<'_>,
    ) -> deserialize::Result<Self> {
        let value = <String as FromSql<Text, Sqlite>>::from_sql(value)?;
        PrState::from_wire(&value)
            .map(Self)
            .ok_or_else(|| format!("bad PR state {value:?}").into())
    }
}

impl ToSql<Text, Sqlite> for DbPrState {
    fn to_sql<'b>(&'b self, output: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        output.set_value(self.0.as_str());
        Ok(IsNull::No)
    }
}

impl FromSql<BigInt, Sqlite> for RepoId {
    fn from_sql(
        value: <Sqlite as diesel::backend::Backend>::RawValue<'_>,
    ) -> deserialize::Result<Self> {
        i64::from_sql(value).map(Self)
    }
}

impl ToSql<BigInt, Sqlite> for RepoId {
    fn to_sql<'b>(&'b self, output: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        output.set_value(self.0);
        Ok(IsNull::No)
    }
}

#[derive(Debug, Clone, AsExpression, FromSqlRow)]
#[diesel(sql_type = Text)]
pub(super) struct DbTimestamp(Timestamp);

impl DbTimestamp {
    pub(super) fn into_timestamp(self) -> Timestamp {
        self.0
    }
}

impl From<Timestamp> for DbTimestamp {
    fn from(timestamp: Timestamp) -> Self {
        Self(timestamp)
    }
}

impl FromSql<Text, Sqlite> for DbTimestamp {
    fn from_sql(
        value: <Sqlite as diesel::backend::Backend>::RawValue<'_>,
    ) -> deserialize::Result<Self> {
        let value = <String as FromSql<Text, Sqlite>>::from_sql(value)?;
        value.parse().map(Self).map_err(Into::into)
    }
}

impl ToSql<Text, Sqlite> for DbTimestamp {
    fn to_sql<'b>(&'b self, output: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        output.set_value(self.0.to_string());
        Ok(IsNull::No)
    }
}
