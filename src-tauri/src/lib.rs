mod config;
mod discord;
mod gesture;
mod input;

use config::{Config, ConfigStore};
use discord::{BridgeStatus, DiscordCommand};
use input::{CaptureResult, InputMonitor};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
};
use tauri::{
    AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use windows::{
    Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    core::{HSTRING, PCWSTR},
};

struct AppState {
    store: Arc<ConfigStore>,
    input: Arc<InputMonitor>,
    discord_tx: mpsc::Sender<DiscordCommand>,
    status: Arc<Mutex<BridgeStatus>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SaveResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<Config>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[tauri::command]
fn get_config(state: State<'_, AppState>) -> Config {
    state.store.get().public()
}

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> BridgeStatus {
    state
        .status
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default()
}

#[tauri::command]
fn get_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
fn save_config(app: AppHandle, state: State<'_, AppState>, config: Config) -> SaveResult {
    match state.store.replace_public(config) {
        Ok(saved) => {
            set_autostart(&app, saved.launch_at_login);
            let _ = state.discord_tx.send(DiscordCommand::RefreshActive);
            SaveResult {
                ok: true,
                config: Some(saved),
                error: None,
            }
        }
        Err(error) => SaveResult {
            ok: false,
            config: None,
            error: Some(error),
        },
    }
}

#[tauri::command]
fn reconnect_discord(state: State<'_, AppState>) {
    let _ = state.discord_tx.send(DiscordCommand::Connect);
}

#[tauri::command]
async fn capture_shortcut(state: State<'_, AppState>) -> Result<CaptureResult, String> {
    let input = state.input.clone();
    tauri::async_runtime::spawn_blocking(move || input.capture())
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") {
        return Err("Only HTTPS links are allowed".into());
    }
    let operation = HSTRING::from("open");
    let target = HSTRING::from(url);
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(target.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err("Windows could not open the URL".into())
    }
}

fn set_autostart(app: &AppHandle, enabled: bool) {
    let autostart = app.autolaunch();
    let _ = if enabled {
        autostart.enable()
    } else {
        autostart.disable()
    };
}

fn show_settings(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window("settings") {
        window.show()?;
        window.set_focus()?;
        return Ok(());
    }
    WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("index.html".into()))
        .title("Willow Discord Bridge")
        .inner_size(600.0, 760.0)
        .min_inner_size(520.0, 620.0)
        .resizable(true)
        .build()?;
    // Let the settings window be destroyed when closed. Recreating this tiny UI
    // on the next tray click releases every WebView2 process while tray-idle.
    Ok(())
}

fn config_path(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("config.json")
}

fn should_prevent_exit(code: Option<i32>) -> bool {
    code.is_none()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            let _ = show_settings(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .setup(|app| {
            let handle = app.handle().clone();
            let store = Arc::new(ConfigStore::load(config_path(&handle)));
            let status = Arc::new(Mutex::new(BridgeStatus::default()));
            let discord_tx = discord::start_worker(store.clone(), status.clone());
            let (input_tx, input_rx) = mpsc::channel();
            let input = Arc::new(InputMonitor::start(store.clone(), input_tx)?);
            if let Ok(mut current) = status.lock() {
                current.engine_ready = true;
            }
            gesture::start_gesture_worker(
                input_rx,
                discord_tx.clone(),
                store.clone(),
                status.clone(),
            );

            app.manage(AppState {
                store: store.clone(),
                input,
                discord_tx: discord_tx.clone(),
                status,
            });
            set_autostart(&handle, store.get().launch_at_login);
            if !store.get().discord_rpc.client_id.is_empty() {
                let _ = discord_tx.send(DiscordCommand::Connect);
            }

            let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&settings, &quit])?;
            let tray = TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().expect("app icon").clone())
                .tooltip("Willow Discord Bridge")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "settings" => {
                        let _ = show_settings(app);
                    }
                    "quit" => {
                        if let Some(state) = app.try_state::<AppState>() {
                            let _ = state.discord_tx.send(DiscordCommand::Shutdown);
                        }
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        let _ = show_settings(tray.app_handle());
                    }
                })
                .build(app)?;
            std::mem::forget(tray);

            if !std::env::args().any(|arg| arg == "--hidden") {
                show_settings(&handle)?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_status,
            get_version,
            save_config,
            reconnect_discord,
            capture_shortcut,
            open_url,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Willow Discord Bridge");

    app.run(|_, event| {
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            // Destroying the settings webview must leave the tray bridge alive.
            // Explicit exits (the tray Quit item) carry a code and are allowed.
            if should_prevent_exit(code) {
                api.prevent_exit();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::should_prevent_exit;

    #[test]
    fn closing_the_last_window_keeps_the_tray_process_alive() {
        assert!(should_prevent_exit(None));
    }

    #[test]
    fn explicit_quit_is_allowed_to_exit() {
        assert!(!should_prevent_exit(Some(0)));
    }
}
