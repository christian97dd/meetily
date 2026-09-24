use tauri::{AppHandle, Runtime};
use tauri_plugin_dialog::DialogExt;

/// Asks where to save and writes `content` there. Returns the saved path, or None if cancelled.
#[tauri::command]
pub async fn export_text_file<R: Runtime>(
    app: AppHandle<R>,
    suggested_name: String,
    content: String,
) -> Result<Option<String>, String> {
    let picked = tokio::task::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_file_name(suggested_name)
            .add_filter("Markdown", &["md"])
            .add_filter("Text", &["txt"])
            .blocking_save_file()
    })
    .await
    .map_err(|e| format!("Save dialog task failed: {}", e))?;

    let Some(picked) = picked else {
        return Ok(None);
    };
    let path = picked.into_path().map_err(|e| format!("Invalid save location: {}", e))?;

    std::fs::write(&path, content).map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    Ok(Some(path.to_string_lossy().into_owned()))
}
