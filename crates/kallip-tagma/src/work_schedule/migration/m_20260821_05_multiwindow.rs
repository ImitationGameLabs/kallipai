//! Multi-window rewrite: weekly/monthly specs grow from one flat
//! start/end pair to a window list.
//!
//! Existing rows carry a single window, so the rewrite is a mechanical
//! reshape: `{"mode":"weekly","days":N,"start_minute":S,"end_minute":E}`
//! becomes the same spec with `"windows":[{"start_minute":S,
//! "end_minute":E}]`. Interval and always specs have no windows and
//! pass through untouched; rows already carrying a window list (the
//! shape this migration produces) are left as-is, which makes the
//! migration idempotent.
//!
//! The rewrite runs on the Rust side: the spec JSON is parsed with the
//! serde model, reshaped, and written back, so there is no second
//! implementation of the spec shape in SQL.

use sea_orm::Statement;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let rows = manager
            .get_connection()
            .query_all(Statement::from_sql_and_values(
                manager.get_database_backend(),
                "SELECT id, spec FROM work_schedules;",
                [],
            ))
            .await?;
        for row in rows {
            let (id, spec_json): (String, String) =
                (row.try_get("", "id")?, row.try_get("", "spec")?);
            let Some(rewritten) = rewrite_spec(&spec_json) else {
                continue;
            };
            manager
                .get_connection()
                .execute(Statement::from_sql_and_values(
                    manager.get_database_backend(),
                    "UPDATE work_schedules SET spec = $1 WHERE id = $2;",
                    [rewritten.into(), id.into()],
                ))
                .await?;
        }
        Ok(())
    }
}

/// Reshape a flat single-window spec into the window-list form. Returns
/// None for specs that need no rewrite (interval/always, already a
/// list, or unparsable — the last is left for validate to reject).
fn rewrite_spec(spec_json: &str) -> Option<String> {
    let mut v: serde_json::Value = serde_json::from_str(spec_json).ok()?;
    let mode = v.get("mode")?.as_str()?;
    if mode != "weekly" && mode != "monthly" {
        return None;
    }
    if v.get("windows").is_some() {
        return None; // already migrated
    }
    let start = v.get("start_minute")?.as_u64()?;
    let end = v.get("end_minute")?.as_u64()?;
    let obj = v.as_object_mut()?;
    obj.remove("start_minute");
    obj.remove("end_minute");
    obj.insert(
        "windows".into(),
        serde_json::json!([{ "start_minute": start, "end_minute": end }]),
    );
    serde_json::to_string(&v).ok()
}

#[cfg(test)]
mod tests {
    use super::rewrite_spec;

    #[test]
    fn reshapes_flat_weekly_into_window_list() {
        let out =
            rewrite_spec(r#"{"mode":"weekly","days":31,"start_minute":540,"end_minute":1020}"#)
                .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["windows"][0]["start_minute"], 540);
        assert_eq!(v["windows"][0]["end_minute"], 1020);
        assert!(v.get("start_minute").is_none());
    }

    #[test]
    fn skips_already_migrated_and_other_modes() {
        let flat = r#"{"mode":"monthly","days":1,"windows":[{"start_minute":0,"end_minute":60}]}"#;
        assert_eq!(rewrite_spec(flat), None);
        assert_eq!(rewrite_spec(r#"{"mode":"always"}"#), None);
        assert_eq!(rewrite_spec("not json"), None);
    }
}
