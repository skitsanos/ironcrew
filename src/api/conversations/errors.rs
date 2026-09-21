use super::*;

pub(super) fn message_execution_error(
    error: &IronCrewError,
    tracker: &crate::usage::UsageTracker,
) -> (StatusCode, Json<ErrorResponse>) {
    let mut response = match tracker.budget().check() {
        Err(budget_error) => map_err_to_response(&budget_error.into()),
        Ok(()) => map_err_to_response(error),
    };
    response.1.budget = Some(tracker.budget().snapshot());
    response
}

pub(super) fn map_err_to_response(e: &IronCrewError) -> (StatusCode, Json<ErrorResponse>) {
    if let IronCrewError::Lua(error) = e
        && let Some(embedded) = embedded_lua_client_error(error)
    {
        return map_err_to_response(embedded);
    }
    // Client errors carry an actionable message; server-side failures are
    // logged in full and answered generically so storage and filesystem
    // internals do not reach the caller.
    match e {
        IronCrewError::TokenBudget(_) => {
            error_response(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        // Client errors keep their actionable message verbatim.
        IronCrewError::Conflict(_) => error_response(StatusCode::CONFLICT, e.to_string()),
        IronCrewError::Validation(_) => error_response(StatusCode::BAD_REQUEST, e.to_string()),
        _ => {
            tracing::error!(error = %e, "conversation request failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal server error".to_string(),
            )
        }
    }
}

pub(super) fn embedded_lua_client_error(error: &mlua::Error) -> Option<&IronCrewError> {
    let mut current = error;
    loop {
        if let Some(embedded) = current.downcast_ref::<IronCrewError>()
            && matches!(
                embedded,
                IronCrewError::Validation(_)
                    | IronCrewError::Conflict(_)
                    | IronCrewError::TokenBudget(_)
            )
        {
            return Some(embedded);
        }
        let parent = current.parent()?;
        current = parent;
    }
}

pub(super) fn map_lua_err_to_response(error: mlua::Error) -> (StatusCode, Json<ErrorResponse>) {
    map_err_to_response(&IronCrewError::Lua(error))
}
