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
    AppState, DecryptSummary, JobChannel, JobEvent, Stage1Summary,
};

/// Kick off stage 1 (password-less download sync) unless a job is running.
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
            // No E2EE password here: encrypted items download as
            // placeholders and are decrypted in the separate pass.
            let outcome = engine
                .sync(None, &mut |msg| {
                    *jobs.status.lock().unwrap() = msg.to_string();
                })
                .await?;
            let pending = storage.items_still_encrypted().unwrap_or_default().len();
            Ok::<_, anyhow::Error>((
                Stage1Summary {
                    downloaded: outcome.downloaded,
                    deleted: outcome.deleted,
                    resources: outcome.resources_downloaded,
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
/// the prompt dialog instead (caller shows `state.dialog`).
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

/// Handle a finished download: with a stored password, auto-start decrypt;
/// otherwise surface the E2EE prompt dialog. Called by the UI layer after
/// [`AppState::handle_job_event`] reports a SyncDone.
pub fn after_sync_done(state: &mut AppState) {
    if state.e2ee_pending_count > 0 && state.e2ee_password_for_decrypt().is_some() {
        start_decrypt(state);
    }
}
