//! Small, validated usage checkpoints shared by all durable session backends.
use crate::usage::UsageSnapshot;
use crate::utils::error::{IronCrewError, Result};

pub(crate) fn validate(snapshot: &UsageSnapshot) -> Result<()> {
    snapshot
        .validate()
        .map_err(|error| IronCrewError::Validation(error.into()))?;
    if snapshot.in_flight != 0 {
        return Err(IronCrewError::Validation(
            "Session usage checkpoint contains in-flight requests".into(),
        ));
    }
    Ok(())
}

pub(crate) fn encode(snapshot: &UsageSnapshot) -> Result<String> {
    validate(snapshot)?;
    serde_json::to_string(snapshot).map_err(|_| {
        IronCrewError::Validation("Session usage checkpoint cannot be serialized".into())
    })
}

pub(crate) fn decode(value: Option<&str>) -> Result<UsageSnapshot> {
    let value = value.filter(|value| value.len() <= 4096).ok_or_else(|| {
        IronCrewError::Validation("Session usage checkpoint missing or oversized".into())
    })?;
    let snapshot = serde_json::from_str(value)
        .map_err(|_| IronCrewError::Validation("Session usage checkpoint is invalid".into()))?;
    validate(&snapshot)?;
    Ok(snapshot)
}
