#[tauri::command]
pub fn save_file(path: String, body: String) -> Result<(), String> {
    std::fs::write(path, body).map_err(|e| e.to_string())
}
