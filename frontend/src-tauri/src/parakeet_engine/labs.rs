//! Opt-in DirectML acceleration for Parakeet's encoder on Windows.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static ENABLED: OnceLock<AtomicBool> = OnceLock::new();

fn path() -> std::path::PathBuf {
    crate::paths::install_data_root().join("parakeet_gpu_enabled.txt")
}

fn flag() -> &'static AtomicBool {
    ENABLED.get_or_init(|| {
        let saved = std::fs::read_to_string(path()).map(|value| value.trim() == "true").unwrap_or(false);
        AtomicBool::new(saved)
    })
}

pub fn enabled() -> bool { flag().load(Ordering::Relaxed) }

fn save(value: bool) -> Result<(), String> {
    let file = path();
    if let Some(parent) = file.parent() { std::fs::create_dir_all(parent).map_err(|error| error.to_string())?; }
    std::fs::write(file, if value { "true" } else { "false" }).map_err(|error| error.to_string())?;
    flag().store(value, Ordering::Relaxed);
    Ok(())
}

#[tauri::command]
pub fn get_parakeet_gpu_enabled() -> bool { enabled() }

/// Reload the current model before accepting the setting. On failure restore
/// the previous mode so the UI never claims GPU acceleration was activated.
#[tauri::command]
pub async fn set_parakeet_gpu_enabled(value: bool) -> Result<(), String> {
    if value && !cfg!(windows) { return Err("Parakeet DirectML acceleration is Windows-only".into()); }
    let previous = enabled();
    if previous == value { return Ok(()); }
    let engine = {
        let guard = super::commands::PARAKEET_ENGINE.lock().map_err(|_| "Parakeet engine lock poisoned")?;
        guard.as_ref().cloned()
    };
    let current = if let Some(engine) = &engine { engine.get_current_model().await } else { None };
    save(value)?;
    if let (Some(engine), Some(model)) = (engine, current) {
        engine.unload_model().await;
        if let Err(error) = engine.load_model(&model).await {
            let _ = save(previous);
            let restore = engine.load_model(&model).await;
            if let Err(restore_error) = restore {
                return Err(format!("Parakeet GPU initialization failed: {error}; previous model could not be restored: {restore_error}"));
            }
            return Err(format!("Parakeet GPU initialization failed: {error}"));
        }
    }
    Ok(())
}
