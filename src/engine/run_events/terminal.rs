use super::*;

/// Terminal run metadata returned alongside a page. `event_sequence` is absent
/// while the terminal journal row is still pending or when its bounded writer
/// failed permanently; the authoritative run record still lets clients close
/// with an explicitly incomplete synthetic terminal event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventTerminalState {
    pub status: RunStatus,
    pub duration_ms: u64,
    pub usage: crate::usage::UsageSnapshot,
    pub event_sequence: Option<u64>,
}

impl RunEventTerminalState {
    pub fn validate(&self) -> Result<()> {
        self.usage
            .validate()
            .map_err(|error| IronCrewError::Validation(error.into()))?;
        if !self.status.is_terminal() {
            return Err(IronCrewError::Validation(
                "Run-event terminal state must contain a terminal run status".into(),
            ));
        }
        if let Some(sequence) = self.event_sequence {
            validate_sequence(sequence)?;
        }
        Ok(())
    }
}
