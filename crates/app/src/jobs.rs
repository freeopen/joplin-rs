//! Background sync/decrypt jobs for the GUI.
//!
//! Split from the former egui `main.rs`: worker threads are framework
//! agnostic. They report progress through a [`JobChannel`] (shared mutexes)
//! and post one [`JobEvent`] when done; the GPUI layer polls and reacts.
//!
//! Stage 1 is a password-less download (encrypted items become placeholders).
//! Stage 2 is the E2EE decryption pass; it is triggered either automatically
//! (a password is available) or after the user submits the prompt dialog.

use crate::app::{
    e2ee_auto_start_pending, AppState, DecryptSummary, JobChannel, JobEvent, Stage1Summary,
};

/// Kick off the sync job (two-way: delete remote, upload local changes, then
/// download; a provided password also decrypts while downloading) unless a job
/// is running.
pub fn start_download(state: &mut AppState) {
    if state.jobs.is_busy() {
        return;
    }
    *state.jobs.running.lock().unwrap() = true;
    *state.jobs.event.lock().unwrap() = None;
    state.set_status("开始同步…".into());

    let config = state.config.clone();
    let data_dir = crate::app::data_dir();
    let jobs: JobChannel = state.jobs.clone();
    // Only a password the user actually supplied this session is used for the
    // combined pass: the stored/WebDAV fallback must not hard-fail a sync that
    // would otherwise succeed as a password-less download (the separate
    // decrypt pass still uses the fallback).
    let password = if state.e2ee_session_password.is_empty() {
        None
    } else {
        Some(state.e2ee_session_password.clone())
    };

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async move {
            let storage = joplin_rs_core::storage::Storage::open(&data_dir.join("joplin-rs.db"))?;
            let client = joplin_rs_core::webdav::WebDavClient::new_with_tls(
                &config.webdav_url,
                &config.username,
                config.password.as_deref().unwrap_or(""),
                &config.tls_options(),
            )?;
            let engine = joplin_rs_core::sync::SyncEngine::new(&client, &storage);
            let outcome = engine
                .sync(password.as_deref(), &mut |msg| {
                    *jobs.status.lock().unwrap() = msg.to_string();
                })
                .await?;
            let pending = storage.items_still_encrypted().unwrap_or_default().len();
            Ok::<_, anyhow::Error>((
                Stage1Summary {
                    downloaded: outcome.downloaded,
                    deleted: outcome.deleted,
                    resources: outcome.resources_downloaded,
                    uploaded: outcome.uploaded,
                    deleted_remote: outcome.deleted_remote,
                    conflicts: outcome.skipped_conflicts.clone(),
                },
                pending,
            ))
        });
        *jobs.running.lock().unwrap() = false;
        // Post the event only after clearing the busy flag: the UI reacts
        // to this event by possibly starting the decrypt job, which is
        // skipped while busy() is still true.
        *jobs.event.lock().unwrap() = Some(JobEvent::SyncDone(result.map_err(|e| e.to_string())));
    });
}

/// Kick off stage 2 (E2EE decryption). If no password is available, opens
/// the prompt dialog instead (caller shows `state.dialog`). Records the
/// password on the state so the auto-continue pass can tell which values
/// have already been tried.
pub fn start_decrypt(state: &mut AppState) {
    if state.jobs.is_busy() {
        return;
    }
    let Some(password) = state.e2ee_password_for_decrypt() else {
        state.e2ee_input.clear();
        state.e2ee_prompt_error = None;
        state.dialog = crate::app::Dialog::E2eePrompt;
        return;
    };
    *state.jobs.running.lock().unwrap() = true;
    *state.jobs.event.lock().unwrap() = None;
    state.e2ee_job_password = password.clone();
    state.set_status("开始解密…".into());

    let config = state.config.clone();
    let data_dir = crate::app::data_dir();
    let jobs = state.jobs.clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async move {
            let storage = joplin_rs_core::storage::Storage::open(&data_dir.join("joplin-rs.db"))?;
            let client = joplin_rs_core::webdav::WebDavClient::new_with_tls(
                &config.webdav_url,
                &config.username,
                config.password.as_deref().unwrap_or(""),
                &config.tls_options(),
            )?;
            let engine = joplin_rs_core::sync::SyncEngine::new(&client, &storage);
            let info = engine.fetch_sync_info().await?;
            let out = engine
                .decrypt_pending(&info, &password, &mut |msg| {
                    *jobs.status.lock().unwrap() = msg.to_string();
                })
                .await?;
            Ok::<_, anyhow::Error>(DecryptSummary {
                items: out.items_decrypted,
                resources: out.resources_decrypted,
                still_encrypted: out.still_encrypted,
            })
        });
        *jobs.running.lock().unwrap() = false;
        *jobs.event.lock().unwrap() =
            Some(JobEvent::DecryptDone(result.map_err(|e| e.to_string())));
    });
}

/// Handle a finished download and every UI tick afterwards: auto-start the
/// decrypt pass when a not-yet-tried password is available. Without the
/// cancellation and already-tried guards this re-opened the prompt dialog
/// forever after a decline (and kept retrying a wrong fallback password).
/// A user-submitted retry goes through the prompt's explicit
/// [`start_decrypt`] and bypasses the already-tried guard.
pub fn auto_decrypt(state: &mut AppState) {
    let should = e2ee_auto_start_pending(
        state.e2ee_pending_count,
        state.jobs.is_busy(),
        state.e2ee_cancelled,
        state.e2ee_password_for_decrypt().as_deref(),
        &state.e2ee_job_password,
    );
    if should {
        start_decrypt(state);
    }
}
