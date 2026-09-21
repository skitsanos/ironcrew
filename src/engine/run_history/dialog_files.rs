use super::*;

pub(super) fn dialog_file_path(dialogs_dir: &Path, flow_path: Option<&str>, id: &str) -> PathBuf {
    match flow_path {
        Some(flow) => {
            let flow_dir = dialogs_dir.join(encode_flow_component(flow));
            let _ = std::fs::create_dir_all(&flow_dir);
            flow_dir.join(format!("{}.json", id))
        }
        None => dialogs_dir.join(format!("{}.json", id)),
    }
}

pub(super) fn load_dialog_file(path: &Path, id: &str) -> Result<Option<DialogStateRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = read_json_record(path)?;
    let record: DialogStateRecord = serde_json::from_str(&data).map_err(|e| {
        IronCrewError::Validation(format!("Failed to parse dialog state '{}': {}", id, e))
    })?;
    crate::engine::session_usage::validate(&record.usage)?;
    Ok(Some(record))
}
