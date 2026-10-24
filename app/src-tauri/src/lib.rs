mod ble;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{Central, CentralEvent, Peripheral as _};
use btleplug::platform::{Adapter, Peripheral};
use futures::StreamExt;
use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_positioner::{Position, WindowExt};
use tokio::sync::{Mutex, Notify};

/// Фаза соединения для UI.
#[derive(Clone, Copy, Serialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
enum Phase {
    /// Автоподключение выключено пользователем («отдать телефону»).
    Paused,
    #[default]
    Searching,
    Connected,
    /// Не нашли — ждём следующей попытки.
    Waiting,
}

#[derive(Clone, Serialize, Default)]
struct Status {
    phase: Phase,
    info: Option<ble::DeviceInfo>,
    battery: Option<ble::Battery>,
    state: Option<ble::ProgramState>,
    error: Option<String>,
    /// Через сколько секунд следующая попытка (для Waiting).
    retry_in: Option<u64>,
}

#[derive(Default)]
struct Inner {
    status: Status,
    peripheral: Option<Peripheral>,
    auto: bool,
}

struct Cube {
    inner: Mutex<Inner>,
    /// Будит фоновый цикл: смена auto, ручное «подключить», обрыв.
    kick: Notify,
}

type CubeState<'a> = State<'a, Arc<Cube>>;

fn last_id_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("last_cube_id"))
}

fn paused_flag_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("paused"))
}

async fn publish(app: &AppHandle, cube: &Cube, f: impl FnOnce(&mut Status)) {
    let status = {
        let mut inner = cube.inner.lock().await;
        f(&mut inner.status);
        inner.status.clone()
    };
    if let Some(tray) = app.tray_by_id("cube") {
        let _ = tray.set_tooltip(Some(tooltip(&status)));
    }
    let _ = app.emit("cube-status", status);
}

fn tooltip(s: &Status) -> String {
    let conn = match s.phase {
        Phase::Connected => "подключён",
        Phase::Searching => "ищу кубик…",
        Phase::Waiting => "не в сети",
        Phase::Paused => "отключён",
    };
    let mut t = format!("Cube Remote — {conn}");
    if let Some(st) = s.state.as_ref().filter(|st| st.state == "active") {
        let left = st.a.saturating_sub(st.b);
        t += &format!(" · играет #{}, осталось {} ч {:02} мин", st.program_id, left / 3600, left % 3600 / 60);
    }
    if let Some(level) = s.battery.as_ref().and_then(|b| b.level) {
        t += &format!(" · {level}%");
    }
    t
}

/// Слушает notify состояния, пока соединение живо. Поток заканчивается при обрыве.
fn spawn_notifications(app: AppHandle, cube: Arc<Cube>, p: Peripheral) {
    tauri::async_runtime::spawn(async move {
        let Ok(mut stream) = p.notifications().await else { return };
        while let Some(n) = stream.next().await {
            if n.uuid == ble::CHR_STATE {
                let st = ble::parse_state(&n.value);
                publish(&app, &cube, |s| s.state = Some(st)).await;
            }
        }
        cube.kick.notify_one();
    });
}

async fn refresh(app: &AppHandle, cube: &Cube, p: &Peripheral) {
    let battery = ble::battery(p).await;
    let state = ble::read_state(p).await.ok();
    publish(app, cube, |s| {
        s.battery = Some(battery);
        if state.is_some() {
            s.state = state;
        }
    })
    .await;
}

async fn try_connect(app: &AppHandle, cube: &Arc<Cube>, adapter: &Adapter) -> Result<Peripheral, String> {
    let preferred = last_id_path(app).and_then(|f| std::fs::read_to_string(f).ok());
    let p = ble::find_cube(adapter, preferred.as_deref(), Duration::from_secs(10)).await?;
    ble::ensure_connected(&p).await?;
    if let Some(f) = last_id_path(app) {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = std::fs::write(f, p.id().to_string());
    }
    let _ = ble::subscribe_state(&p).await;
    spawn_notifications(app.clone(), cube.clone(), p.clone());
    let info = ble::device_info(&p).await;
    cube.inner.lock().await.peripheral = Some(p.clone());
    publish(app, cube, |s| {
        s.phase = Phase::Connected;
        s.info = Some(info);
        s.error = None;
        s.retry_in = None;
    })
    .await;
    refresh(app, cube, &p).await;
    Ok(p)
}

/// Фоновый цикл: держит соединение, переподключается с бэкоффом 5 → 60 с.
async fn connection_manager(app: AppHandle, cube: Arc<Cube>) {
    let adapter = loop {
        match ble::adapter().await {
            Ok(a) => break a,
            Err(e) => {
                publish(&app, &cube, |s| { s.phase = Phase::Waiting; s.error = Some(e); }).await;
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    };
    let mut events = match adapter.events().await {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut backoff = 5u64;

    loop {
        let (auto, current) = {
            let inner = cube.inner.lock().await;
            (inner.auto, inner.peripheral.clone())
        };

        if !auto {
            if let Some(p) = current {
                let _ = p.disconnect().await;
                cube.inner.lock().await.peripheral = None;
            }
            publish(&app, &cube, |s| { s.phase = Phase::Paused; s.state = None; s.retry_in = None; }).await;
            cube.kick.notified().await;
            continue;
        }

        match current {
            Some(p) if p.is_connected().await.unwrap_or(false) => {
                backoff = 5;
                // Ждём обрыва / пинка / минуты на обновление заряда.
                let id = p.id();
                tokio::select! {
                    _ = cube.kick.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(60)) => refresh(&app, &cube, &p).await,
                    _ = async {
                        while let Some(ev) = events.next().await {
                            if matches!(ev, CentralEvent::DeviceDisconnected(ref d) if *d == id) { break; }
                        }
                    } => {}
                }
            }
            _ => {
                cube.inner.lock().await.peripheral = None;
                publish(&app, &cube, |s| { s.phase = Phase::Searching; s.retry_in = None; }).await;
                match try_connect(&app, &cube, &adapter).await {
                    Ok(_) => backoff = 5,
                    Err(e) => {
                        publish(&app, &cube, |s| {
                            s.phase = Phase::Waiting;
                            s.error = Some(e);
                            s.retry_in = Some(backoff);
                            s.state = None;
                        })
                        .await;
                        tokio::select! {
                            _ = cube.kick.notified() => {}
                            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                        }
                        backoff = (backoff * 2).min(60);
                    }
                }
            }
        }
    }
}

async fn connected_peripheral(cube: &Cube) -> Result<Peripheral, String> {
    let p = cube.inner.lock().await.peripheral.clone().ok_or("Кубик не подключён")?;
    ble::ensure_connected(&p).await?;
    Ok(p)
}

#[tauri::command]
async fn status(cube: CubeState<'_>) -> Result<Status, String> {
    Ok(cube.inner.lock().await.status.clone())
}

/// on = true — подключиться сейчас и держать; false — отпустить кубик (для телефона).
#[tauri::command]
async fn set_auto(app: AppHandle, cube: CubeState<'_>, on: bool) -> Result<(), String> {
    cube.inner.lock().await.auto = on;
    if let Some(f) = paused_flag_path(&app) {
        let _ = std::fs::create_dir_all(f.parent().unwrap());
        let _ = if on { std::fs::remove_file(f) } else { std::fs::write(f, "1") };
    }
    cube.kick.notify_one();
    Ok(())
}

#[tauri::command]
async fn play(
    app: AppHandle,
    cube: CubeState<'_>,
    program_id: u8,
    power: u8,
    side: u8,
    seconds: u32,
) -> Result<(), String> {
    if program_id > 99 {
        return Err("Медитации (ID > 99) требуют аудио с телефона".into());
    }
    if !(ble::MIN_SECONDS..=ble::MAX_SECONDS).contains(&seconds) {
        return Err("Длительность должна быть от 1 до 24 часов".into());
    }
    let p = connected_peripheral(&cube).await?;
    // Как в штатном приложении: если что-то играет — сначала stop, пауза 50 мс, потом play.
    if let Ok(s) = ble::read_state(&p).await {
        if s.state == "active" || s.state == "paused" {
            ble::write_control(&p, ble::control_packet(ble::CMD_STOP, 0, 0, 0, 0)).await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    ble::write_control(&p, ble::control_packet(ble::CMD_PLAY, program_id, power, side, seconds)).await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    refresh(&app, &cube, &p).await;
    Ok(())
}

async fn simple_command(app: &AppHandle, cube: &Cube, cmd: u8) -> Result<(), String> {
    let p = connected_peripheral(cube).await?;
    ble::write_control(&p, ble::control_packet(cmd, 0, 0, 0, 0)).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    refresh(app, cube, &p).await;
    Ok(())
}

#[tauri::command]
async fn stop(app: AppHandle, cube: CubeState<'_>) -> Result<(), String> {
    simple_command(&app, &cube, ble::CMD_STOP).await
}

#[tauri::command]
async fn pause(app: AppHandle, cube: CubeState<'_>) -> Result<(), String> {
    simple_command(&app, &cube, ble::CMD_PAUSE).await
}

fn toggle_window(app: &AppHandle) {
    let Some(w) = app.get_webview_window("main") else { return };
    if w.is_visible().unwrap_or(false) {
        let _ = w.hide();
    } else {
        let _ = w.move_window(Position::TrayBottomCenter);
        let _ = w.show();
        let _ = w.set_focus();
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_positioner::init())
        .invoke_handler(tauri::generate_handler![status, set_auto, play, stop, pause])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let paused = paused_flag_path(app.handle()).is_some_and(|f| f.exists());
            let cube = Arc::new(Cube {
                inner: Mutex::new(Inner { auto: !paused, ..Default::default() }),
                kick: Notify::new(),
            });
            app.manage(cube.clone());
            tauri::async_runtime::spawn(connection_manager(app.handle().clone(), cube));

            let release = MenuItem::with_id(app, "release", "Отдать кубик телефону", true, None::<&str>)?;
            let connect = MenuItem::with_id(app, "connect", "Подключить", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Выйти", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&connect, &release, &quit])?;
            TrayIconBuilder::with_id("cube")
                .icon(Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .icon_as_template(true)
                .tooltip("Cube Remote")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| {
                    let on = match e.id().as_ref() {
                        "quit" => return app.exit(0),
                        "release" => false,
                        "connect" => true,
                        _ => return,
                    };
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        let cube = app.state::<Arc<Cube>>();
                        let _ = set_auto(app.clone(), cube, on).await;
                    });
                })
                .on_tray_icon_event(|tray, event| {
                    tauri_plugin_positioner::on_tray_event(tray.app_handle(), &event);
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::Focused(false) = event {
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
