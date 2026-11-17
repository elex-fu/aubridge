//! The desktop hosts the same loopback gateway as aubridged.

use std::sync::{Arc, Mutex};

use gateway::runtime::{GatewayAccess, Runtime};
use gateway::state::GatewayState;
use tauri::async_runtime;
#[cfg(target_os = "macos")]
use tauri::menu::PredefinedMenuItem;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{
    App, AppHandle, Emitter, Manager, RunEvent, State, WebviewWindow, WindowEvent,
    RESTART_EXIT_CODE,
};

const TRAY_ID: &str = "aubridge-tray";
#[cfg(target_os = "macos")]
const APP_QUIT_ID: &str = "aubridge-app-quit";

enum ShutdownPhase {
    Serving,
    Draining(i32),
    Ready(Option<i32>),
}

#[derive(Debug, PartialEq, Eq)]
enum ExitAction {
    Allow,
    Prevent,
    BeginDrain,
}

impl ShutdownPhase {
    fn set_gateway_serving(&self, gateway: &GatewayState, on: bool) -> bool {
        if !matches!(self, Self::Serving) {
            return false;
        }
        if gateway.is_serving() != on {
            gateway.set_serving(on);
        }
        true
    }

    fn request_exit(&mut self, code: Option<i32>) -> ExitAction {
        let code = code.unwrap_or(0);
        if code == RESTART_EXIT_CODE {
            return ExitAction::Allow;
        }

        match *self {
            Self::Serving => {
                *self = Self::Draining(code);
                ExitAction::BeginDrain
            }
            Self::Ready(None) => ExitAction::Allow,
            Self::Ready(Some(saved)) if saved == code => ExitAction::Allow,
            Self::Draining(_) | Self::Ready(Some(_)) => ExitAction::Prevent,
        }
    }

    fn finish(&mut self) -> Option<i32> {
        let code = match *self {
            Self::Serving => None,
            Self::Draining(code) => Some(code),
            Self::Ready(_) => return None,
        };
        *self = Self::Ready(code);
        code
    }
}

struct ServingTray {
    status: MenuItem<tauri::Wry>,
    pause: MenuItem<tauri::Wry>,
    resume: MenuItem<tauri::Wry>,
    listener: Mutex<Option<async_runtime::JoinHandle<()>>>,
}

impl ServingTray {
    fn update(&self, serving: bool, phase: &ShutdownPhase) -> tauri::Result<()> {
        let active = matches!(phase, ShutdownPhase::Serving);
        let text = match phase {
            ShutdownPhase::Draining(_) => "网关正在退出",
            ShutdownPhase::Ready(_) => "网关已停止",
            ShutdownPhase::Serving if serving => "网关运行中",
            ShutdownPhase::Serving => "网关已暂停",
        };
        // Disable both first so a failed update cannot leave a stale action enabled.
        self.pause.set_enabled(false)?;
        self.resume.set_enabled(false)?;
        self.status.set_text(text)?;
        self.pause.set_enabled(active && serving)?;
        self.resume.set_enabled(active && !serving)?;
        Ok(())
    }

    fn stop_listener(&self) {
        if let Some(task) = self.listener.lock().unwrap().take() {
            task.abort();
        }
    }
}

fn refresh_serving_tray(app: &AppHandle) {
    if let Some(tray) = app.try_state::<ServingTray>() {
        let phase = app.state::<Mutex<ShutdownPhase>>();
        let phase = phase.lock().unwrap();
        let serving = app.state::<Arc<GatewayState>>().is_serving();
        if let Err(error) = tray.update(serving, &phase) {
            eprintln!("aubridge: 更新托盘网关状态失败: {error}");
        }
    }
}

fn change_serving_from_tray(app: &AppHandle, on: bool) {
    let phase = app.state::<Mutex<ShutdownPhase>>();
    let phase = phase.lock().unwrap();
    let gateway = app.state::<Arc<GatewayState>>();
    if !phase.set_gateway_serving(&gateway, on) {
        eprintln!("aubridge: 网关已开始退出，无法切换服务状态");
    }
    drop(phase);
    refresh_serving_tray(app);
}

#[tauri::command]
fn gateway_access(
    webview: WebviewWindow,
    state: State<'_, Arc<GatewayState>>,
) -> Result<GatewayAccess, String> {
    let url = webview.url().map_err(|e| e.to_string())?;
    let bundled = matches!(
        (url.scheme(), url.host_str()),
        ("tauri", Some("localhost")) | ("http" | "https", Some("tauri.localhost"))
    ) && url.port().is_none();
    if webview.label() != "main" || !bundled {
        return Err("网关管理凭据仅供本机 aubridge 窗口使用".into());
    }
    Ok(GatewayAccess::from_state(&state))
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn tray_icon() -> tauri::image::Image<'static> {
    // Compile-time pixels; macOS displays this Retina template at 18 points.
    tauri::include_image!("icons/tray-icon@2x.png")
}

fn install_tray(app: &App) -> tauri::Result<()> {
    let gateway = app.state::<Arc<GatewayState>>();
    let mut receiver = gateway.subscribe_serving();
    let serving = *receiver.borrow_and_update();
    let show = MenuItem::with_id(app, "aubridge-show", "打开 aubridge", true, None::<&str>)?;
    let status = MenuItem::with_id(app, "aubridge-status", "网关运行中", false, None::<&str>)?;
    let pause = MenuItem::with_id(
        app,
        "aubridge-pause",
        "暂停新推理请求",
        serving,
        None::<&str>,
    )?;
    let resume = MenuItem::with_id(
        app,
        "aubridge-resume",
        "恢复新推理请求",
        !serving,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "aubridge-quit", "退出 aubridge", true, None::<&str>)?;
    let control = ServingTray {
        status,
        pause,
        resume,
        listener: Mutex::new(None),
    };
    control.update(serving, &ShutdownPhase::Serving)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &control.status,
            &control.pause,
            &control.resume,
            &quit,
        ],
    )?;
    let tray = TrayIconBuilder::with_id(TRAY_ID)
        .icon(tray_icon())
        .icon_as_template(true)
        .menu(&menu)
        .tooltip("aubridge")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "aubridge-show" => show_main(app),
            "aubridge-quit" => app.exit(0),
            "aubridge-pause" => change_serving_from_tray(app, false),
            "aubridge-resume" => change_serving_from_tray(app, true),
            _ => {}
        });
    tray.build(app)?;
    app.manage(control);
    let app_handle = app.handle().clone();
    let task = async_runtime::spawn(async move {
        while receiver.changed().await.is_ok() {
            let app = app_handle.clone();
            if let Err(error) = app_handle.run_on_main_thread(move || {
                refresh_serving_tray(&app);
                let phase = app.state::<Mutex<ShutdownPhase>>();
                if matches!(*phase.lock().unwrap(), ShutdownPhase::Serving) {
                    let serving = app.state::<Arc<GatewayState>>().is_serving();
                    if let Err(error) = app.emit_to("main", "aubridge-serving-changed", serving) {
                        eprintln!("aubridge: 同步窗口网关状态失败: {error}");
                    }
                }
            }) {
                eprintln!("aubridge: 调度托盘网关状态更新失败: {error}");
                break;
            }
        }
    });
    *app.state::<ServingTray>().listener.lock().unwrap() = Some(task);
    Ok(())
}

#[cfg(target_os = "macos")]
fn desktop_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::default(app)?;
    let unexpected_menu = || std::io::Error::other("unexpected macOS default application menu");
    let items = menu.items()?;
    let app_menu = items
        .first()
        .and_then(|item| item.as_submenu())
        .ok_or_else(unexpected_menu)?;
    let app_items = app_menu.items()?;
    let original_quit = app_items
        .last()
        .and_then(|item| item.as_predefined_menuitem())
        .ok_or_else(unexpected_menu)?;
    let title = original_quit.text()?;
    // Muda's unattached reference item encodes literal ampersands as mnemonics.
    let expected = PredefinedMenuItem::quit(app, None)?.text()?;
    if title != expected.replace("&&", "&") {
        return Err(unexpected_menu().into());
    }

    // The predefined item invokes AppKit terminate: directly, bypassing ExitRequested.
    let quit = MenuItem::with_id(app, APP_QUIT_ID, expected, true, Some("Cmd+Q"))?;
    let position = app_items.len() - 1;
    app_menu.remove_at(position)?.ok_or_else(unexpected_menu)?;
    app_menu.insert(&quit, position)?;
    Ok(menu)
}

fn main() {
    let (shutdown_tx, mut shutdown_rx) = async_runtime::channel(1);
    let builder = tauri::Builder::default();
    #[cfg(target_os = "macos")]
    let builder = builder.menu(desktop_menu).on_menu_event(|app, event| {
        if event.id() == APP_QUIT_ID {
            app.exit(0);
        }
    });
    let app = builder
        .invoke_handler(tauri::generate_handler![gateway_access])
        .manage(Mutex::new(ShutdownPhase::Serving))
        .setup(move |app| {
            let runtime = async_runtime::block_on(Runtime::start(true))?;
            app.manage(runtime.state());
            // 会话文件(变更记录 34):桌面壳与 headless 同样发布,
            // 诊断 CLI 据此中继到本网关。持有至进程退出,Drop 时删除。
            match runtime.publish_session() {
                Ok(session) => {
                    app.manage(session);
                }
                Err(error) => {
                    eprintln!("aubridge: 发布管理会话文件失败: {error}");
                }
            }
            if let Err(error) = install_tray(app) {
                eprintln!("aubridge: 无法创建系统托盘: {error}");
            }
            let app = app.handle().clone();
            async_runtime::spawn(async move {
                let shutdown = async {
                    let _ = shutdown_rx.recv().await;
                };
                if let Err(error) = runtime.serve_until_shutdown(shutdown).await {
                    eprintln!("aubridge: 网关停止: {error}");
                }
                let code = app.state::<Mutex<ShutdownPhase>>().lock().unwrap().finish();
                if let Some(tray) = app.try_state::<ServingTray>() {
                    tray.stop_listener();
                }
                let update = app.clone();
                if let Err(error) = app.run_on_main_thread(move || refresh_serving_tray(&update)) {
                    eprintln!("aubridge: 更新网关结束状态失败: {error}");
                }
                if let Some(code) = code {
                    app.exit(code);
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main"
                    && window.app_handle().tray_by_id(TRAY_ID).is_some()
                    && window.hide().is_ok()
                {
                    api.prevent_close();
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("start aubridge desktop");
    app.run(move |app, event| match event {
        RunEvent::ExitRequested { code, api, .. } => {
            let phase = app.state::<Mutex<ShutdownPhase>>();
            let action = phase.lock().unwrap().request_exit(code);
            match action {
                ExitAction::Allow => {}
                ExitAction::Prevent => api.prevent_exit(),
                ExitAction::BeginDrain => {
                    api.prevent_exit();
                    if let Some(tray) = app.try_state::<ServingTray>() {
                        tray.stop_listener();
                    }
                    refresh_serving_tray(app);
                    // Only the first request sends. If the receiver already closed,
                    // the server task is finishing and will still re-request exit.
                    let _ = shutdown_tx.try_send(());
                }
            }
        }
        RunEvent::Exit => {
            if let Some(tray) = app.try_state::<ServingTray>() {
                tray.stop_listener();
            }
            app.state::<Arc<GatewayState>>().flush_router_state();
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => show_main(app),
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::{ExitAction, ShutdownPhase, RESTART_EXIT_CODE};

    #[test]
    fn desktop_shutdown_rejects_serving_changes_during_drain_and_after_finish() {
        let gateway = gateway::state::GatewayState::from_manifests(Default::default(), "test");
        let mut phase = ShutdownPhase::Serving;
        assert!(phase.set_gateway_serving(&gateway, false));
        assert!(!gateway.is_serving());
        assert!(phase.set_gateway_serving(&gateway, false));
        assert_eq!(phase.request_exit(Some(0)), ExitAction::BeginDrain);
        assert!(!phase.set_gateway_serving(&gateway, true));
        assert!(!gateway.is_serving());
        phase.finish();
        assert!(!phase.set_gateway_serving(&gateway, true));
        assert!(!gateway.is_serving());
    }

    #[test]
    fn desktop_shutdown_preserves_first_exit_code() {
        let mut phase = ShutdownPhase::Serving;
        assert_eq!(phase.request_exit(Some(7)), ExitAction::BeginDrain);
        assert_eq!(phase.request_exit(None), ExitAction::Prevent);
        assert_eq!(phase.request_exit(Some(7)), ExitAction::Prevent);
        assert_eq!(phase.finish(), Some(7));
        assert_eq!(phase.request_exit(Some(8)), ExitAction::Prevent);
        assert_eq!(phase.request_exit(Some(7)), ExitAction::Allow);

        let mut phase = ShutdownPhase::Serving;
        assert_eq!(phase.request_exit(None), ExitAction::BeginDrain);
        assert_eq!(phase.finish(), Some(0));
        assert_eq!(phase.request_exit(Some(0)), ExitAction::Allow);
    }

    #[test]
    fn desktop_shutdown_allows_exit_after_early_completion() {
        let mut phase = ShutdownPhase::Serving;
        assert_eq!(phase.finish(), None);
        assert_eq!(phase.request_exit(Some(7)), ExitAction::Allow);
        assert_eq!(phase.request_exit(None), ExitAction::Allow);
    }

    #[test]
    fn desktop_shutdown_leaves_restart_unintercepted() {
        let mut phase = ShutdownPhase::Serving;
        assert_eq!(
            phase.request_exit(Some(RESTART_EXIT_CODE)),
            ExitAction::Allow
        );
        assert_eq!(phase.request_exit(Some(7)), ExitAction::BeginDrain);
        assert_eq!(
            phase.request_exit(Some(RESTART_EXIT_CODE)),
            ExitAction::Allow
        );
        assert_eq!(phase.finish(), Some(7));
    }
}
