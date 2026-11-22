//! The desktop bridge owns exit; all package and transaction logic stays in gateway.
use gateway::app_update::{self, TrustedHelperReady, UpdateService, UpdateStatus};
use gateway::state::GatewayState;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager, State, WebviewWindow};

pub type PendingExit = Mutex<Option<TrustedHelperReady>>;

fn local_window(window: &WebviewWindow) -> Result<(), String> {
    let url = window.url().map_err(|error| error.to_string())?;
    if window.label() != "main"
        || url.port().is_some()
        || !matches!(
            (url.scheme(), url.host_str()),
            ("tauri", Some("localhost")) | ("http" | "https", Some("tauri.localhost"))
        )
    {
        return Err("update-requires-local-desktop-window".into());
    }
    Ok(())
}

#[tauri::command]
pub fn update_status(
    window: WebviewWindow,
    service: State<'_, Arc<UpdateService>>,
) -> Result<UpdateStatus, String> {
    local_window(&window)?;
    Ok(match app_update::installed_app() {
        Ok(app) => service.persisted_status(&app),
        Err(_) => service.status(),
    })
}

#[tauri::command]
pub async fn check_update(
    window: WebviewWindow,
    service: State<'_, Arc<UpdateService>>,
) -> Result<UpdateStatus, String> {
    local_window(&window)?;
    service.check().await
}

#[tauri::command]
pub async fn prepare_update(
    window: WebviewWindow,
    offer_handle: String,
    service: State<'_, Arc<UpdateService>>,
) -> Result<UpdateStatus, String> {
    local_window(&window)?;
    service
        .prepare(
            &offer_handle,
            &app_update::installed_app()?,
            &gateway::runtime::data_dir(),
        )
        .await
}

#[tauri::command]
pub async fn prepare_development_update(
    window: WebviewWindow,
    archive_path: String,
    receipt_path: String,
    independent_sha256: String,
    confirmed: bool,
    service: State<'_, Arc<UpdateService>>,
) -> Result<UpdateStatus, String> {
    local_window(&window)?;
    let service = service.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        service.prepare_development(
            std::path::Path::new(&archive_path),
            std::path::Path::new(&receipt_path),
            &independent_sha256,
            confirmed,
            &app_update::installed_app()?,
            &gateway::runtime::data_dir(),
        )
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn install_update(
    window: WebviewWindow,
    app: AppHandle,
    prepared_handle: String,
    confirmed: bool,
    service: State<'_, Arc<UpdateService>>,
    state: State<'_, Arc<GatewayState>>,
) -> Result<UpdateStatus, String> {
    local_window(&window)?;
    let service = service.inner().clone();
    let state = state.inner().clone();
    let ready = tauri::async_runtime::spawn_blocking(move || {
        service.install(&state, &prepared_handle, confirmed)
    })
    .await
    .map_err(|error| error.to_string())??;
    *app.state::<PendingExit>()
        .lock()
        .map_err(|error| error.to_string())? = Some(ready);
    // ExitRequested follows the normal Tauri drain and preserves the maintenance lease.
    let status = app.state::<Arc<UpdateService>>().status();
    app.exit(0);
    Ok(status)
}
