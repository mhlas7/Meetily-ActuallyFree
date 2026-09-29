//! Persisted opt-in Whisper silence threshold. The worker reads an atomic value;
//! settings I/O never runs in the capture or transcription hot path.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static STRICT_SILENCE: OnceLock<AtomicBool> = OnceLock::new();

fn flag() -> &'static AtomicBool {
    STRICT_SILENCE.get_or_init(|| {
        let enabled = std::fs::read_to_string(path())
            .map(|value| value.trim() == "true")
            .unwrap_or(false);
        AtomicBool::new(enabled)
    })
}

fn path() -> std::path::PathBuf {
    crate::paths::install_data_root().join("whisper_strict_silence.txt")
}

pub fn strict_silence_enabled() -> bool {
    flag().load(Ordering::Relaxed)
}

#[tauri::command]
pub fn get_whisper_strict_silence() -> bool {
    strict_silence_enabled()
}

#[tauri::command]
pub fn set_whisper_strict_silence(enabled: bool) -> Result<(), String> {
    let file = path();
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(file, if enabled { "true" } else { "false" })
        .map_err(|error| error.to_string())?;
    flag().store(enabled, Ordering::Relaxed);
    Ok(())
}
