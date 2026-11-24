//! Labs: keep learned voices current as meetings finish and speakers are
//! relabeled. Only contacts who already have a voice are updated; creating one
//! stays a manual, consent-based step.
//!
//! Ownership: one native worker drains a set of pending meeting IDs. A meeting
//! queued again while it waits is coalesced; one queued while it is being
//! processed runs once more afterwards, so the last label change always gets a
//! pass. The work is idempotent (a meeting's share is replaced, never added
//! twice), so ids pending at shutdown are simply dropped; Update voice on the
//! contact page catches up.
//!
//! The diarization operation guard is held only while reading the meeting's
//! links, never while embedding, so a long pass never blocks a speaker edit.
//! An edit that lands mid-pass queues the meeting again.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use sqlx::SqlitePool;
use tauri::{AppHandle, Emitter, Manager, Runtime};

use super::voice_profiles::{self, EnrollError, VoiceProfile};

/// Rapid edits (renaming several speakers in a row) settle into one pass.
const SETTLE: Duration = Duration::from_millis(1500);

static PENDING: Mutex<Vec<String>> = Mutex::new(Vec::new());
static RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VoicesUpdated {
    meeting_id: String,
    names: Vec<String>,
}

/// Queue a meeting whose speaker labels just became final or changed. Returns
/// at once; a no-op unless voice profiles and automatic updates are both on.
pub fn queue<R: Runtime>(app: &AppHandle<R>, meeting_id: &str) {
    if !voice_profiles::auto_update_enabled() {
        return;
    }
    if let Ok(mut pending) = PENDING.lock() {
        if !pending.iter().any(|id| id == meeting_id) {
            pending.push(meeting_id.to_string());
        }
    }
    if !RUNNING.swap(true, Ordering::AcqRel) {
        let app = app.clone();
        tauri::async_runtime::spawn(async move { run(app).await });
    }
}

#[tauri::command]
pub fn queue_voice_profile_update<R: Runtime>(app: AppHandle<R>, meeting_id: String) {
    queue(&app, &meeting_id);
}

async fn run<R: Runtime>(app: AppHandle<R>) {
    loop {
        tokio::time::sleep(SETTLE).await;
        let next = PENDING.lock().ok().and_then(|mut pending| {
            (!pending.is_empty()).then(|| pending.remove(0))
        });
        let Some(meeting_id) = next else {
            RUNNING.store(false, Ordering::Release);
            // A meeting queued after the empty check but before the flag
            // cleared found the worker still running; pick it up here.
            let more = PENDING.lock().map(|pending| !pending.is_empty()).unwrap_or(false);
            if more && !RUNNING.swap(true, Ordering::AcqRel) {
                continue;
            }
            return;
        };
        if !voice_profiles::auto_update_enabled() {
            continue;
        }
        let Some(state) = app.try_state::<crate::state::AppState>() else {
            continue;
        };
        let pool = state.db_manager.pool().clone();
        match sync_meeting(&pool, &meeting_id).await {
            Ok(names) if !names.is_empty() => {
                log::info!("Updated voices from meeting {meeting_id}: {}", names.join(", "));
                let _ = app.emit("voice-profiles-auto-updated", VoicesUpdated { meeting_id, names });
            }
            Ok(_) => {}
            Err(message) => log::warn!("Automatic voice update stopped for meeting {meeting_id}: {message}"),
        }
    }
}

/// What a pass does for one contact who has a voice.
#[derive(Debug, PartialEq)]
enum Action {
    /// Learn (or relearn) this meeting's share from the speaker label.
    Learn(String),
    /// The contact is no longer in this meeting: remove its share, if any.
    Drop,
    /// The link is not trusted enough to learn from; leave the voice alone.
    Keep,
}

/// One link row: `(person_id, speaker_label, source, match_score, match_margin)`.
type Link = (String, String, String, Option<f64>, Option<f64>);

fn plan(profiles: &[VoiceProfile], links: &[Link]) -> Vec<(String, String, Action)> {
    profiles.iter().map(|profile| {
        let theirs: Vec<&Link> = links.iter().filter(|link| link.0 == profile.person_id).collect();
        let action = if theirs.is_empty() {
            Action::Drop
        } else if let Some(link) = theirs.iter().find(|link| voice_profiles::learnable_link(&link.2, link.3, link.4)) {
            Action::Learn(link.1.clone())
        } else {
            Action::Keep
        };
        (profile.person_id.clone(), profile.name.clone(), action)
    }).collect()
}

/// Brings every learned voice in step with one meeting's current labels and
/// returns the names of the voices that changed. `Err` only when voices are
/// unavailable altogether (feature off, models missing, data unreadable).
async fn sync_meeting(pool: &SqlitePool, meeting_id: &str) -> Result<Vec<String>, String> {
    let (profiles, links) = {
        let _guard = super::operation_guard().await;
        let profiles = voice_profiles::load().map_err(|error| error.to_string())?;
        let links: Vec<Link> = sqlx::query_as(
            "SELECT person_id, speaker_label, source, match_score, match_margin \
             FROM person_speakers WHERE meeting_id = ?",
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .map_err(|error| error.to_string())?;
        (profiles, links)
    };
    let before: HashMap<String, VoiceProfile> = profiles.iter()
        .map(|profile| (profile.person_id.clone(), profile.clone())).collect();

    let mut changed = Vec::new();
    for (person_id, name, action) in plan(&profiles, &links) {
        match action {
            Action::Keep => {}
            Action::Learn(label) => match voice_profiles::meeting_share(pool, meeting_id, &label).await {
                Ok((linked_id, linked_name, share)) if linked_id == person_id => {
                    let known = before.get(&person_id)
                        .and_then(|profile| profile.sources.iter().find(|earlier| earlier.meeting_id == meeting_id));
                    if known.is_some_and(|earlier| same_share(earlier, &share)) {
                        continue;
                    }
                    match voice_profiles::store_shares(&person_id, &linked_name, vec![share], false) {
                        Ok(_) => changed.push(linked_name),
                        Err(EnrollError::Unavailable(message)) => return Err(message),
                        Err(EnrollError::Meeting(message)) => log::debug!("{name}'s voice was not updated: {message}"),
                    }
                }
                // The label moved to someone else since the links were read;
                // that edit queued another pass.
                Ok(_) => {}
                Err(EnrollError::Unavailable(message)) => return Err(message),
                Err(EnrollError::Meeting(message)) => log::debug!("{name}'s voice was not updated: {message}"),
            },
            Action::Drop => {
                let mut dropped = false;
                let result = voice_profiles::edit_profiles(|profiles| {
                    let Some(index) = profiles.iter().position(|profile| profile.person_id == person_id) else {
                        return false;
                    };
                    match voice_profiles::drop_share(&profiles[index], meeting_id) {
                        Ok(Some(rebuilt)) => {
                            profiles[index] = rebuilt;
                            dropped = true;
                            true
                        }
                        Ok(None) => false,
                        Err(error) => {
                            log::warn!(
                                "Kept {name}'s voice although they are no longer in meeting {meeting_id}: \
                                 the rest of it would be too little to match ({error})"
                            );
                            false
                        }
                    }
                });
                match result {
                    Ok(()) if dropped => changed.push(name),
                    Ok(()) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
        }
    }
    Ok(changed)
}

/// Embedding is deterministic, so a meeting whose audio and labels did not
/// change for this contact yields the same share; that is not an update.
fn same_share(a: &voice_profiles::VoiceSource, b: &voice_profiles::VoiceSource) -> bool {
    a.turns == b.turns
        && a.sum.len() == b.sum.len()
        && a.sum.iter().zip(&b.sum).all(|(x, y)| (x - y).abs() <= 1e-4 * x.abs().max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::repositories::person::{LINK_MATCH, LINK_SAVED, LINK_USER};

    fn profile(person_id: &str) -> VoiceProfile {
        VoiceProfile {
            person_id: person_id.into(),
            name: person_id.to_uppercase(),
            embedding: vec![0.0; 128],
            samples: 2,
            model: String::new(),
            sources: Vec::new(),
        }
    }

    fn link(person_id: &str, label: &str, source: &str, score: Option<f64>, margin: Option<f64>) -> Link {
        (person_id.into(), label.into(), source.into(), score, margin)
    }

    #[test]
    fn only_trusted_links_are_learned_and_absent_contacts_drop() {
        let profiles = [profile("a"), profile("b"), profile("c"), profile("d"), profile("e")];
        let links = [
            link("a", "Alice", LINK_USER, None, None),
            link("b", "Bob", LINK_MATCH, Some(0.90), Some(0.13)),
            link("c", "Cara", LINK_MATCH, Some(0.90), Some(0.10)),
            link("d", "Dan", LINK_SAVED, None, None),
        ];
        let actions: Vec<Action> = plan(&profiles, &links).into_iter().map(|(_, _, action)| action).collect();
        assert_eq!(actions, vec![
            Action::Learn("Alice".into()),
            Action::Learn("Bob".into()),
            Action::Keep,
            Action::Keep,
            Action::Drop,
        ]);
    }

    #[test]
    fn a_recomputed_identical_share_is_not_a_change() {
        let share = |turns, value| voice_profiles::VoiceSource { meeting_id: "m".into(), sum: vec![value; 128], turns };
        assert!(same_share(&share(3, 1.5), &share(3, 1.5)));
        assert!(!same_share(&share(3, 1.5), &share(4, 1.5)));
        assert!(!same_share(&share(3, 1.5), &share(3, 1.6)));
    }
}
