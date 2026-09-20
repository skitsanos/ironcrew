use crate::usage::UsageSnapshot;
use crate::utils::error::{IronCrewError, Result};

/// Historical counters are not converted into checked receipts. Missing
/// checkpoints remain explicitly unavailable; existing data is not deleted.
pub(super) fn migrate(conn: &rusqlite::Connection) -> Result<()> {
    let sql = format!(
        "ALTER TABLE runs ADD COLUMN usage TEXT NOT NULL DEFAULT '{}'",
        serde_json::to_string(&UsageSnapshot::unavailable())
            .expect("scalar usage snapshot serializes")
    );
    if let Err(error) = conn.execute(&sql, [])
        && !error.to_string().contains("duplicate column name: usage")
    {
        return Err(IronCrewError::Validation(format!(
            "SQLite usage schema: {error}"
        )));
    }
    Ok(())
}
