use crate::api::TranscriptSegment;
use anyhow::Result;
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use once_cell::sync::Lazy;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

static ENGINE_LIFECYCLE_LOCK: Lazy<Arc<AsyncMutex<()>>> =
    Lazy::new(|| Arc::new(AsyncMutex::new(())));

/// Last time STT was actively used (unix secs). Idle unload watches this.
static STT_LAST_ACTIVITY_SECS: AtomicU64 = AtomicU64::new(0);
/// Unix secs of last STT unload (0 = never). Exposed on Local stack page.
static STT_LAST_UNLOAD_SECS: AtomicU64 = AtomicU64::new(0);
/// Unix secs of last LLM sidecar shutdown.
static LLM_LAST_UNLOAD_SECS: AtomicU64 = AtomicU64::new(0);

/// How long STT may sit loaded with no work before we free VRAM/RAM.
pub const STT_IDLE_UNLOAD_SECS: u64 = 120;

pub fn stt_last_unload_secs() -> u64 {
    STT_LAST_UNLOAD_SECS.load(Ordering::Relaxed)
}
pub fn llm_last_unload_secs() -> u64 {
    LLM_LAST_UNLOAD_SECS.load(Ordering::Relaxed)
}
pub fn mark_llm_unloaded() {
    LLM_LAST_UNLOAD_SECS.store(now_secs(), Ordering::Relaxed);
}

pub(crate) async fn acquire_engine_lifecycle_lock() -> OwnedMutexGuard<()> {
    ENGINE_LIFECYCLE_LOCK.clone().lock_owned().await
}

/// Keeps a batch transcription's model loaded until every segment finishes.
/// Idle cleanup, manual memory cleanup, and LLM startup all use the same lock.
pub(crate) struct SttBatchLease {
    _engine_lifecycle_guard: OwnedMutexGuard<()>,
}

pub(crate) async fn acquire_stt_batch_lease() -> SttBatchLease {
    let engine_lifecycle_guard = acquire_engine_lifecycle_lock().await;
    prepare_for_stt().await;
    mark_stt_activity();
    SttBatchLease {
        _engine_lifecycle_guard: engine_lifecycle_guard,
    }
}

impl Drop for SttBatchLease {
    fn drop(&mut self) {
        // An idle-unload waiter re-checks activity after acquiring the lock.
        mark_stt_activity();
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Call whenever STT does real work so the idle unloader doesn't yank the model mid-use.
pub fn mark_stt_activity() {
    STT_LAST_ACTIVITY_SECS.store(now_secs(), Ordering::Relaxed);
}

/// Unload Whisper + Parakeet if neither is needed (not recording, idle long enough).
pub async fn unload_stt_if_idle() {
    let last = STT_LAST_ACTIVITY_SECS.load(Ordering::Relaxed);
    if last == 0 {
        return;
    }
    let idle_for = now_secs().saturating_sub(last);
    if idle_for < STT_IDLE_UNLOAD_SECS {
        return;
    }
    if crate::audio::recording_commands::is_recording().await {
        return;
    }

    let _guard = acquire_engine_lifecycle_lock().await;
    // Re-check under lock
    if crate::audio::recording_commands::is_recording().await {
        return;
    }
    let last = STT_LAST_ACTIVITY_SECS.load(Ordering::Relaxed);
    if now_secs().saturating_sub(last) < STT_IDLE_UNLOAD_SECS {
        return;
    }

    info!("🧊 STT idle for {idle_for}s — unloading Whisper/Parakeet to free memory");
    unload_both_stt_engines().await;
    STT_LAST_ACTIVITY_SECS.store(0, Ordering::Relaxed);
    STT_LAST_UNLOAD_SECS.store(now_secs(), Ordering::Relaxed);
}

async fn unload_both_stt_engines() {
    {
        use crate::parakeet_engine::commands::PARAKEET_ENGINE;
        let engine = {
            let guard = PARAKEET_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().cloned()
        };
        if let Some(e) = engine {
            if e.is_model_loaded().await {
                e.unload_model().await;
                info!("Unloaded Parakeet model");
            }
        }
    }
    {
        use crate::whisper_engine::commands::WHISPER_ENGINE;
        let engine = {
            let guard = WHISPER_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().cloned()
        };
        if let Some(e) = engine {
            if e.is_model_loaded().await {
                e.unload_model().await;
                info!("Unloaded Whisper model");
            }
        }
    }
}

/// Before loading STT: free the builtin LLM so VRAM isn't shared with a big model.
pub async fn prepare_for_stt() {
    info!("🔄 Preparing for STT — shutting down builtin LLM if running");
    let _ = crate::summary::summary_engine::force_shutdown_sidecar().await;
    mark_llm_unloaded();
}

/// Before starting the LLM sidecar: unload Whisper/Parakeet (unless recording).
pub async fn prepare_for_llm() {
    if crate::audio::recording_commands::is_recording().await {
        info!("🔄 Preparing for LLM — STT kept (recording in progress)");
        return;
    }
    info!("🔄 Preparing for LLM — unloading STT models");
    let _ = force_unload_stt().await;
}

/// Force-unload both STT engines (manual / settings). Safe no-op if recording.
pub async fn force_unload_stt() -> Result<(), String> {
    if crate::audio::recording_commands::is_recording().await {
        return Err("Cannot unload while recording".into());
    }
    let _guard = acquire_engine_lifecycle_lock().await;
    unload_both_stt_engines().await;
    STT_LAST_ACTIVITY_SECS.store(0, Ordering::Relaxed);
    STT_LAST_UNLOAD_SECS.store(now_secs(), Ordering::Relaxed);
    Ok(())
}

/// Free STT + builtin LLM memory (settings “Free all memory”).
pub async fn force_unload_all() -> Result<(), String> {
    if crate::audio::recording_commands::is_recording().await {
        return Err("Cannot free memory while recording".into());
    }
    force_unload_stt().await?;
    let _ = crate::summary::summary_engine::force_shutdown_sidecar().await;
    mark_llm_unloaded();
    Ok(())
}

/// Sum bytes under a directory tree (best-effort).
pub fn dir_size_bytes(path: &Path) -> u64 {
    fn walk(p: &Path, acc: &mut u64) {
        let Ok(rd) = std::fs::read_dir(p) else {
            return;
        };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, acc);
            } else if let Ok(m) = e.metadata() {
                *acc = acc.saturating_add(m.len());
            }
        }
    }
    let mut n = 0u64;
    if path.exists() {
        walk(path, &mut n);
    }
    n
}

/// Background loop: free STT VRAM a couple minutes after the last transcription work.
pub fn start_stt_idle_unloader() {
    tauri::async_runtime::spawn(async {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            unload_stt_if_idle().await;
        }
    });
}

/// Unload the transcription engine after a batch job (import or retranscription).
/// Skips unloading if a live recording is currently in progress, since recording
/// uses the same global engine instances.
pub(crate) async fn unload_engine_after_batch(use_parakeet: bool) {
    let _engine_lifecycle_guard = acquire_engine_lifecycle_lock().await;

    if crate::audio::recording_commands::is_recording().await {
        log::info!("Skipping model unload after batch: recording in progress");
        return;
    }

    if use_parakeet {
        use crate::parakeet_engine::commands::PARAKEET_ENGINE;
        let engine = {
            let guard = PARAKEET_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().cloned()
        };
        if let Some(e) = engine {
            e.unload_model().await;
        }
    } else {
        use crate::whisper_engine::commands::WHISPER_ENGINE;
        let engine = {
            let guard = WHISPER_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().cloned()
        };
        if let Some(e) = engine {
            e.unload_model().await;
        }
    }
    STT_LAST_ACTIVITY_SECS.store(0, Ordering::Relaxed);
}

/// Create transcript segments from transcription results.
/// Each tuple is (text, start_ms, end_ms) from VAD timestamps.
pub(crate) fn create_transcript_segments(
    transcripts: &[(String, f64, f64)],
    recording_started_at: DateTime<Utc>,
) -> Result<Vec<TranscriptSegment>> {
    transcripts
        .iter()
        .map(|(text, start_ms, end_ms)| {
            let start_seconds = start_ms / 1000.0;
            let end_seconds = end_ms / 1000.0;
            let duration = end_seconds - start_seconds;

            Ok(TranscriptSegment {
                id: format!("transcript-{}", Uuid::new_v4()),
                text: text.trim().to_string(),
                timestamp: crate::database::repositories::transcript::timestamp_from_offset(
                    recording_started_at,
                    start_seconds,
                )?,
                audio_start_time: Some(start_seconds),
                audio_end_time: Some(end_seconds),
                duration: Some(duration),
                // Import/retranscribe path: no live capture, so no speaker
                // hint exists; offline diarization can label these later.
                speaker: None,
            })
        })
        .collect()
}

/// Segment keys this writer produces from `TranscriptSegment`. Any other key in
/// an existing file is a recording-time field to carry over.
const TRANSCRIPT_JSON_WRITER_KEYS: [&str; 8] = [
    "id",
    "text",
    "timestamp",
    "audio_start_time",
    "audio_end_time",
    "duration",
    "speaker",
    "sequence_id",
];

/// Match key for one transcript line: start time in whole milliseconds plus
/// trimmed text. File ids (`seg_N`) and DB ids (`transcript-<uuid>`) differ, so
/// ids can't link a file segment to its DB row.
fn transcript_json_match_key(audio_start_time: Option<f64>, text: &str) -> Option<(i64, String)> {
    let start = audio_start_time.filter(|start| start.is_finite())?;
    Some(((start * 1000.0).round() as i64, text.trim().to_string()))
}

/// Recording-time fields from an existing transcripts.json, keyed by
/// `transcript_json_match_key`. A missing or malformed file yields no fields.
fn previous_transcript_json_fields(
    path: &Path,
) -> HashMap<(i64, String), serde_json::Map<String, serde_json::Value>> {
    let mut fields = HashMap::new();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return fields;
    };
    let previous: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(previous) => previous,
        Err(e) => {
            debug!("Ignoring unreadable {} while rewriting it: {}", path.display(), e);
            return fields;
        }
    };
    let Some(segments) = previous.get("segments").and_then(|s| s.as_array()) else {
        return fields;
    };
    for segment in segments.iter().filter_map(|s| s.as_object()) {
        let start = segment.get("audio_start_time").and_then(|v| v.as_f64());
        let text = segment.get("text").and_then(|v| v.as_str()).unwrap_or_default();
        let Some(key) = transcript_json_match_key(start, text) else {
            continue;
        };
        let kept: serde_json::Map<String, serde_json::Value> = segment
            .iter()
            .filter(|(name, _)| !TRANSCRIPT_JSON_WRITER_KEYS.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        if !kept.is_empty() {
            fields.insert(key, kept);
        }
    }
    fields
}

/// Write transcripts.json to a meeting folder (atomic write with temp file).
///
/// `segments` usually come from the DB, which doesn't store the recording-time
/// fields `recording_saver` writes during capture (`confidence`,
/// `display_time`). So a rewrite keeps those fields from the existing file for
/// every segment with the same start time and text. Fields this writer
/// produces always come from `segments`. Retranscription can return a line
/// with identical text and timing; it then keeps the earlier run's confidence.
pub(crate) fn write_transcripts_json(folder: &Path, segments: &[TranscriptSegment]) -> Result<()> {
    let transcript_path = folder.join("transcripts.json");
    let temp_path = folder.join(".transcripts.json.tmp");
    let previous_fields = previous_transcript_json_fields(&transcript_path);

    let json = serde_json::json!({
        "version": "1.0",
        "last_updated": chrono::Utc::now().to_rfc3339(),
        "total_segments": segments.len(),
        "segments": segments.iter().enumerate().map(|(i, s)| {
            let mut segment = serde_json::json!({
                "id": s.id,
                "text": s.text,
                "timestamp": s.timestamp,
                "audio_start_time": s.audio_start_time,
                "audio_end_time": s.audio_end_time,
                "duration": s.duration,
                "speaker": s.speaker,
                "sequence_id": i
            });
            let carried = transcript_json_match_key(s.audio_start_time, &s.text)
                .and_then(|key| previous_fields.get(&key));
            if let (Some(carried), Some(segment)) = (carried, segment.as_object_mut()) {
                for (name, value) in carried {
                    segment.entry(name.clone()).or_insert_with(|| value.clone());
                }
            }
            segment
        }).collect::<Vec<_>>()
    });

    let json_string = serde_json::to_string_pretty(&json)?;
    std::fs::write(&temp_path, &json_string)?;
    std::fs::rename(&temp_path, &transcript_path)?;

    info!(
        "Wrote transcripts.json with {} segments to {}",
        segments.len(),
        transcript_path.display()
    );
    Ok(())
}

/// Rewrite a meeting folder's transcripts.json from its DB rows, so the file
/// shows the same speaker labels as the app. Call after any change to
/// `transcripts.speaker`. A meeting without a folder on disk is skipped.
pub(crate) async fn mirror_transcripts_json(pool: &SqlitePool, meeting_id: &str) -> Result<()> {
    let folder: Option<Option<String>> =
        sqlx::query_scalar("SELECT folder_path FROM meetings WHERE id = ?")
            .bind(meeting_id)
            .fetch_optional(pool)
            .await?;
    let Some(folder) = folder.flatten().map(PathBuf::from).filter(|p| p.is_dir()) else {
        return Ok(());
    };
    let rows = sqlx::query_as::<_, (String, String, String, Option<f64>, Option<f64>, Option<f64>, Option<String>)>(
        "SELECT id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker \
         FROM transcripts WHERE meeting_id = ? ORDER BY audio_start_time ASC",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;
    let segments: Vec<TranscriptSegment> = rows
        .into_iter()
        .map(|(id, text, timestamp, audio_start_time, audio_end_time, duration, speaker)| {
            TranscriptSegment { id, text, timestamp, audio_start_time, audio_end_time, duration, speaker }
        })
        .collect();
    write_transcripts_json(&folder, &segments)
}

/// `mirror_transcripts_json` for callers whose DB change has already
/// committed. The DB stays the source of truth, so a file error is logged
/// instead of failing the edit.
pub(crate) async fn mirror_transcripts_json_logged(pool: &SqlitePool, meeting_id: &str) {
    if let Err(e) = mirror_transcripts_json(pool, meeting_id).await {
        warn!("Could not update transcripts.json for meeting {}: {}", meeting_id, e);
    }
}

/// Split a long speech segment at the lowest-energy (silence) point near the target size.
///
/// Scans for 100ms windows with minimal RMS energy within +/-3 seconds of each target
/// split point. If no clear silence is found, falls back to a 1-second overlap split
/// to avoid cutting words at boundaries.
pub(crate) fn split_segment_at_silence(
    segment: &crate::audio::vad::SpeechSegment,
    max_samples: usize,
) -> Vec<crate::audio::vad::SpeechSegment> {
    const SAMPLE_RATE: usize = 16000;
    // 100ms window for energy measurement (1600 samples at 16kHz)
    const ENERGY_WINDOW: usize = SAMPLE_RATE / 10;
    // Search +/-3 seconds around the target split point
    const SEARCH_RADIUS: usize = SAMPLE_RATE * 3;
    // RMS threshold below which we consider a window "silent"
    const SILENCE_RMS_THRESHOLD: f32 = 0.02;
    // Overlap to use when no silence boundary is found (1 second)
    const FALLBACK_OVERLAP: usize = SAMPLE_RATE;

    let total = segment.samples.len();
    if total <= max_samples {
        return vec![segment.clone()];
    }

    let ms_per_sample = (segment.end_timestamp_ms - segment.start_timestamp_ms)
        / segment.samples.len() as f64;
    let mut result = Vec::new();
    let mut pos = 0usize;

    while pos < total {
        let remaining = total - pos;
        if remaining <= max_samples {
            // Last chunk - take everything remaining
            let chunk_samples = segment.samples[pos..].to_vec();
            let chunk_start_ms = segment.start_timestamp_ms + (pos as f64 * ms_per_sample);
            let chunk_end_ms = segment.end_timestamp_ms;
            result.push(crate::audio::vad::SpeechSegment {
                samples: chunk_samples,
                start_timestamp_ms: chunk_start_ms,
                end_timestamp_ms: chunk_end_ms,
                confidence: segment.confidence,
            });
            break;
        }

        // Target split point
        let target = pos + max_samples;

        // Search window: [target - SEARCH_RADIUS, target + SEARCH_RADIUS]
        let search_start = target.saturating_sub(SEARCH_RADIUS).max(pos + SAMPLE_RATE);
        let search_end = (target + SEARCH_RADIUS).min(total.saturating_sub(ENERGY_WINDOW));

        // Find the lowest-energy 100ms window in the search range
        let mut best_split = target.min(total); // fallback: exact target
        let mut best_rms = f32::MAX;

        if search_start + ENERGY_WINDOW <= search_end {
            let mut idx = search_start;
            while idx + ENERGY_WINDOW <= search_end {
                let window = &segment.samples[idx..idx + ENERGY_WINDOW];
                let rms = (window.iter().map(|s| s * s).sum::<f32>() / ENERGY_WINDOW as f32).sqrt();
                if rms < best_rms {
                    best_rms = rms;
                    best_split = idx + ENERGY_WINDOW / 2; // split at center of quiet window
                }
                // Step by 10ms (160 samples) for efficiency
                idx += SAMPLE_RATE / 100;
            }
        }

        let split_at = best_split;
        if best_rms <= SILENCE_RMS_THRESHOLD {
            debug!(
                "Splitting at silence boundary: sample {} (RMS={:.4})",
                split_at, best_rms
            );
        } else {
            debug!(
                "No silence found near target (best RMS={:.4}), splitting with overlap at sample {}",
                best_rms, split_at
            );
        }

        // Determine the actual end of this chunk (with overlap if no silence)
        let chunk_end = if best_rms > SILENCE_RMS_THRESHOLD {
            (split_at + FALLBACK_OVERLAP).min(total)
        } else {
            split_at
        };

        let chunk_samples = segment.samples[pos..chunk_end].to_vec();
        let chunk_start_ms = segment.start_timestamp_ms + (pos as f64 * ms_per_sample);
        let chunk_end_ms = segment.start_timestamp_ms + (chunk_end as f64 * ms_per_sample);

        result.push(crate::audio::vad::SpeechSegment {
            samples: chunk_samples,
            start_timestamp_ms: chunk_start_ms,
            end_timestamp_ms: chunk_end_ms,
            confidence: segment.confidence,
        });

        // Advance position to where the current chunk actually ends
        // to avoid transcribing the overlap region twice
        pos = chunk_end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_lifecycle_lock_serializes_acquirers() {
        let guard = acquire_engine_lifecycle_lock().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async {
            started_tx.send(()).unwrap();
            let _guard = acquire_engine_lifecycle_lock().await;
            acquired_tx.send(()).unwrap();
        });

        started_rx.await.unwrap();
        assert!(acquired_rx.try_recv().is_err());
        drop(guard);

        acquired_rx.await.unwrap();
        waiter.await.unwrap();
    }

    #[tokio::test]
    async fn stt_batch_lease_blocks_model_cleanup_until_batch_finishes() {
        let lease = acquire_stt_batch_lease().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let cleanup = tokio::spawn(async {
            started_tx.send(()).unwrap();
            let _guard = acquire_engine_lifecycle_lock().await;
            acquired_tx.send(()).unwrap();
        });

        started_rx.await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut acquired_rx)
                .await
                .is_err()
        );
        drop(lease);

        tokio::time::timeout(std::time::Duration::from_secs(1), acquired_rx)
            .await
            .unwrap()
            .unwrap();
        cleanup.await.unwrap();
    }

    #[test]
    fn transcript_json_preserves_speaker_hint() {
        let dir = tempfile::tempdir().unwrap();
        let segments = vec![TranscriptSegment {
            id: "mic-row".to_string(),
            text: "Hello one two three".to_string(),
            timestamp: "2026-08-12T22:59:36Z".to_string(),
            audio_start_time: Some(14.97),
            audio_end_time: Some(18.88),
            duration: Some(3.91),
            speaker: Some("You".to_string()),
        }];

        write_transcripts_json(dir.path(), &segments).unwrap();
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("transcripts.json")).unwrap(),
        )
        .unwrap();

        assert_eq!(json["segments"][0]["speaker"], "You");
    }

    fn segment(id: &str, text: &str, start: f64, speaker: &str) -> TranscriptSegment {
        TranscriptSegment {
            id: id.to_string(),
            text: text.to_string(),
            timestamp: "2026-11-16T17:03:07.551Z".to_string(),
            audio_start_time: Some(start),
            audio_end_time: Some(start + 2.0),
            duration: Some(2.0),
            speaker: Some(speaker.to_string()),
        }
    }

    /// A transcripts.json as `recording_saver` writes it during capture.
    fn write_recording_time_json(folder: &Path) {
        let json = serde_json::json!({
            "version": "1.0",
            "last_updated": "2026-11-16T17:28:00Z",
            "total_segments": 2,
            "segments": [
                {"id": "seg_0", "text": "First line.", "audio_start_time": 0.0, "audio_end_time": 2.0,
                 "duration": 2.0, "display_time": "17:03:14", "confidence": 0.85, "sequence_id": 0,
                 "speaker": "Speaker 1"},
                {"id": "seg_1", "text": " Second line. ", "audio_start_time": 5.975, "audio_end_time": 7.975,
                 "duration": 2.0, "display_time": "17:03:19", "confidence": 0.21, "sequence_id": 1,
                 "speaker": "Speaker 4"}
            ]
        });
        std::fs::write(folder.join("transcripts.json"), json.to_string()).unwrap();
    }

    fn read_segments(folder: &Path) -> Vec<serde_json::Value> {
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(folder.join("transcripts.json")).unwrap(),
        )
        .unwrap();
        json["segments"].as_array().unwrap().clone()
    }

    #[test]
    fn transcripts_json_rewrite_keeps_recording_time_fields() {
        let dir = tempfile::tempdir().unwrap();
        write_recording_time_json(dir.path());

        let segments = vec![
            segment("transcript-a", "First line.", 0.0, "Paul M."),
            segment("transcript-b", "Second line.", 5.975, "Glenn R."),
        ];
        write_transcripts_json(dir.path(), &segments).unwrap();

        let written = read_segments(dir.path());
        assert_eq!(written[0]["speaker"], "Paul M.");
        assert_eq!(written[0]["id"], "transcript-a");
        assert_eq!(written[0]["timestamp"], "2026-11-16T17:03:07.551Z");
        assert_eq!(written[0]["display_time"], "17:03:14");
        assert_eq!(written[0]["confidence"], 0.85);
        assert_eq!(written[1]["speaker"], "Glenn R.");
        assert_eq!(written[1]["display_time"], "17:03:19");
        assert_eq!(written[1]["confidence"], 0.21);
    }

    #[test]
    fn transcripts_json_rewrite_drops_fields_when_text_changed() {
        let dir = tempfile::tempdir().unwrap();
        write_recording_time_json(dir.path());

        let segments = vec![segment("transcript-a", "A retranscribed line.", 0.0, "Speaker 1")];
        write_transcripts_json(dir.path(), &segments).unwrap();

        let written = read_segments(dir.path());
        assert_eq!(written.len(), 1);
        assert!(written[0].get("confidence").is_none());
        assert!(written[0].get("display_time").is_none());
    }

    #[test]
    fn transcripts_json_rewrite_ignores_malformed_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("transcripts.json"), "{ not json").unwrap();

        write_transcripts_json(dir.path(), &[segment("transcript-a", "Hello.", 1.0, "You")]).unwrap();

        let written = read_segments(dir.path());
        assert_eq!(written[0]["speaker"], "You");
        assert!(written[0].get("confidence").is_none());
    }

    async fn mirror_test_pool(folder_path: Option<&Path>) -> sqlx::SqlitePool {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        sqlx::raw_sql(
            "CREATE TABLE meetings (id TEXT PRIMARY KEY, folder_path TEXT);
             CREATE TABLE people (id TEXT PRIMARY KEY, display_name TEXT NOT NULL, normalized_name TEXT NOT NULL UNIQUE, notes TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, is_manual INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE person_speakers (person_id TEXT NOT NULL, meeting_id TEXT NOT NULL, speaker_label TEXT NOT NULL, source TEXT NOT NULL DEFAULT 'saved', match_score REAL, match_margin REAL, UNIQUE(meeting_id, speaker_label));
             CREATE TABLE transcripts (id TEXT PRIMARY KEY, meeting_id TEXT NOT NULL, transcript TEXT NOT NULL, timestamp TEXT NOT NULL, audio_start_time REAL, audio_end_time REAL, duration REAL, speaker TEXT, source_type TEXT);
             INSERT INTO transcripts VALUES
             ('transcript-a', 'm1', 'First line.', '2026-11-16T17:03:07.551Z', 0.0, 2.0, 2.0, 'Speaker 1', 'system'),
             ('transcript-b', 'm1', 'Second line.', '2026-11-16T17:03:13.526Z', 5.975, 7.975, 2.0, 'Speaker 4', 'system');",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO meetings VALUES ('m1', ?)")
            .bind(folder_path.map(|p| p.to_string_lossy().to_string()))
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    #[tokio::test]
    async fn mirror_transcripts_json_applies_speaker_rename() {
        let dir = tempfile::tempdir().unwrap();
        write_recording_time_json(dir.path());
        let pool = mirror_test_pool(Some(dir.path())).await;

        crate::database::repositories::person::PeopleRepository::rename_meeting_speaker(
            &pool, "m1", "Speaker 4", "Glenn R.",
        )
        .await
        .unwrap();
        mirror_transcripts_json(&pool, "m1").await.unwrap();

        let written = read_segments(dir.path());
        assert_eq!(written[0]["speaker"], "Speaker 1");
        assert_eq!(written[1]["speaker"], "Glenn R.");
        assert_eq!(written[1]["confidence"], 0.21);
        assert_eq!(written[1]["display_time"], "17:03:19");
    }

    #[tokio::test]
    async fn mirror_transcripts_json_skips_meetings_without_a_folder() {
        let pool = mirror_test_pool(None).await;
        mirror_transcripts_json(&pool, "m1").await.unwrap();

        let missing = tempfile::tempdir().unwrap().path().join("gone");
        let pool = mirror_test_pool(Some(&missing)).await;
        mirror_transcripts_json(&pool, "m1").await.unwrap();
        assert!(!missing.exists());
    }
}
