#![recursion_limit = "256"]

mod cats;
mod config;
mod input_hook;
mod keymap;
#[cfg(target_os = "macos")]
mod keymap_macos;
mod mcp;
mod tray;
#[cfg(all(windows, feature = "win7"))]
mod win7;
#[cfg(windows)]
mod windows_zorder;

use config::{
    config_path, load_settings, save_settings, set_autostart, try_load_settings, AppSettings,
    MAX_SCALE, MIN_SCALE,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{
    AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, Size, State, WebviewWindow,
};

pub const BASE_WIDTH: f64 = 660.0;
pub const BASE_HEIGHT: f64 = 480.0;
pub const BASE_WIDTH_104: f64 = 740.0;
pub const BASE_HEIGHT_104: f64 = 520.0;
pub const SETTINGS_WIDTH: f64 = 244.0;
pub const SETTINGS_HEIGHT: f64 = 516.0;
/// Safety cap for the server-side modal guard: even if the webview never
/// reports the picker closing, outside clicks close the window again after this.
const SETTINGS_MODAL_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const BROWSER_ARGS: &str = if cfg!(all(windows, feature = "win7")) {
    "--autoplay-policy=no-user-gesture-required --disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling --disable-features=CalculateNativeWinOcclusion"
} else {
    "--autoplay-policy=no-user-gesture-required"
};

#[cfg(all(windows, feature = "win7"))]
fn configure_win7_runtime() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let runtime = dir.join("WebView2Runtime");
    if runtime.join("msedgewebview2.exe").is_file() {
        std::env::set_var("WEBVIEW2_BROWSER_EXECUTABLE_FOLDER", runtime);
    }
    if std::env::var_os("WEBVIEW2_USER_DATA_FOLDER").is_none() {
        let profile = std::env::var_os("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| dir.to_path_buf())
            .join("7paws")
            .join("EBWebView-Win7");
        let _ = std::fs::create_dir_all(&profile);
        std::env::set_var("WEBVIEW2_USER_DATA_FOLDER", profile);
    }
    if std::env::var_os("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").is_none() {
        std::env::set_var(
            "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
            BROWSER_ARGS,
        );
    }
}

pub struct AppState {
    settings: Arc<Mutex<AppSettings>>,
    custom_cats: Arc<Mutex<Vec<cats::CustomCat>>>,
    dragging: Arc<AtomicBool>,
    hit_regions: Arc<Mutex<Vec<[f64; 4]>>>,
    /// Epoch millis until which the settings window hosts a native dialog
    /// (file picker) and must not be auto-closed by the global mouse hook.
    settings_modal_until: Arc<AtomicU64>,
}

#[cfg(windows)]
struct SingleInstanceGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn acquire_single_instance(retry_until_available: bool) -> Option<SingleInstanceGuard> {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE,
    };
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = "Local\\com.qoder.7paws.SingleInstance\0"
        .encode_utf16()
        .collect();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        unsafe {
            SetLastError(0);
        }
        let handle: HANDLE = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
        if handle.is_null() {
            eprintln!("failed to create the 7paws single-instance mutex");
            return None;
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(handle);
            }
            if !retry_until_available || Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        return Some(SingleInstanceGuard(handle));
    }
}

fn base_dimensions(layout: &str) -> (f64, f64) {
    if layout == "104" {
        (BASE_WIDTH_104, BASE_HEIGHT_104)
    } else {
        (BASE_WIDTH, BASE_HEIGHT)
    }
}

fn window_dimensions(layout: &str, scale: f64) -> (f64, f64) {
    let s = scale.clamp(MIN_SCALE, MAX_SCALE);
    let (w, h) = base_dimensions(layout);
    ((w * s).round(), (h * s).round())
}

// 65 layout, right-handed; the frontend reports the active regions right after boot.
fn default_regions() -> Vec<[f64; 4]> {
    vec![
        [0.392, 0.160, 0.216, 0.309],
        [0.369, 0.231, 0.262, 0.499],
        [0.508, 0.474, 0.227, 0.253],
        [0.325, 0.680, 0.428, 0.169],
        [0.248, 0.667, 0.113, 0.145],
    ]
}

#[cfg(all(windows, feature = "win7"))]
fn main_window_rect(_app: &AppHandle) -> Option<(i32, i32, i32, i32)> {
    win7::get_rect()
}

#[cfg(not(all(windows, feature = "win7")))]
fn main_window_rect(app: &AppHandle) -> Option<(i32, i32, i32, i32)> {
    let win = app.get_webview_window("main")?;
    let pos = win.outer_position().ok()?;
    let size = win.outer_size().ok()?;
    Some((pos.x, pos.y, size.width as i32, size.height as i32))
}

pub(crate) fn main_window_is_visible(app: &AppHandle) -> bool {
    #[cfg(all(windows, feature = "win7"))]
    {
        let _ = app;
        win7::is_visible()
    }
    #[cfg(not(all(windows, feature = "win7")))]
    {
        app.get_webview_window("main")
            .and_then(|win| win.is_visible().ok())
            .unwrap_or(true)
    }
}

pub(crate) fn set_main_window_visible(app: &AppHandle, visible: bool) {
    #[cfg(all(windows, feature = "win7"))]
    {
        let _ = app;
        win7::set_visible(visible);
    }
    #[cfg(not(all(windows, feature = "win7")))]
    {
        if let Some(win) = app.get_webview_window("main") {
            if visible {
                let _ = win.show();
            } else {
                let _ = win.hide();
            }
        }
    }
}

fn resize_anchored_bottom_center(
    win: &WebviewWindow,
    layout: &str,
    scale: f64,
) -> Option<(i32, i32)> {
    let (new_w, new_h) = window_dimensions(layout, scale);
    #[cfg(all(windows, feature = "win7"))]
    let (old_pos, old_size) = win7::get_rect()
        .map(|(x, y, w, h)| {
            (
                Some(PhysicalPosition::new(x, y)),
                Some(tauri::PhysicalSize::new(w as u32, h as u32)),
            )
        })
        .unwrap_or((None, None));
    #[cfg(not(all(windows, feature = "win7")))]
    let (old_pos, old_size) = (win.outer_position().ok(), win.outer_size().ok());
    let sf = win.scale_factor().unwrap_or(1.0);

    let _ = win.set_size(Size::Logical(LogicalSize::new(new_w, new_h)));

    let mut anchored = None;
    if let (Some(pos), Some(sz)) = (old_pos, old_size) {
        let new_pw = (new_w * sf).round() as i32;
        let new_ph = (new_h * sf).round() as i32;
        let mut new_x = pos.x + (sz.width as i32 - new_pw) / 2;
        let mut new_y = pos.y + sz.height as i32 - new_ph;
        if let Ok(Some(monitor)) = win.current_monitor() {
            let mx = monitor.position().x;
            let my = monitor.position().y;
            let mw = monitor.size().width as i32;
            let mh = monitor.size().height as i32;
            new_x = new_x.clamp(mx, (mx + mw - new_pw).max(mx));
            new_y = new_y.clamp(my, (my + mh - new_ph).max(my));
        }
        #[cfg(not(all(windows, feature = "win7")))]
        let _ = win.set_position(PhysicalPosition::new(new_x, new_y));
        anchored = Some((new_x, new_y));
    }
    #[cfg(all(windows, feature = "win7"))]
    {
        let physical_w = (new_w * sf).round() as u32;
        let physical_h = (new_h * sf).round() as u32;
        win7::set_size(physical_w, physical_h);
        if let Some((x, y)) = anchored {
            win7::set_position(x, y);
        }
    }
    anchored
}

fn bottom_right_position(app: &AppHandle, layout: &str, scale: f64, sf: f64) -> PhysicalPosition<i32> {
    let (lw, lh) = window_dimensions(layout, scale);
    let w = (lw * sf).round() as i32;
    let h = (lh * sf).round() as i32;
    let pad_x = (28.0 * sf).round() as i32;
    let pad_y = (64.0 * sf).round() as i32;

    if let Ok(Some(monitor)) = app.primary_monitor() {
        let size = monitor.size();
        let pos = monitor.position();
        return PhysicalPosition::new(
            pos.x + size.width as i32 - w - pad_x,
            pos.y + size.height as i32 - h - pad_y,
        );
    }
    PhysicalPosition::new(pad_x, pad_y)
}

fn clamp_to_primary(app: &AppHandle, x: i32, y: i32, w: i32, h: i32) -> PhysicalPosition<i32> {
    if let Ok(Some(monitor)) = app.primary_monitor() {
        let mx = monitor.position().x;
        let my = monitor.position().y;
        let mw = monitor.size().width as i32;
        let mh = monitor.size().height as i32;
        return PhysicalPosition::new(
            x.clamp(mx, (mx + mw - w).max(mx)),
            y.clamp(my, (my + mh - h).max(my)),
        );
    }
    PhysicalPosition::new(x, y)
}

#[cfg(windows)]
fn cursor_pos() -> Option<(i32, i32)> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) != 0 {
            Some((pt.x, pt.y))
        } else {
            None
        }
    }
}

#[cfg(windows)]
fn cursor_pos_for(_win: &WebviewWindow) -> Option<(f64, f64)> {
    cursor_pos().map(|(x, y)| (x as f64, y as f64))
}

// NSEvent reports points; Tauri converts to the physical pixels used by the
// window position and size, so hit-test ratios match the Windows path.
#[cfg(target_os = "macos")]
fn cursor_pos_for(win: &WebviewWindow) -> Option<(f64, f64)> {
    win.cursor_position().ok().map(|p| (p.x, p.y))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn cursor_pos_for(_win: &WebviewWindow) -> Option<(f64, f64)> {
    None
}

// Is the primary mouse button physically held right now? VK_LBUTTON tracks the
// physical button (so it also works when the OS swaps buttons for a left-handed
// mouse). A native window drag (startDragging → WM_NCLBUTTONDOWN/HTCAPTION) runs a
// modal move loop while the button stays down; we use this to keep the click-through
// poll from changing state underneath it.
#[cfg(windows)]
fn primary_mouse_button_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    unsafe { (GetAsyncKeyState(VK_LBUTTON as i32) as i16) < 0 }
}

#[cfg(not(windows))]
fn primary_mouse_button_down() -> bool {
    false
}

fn start_hit_test_loop(
    app: AppHandle,
    settings: Arc<Mutex<AppSettings>>,
    dragging: Arc<AtomicBool>,
    regions: Arc<Mutex<Vec<[f64; 4]>>>,
) {
    thread::spawn(move || {
        let mut last_ignore: Option<bool> = None;
        let mut sleep_ms: u64 = 25;

        loop {
            thread::sleep(Duration::from_millis(sleep_ms));

            let Some(win) = app.get_webview_window("main") else {
                sleep_ms = 120;
                continue;
            };
            if !main_window_is_visible(&app) {
                sleep_ms = 120;
                continue;
            }
            // While the primary mouse button is held (a pet drag in progress, or any drag
            // passing over the pet) do NOT recompute the click-through state. If the poll
            // flips WS_EX_TRANSPARENT mid-drag the window is pulled out from under the
            // cursor and the native move loop is cancelled, so the window never moves.
            // This is the classic trigger when dragging across a mixed-DPI monitor seam:
            // outer_position / scale can read inconsistently for a frame, making `inside`
            // look false and the window suddenly turn click-through.
            if dragging.load(Ordering::Relaxed) || primary_mouse_button_down() {
                sleep_ms = 20;
                continue;
            }
            let Some((cx, cy)) = cursor_pos_for(&win) else {
                sleep_ms = 50;
                continue;
            };
            let Some((px, py, pw, ph)) = main_window_rect(&app) else {
                sleep_ms = 50;
                continue;
            };

            let w = pw.max(1) as f64;
            let h = ph.max(1) as f64;
            let rx = (cx - px as f64) / w;
            let ry = (cy - py as f64) / h;
            let inside = rx >= 0.0 && rx <= 1.0 && ry >= 0.0 && ry <= 1.0;

            let locked = settings
                .lock()
                .map(|s| s.click_through_locked)
                .unwrap_or(false);
            let effective_locked = locked && !input_hook::is_alt_pressed();

            let over = if effective_locked || !inside {
                false
            } else {
                regions
                    .lock()
                    .map(|list| {
                        list.iter().any(|[x, y, rw, rh]| {
                            rx >= *x && rx <= x + rw && ry >= *y && ry <= y + rh
                        })
                    })
                    .unwrap_or(true)
            };

            let should_ignore = !over;
            if last_ignore != Some(should_ignore) {
                #[cfg(all(windows, feature = "win7"))]
                {
                    win7::set_click_through(should_ignore);
                }
                #[cfg(not(all(windows, feature = "win7")))]
                {
                    let _ = win.set_ignore_cursor_events(should_ignore);
                }
                last_ignore = Some(should_ignore);
            }

            // Poll fast only while the cursor is at or near the cat; a resting cursor
            // elsewhere does not need 40 wakeups per second.
            let near = rx >= -0.15 && rx <= 1.15 && ry >= -0.15 && ry <= 1.15;
            sleep_ms = if near { 25 } else { 50 };
        }
    });
}

fn start_position_watcher(app: AppHandle, settings: Arc<Mutex<AppSettings>>) {
    thread::spawn(move || {
        let mut last: Option<(i32, i32)> = None;
        loop {
            thread::sleep(Duration::from_millis(700));
            let Some(_win) = app.get_webview_window("main") else {
                continue;
            };
            if !main_window_is_visible(&app) {
                continue;
            }
            let Some((x, y, _, _)) = main_window_rect(&app) else {
                continue;
            };
            if last == Some((x, y)) {
                continue;
            }
            last = Some((x, y));
            let mut s = settings.lock().unwrap();
            if s.x != Some(x) || s.y != Some(y) {
                s.x = Some(x);
                s.y = Some(y);
                save_settings(&s);
            }
        }
    });
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> AppSettings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn update_settings(
    app: AppHandle,
    state: State<'_, AppState>,
    mut patch: AppSettings,
) -> AppSettings {
    let mut s = state.settings.lock().unwrap();
    let prev_scale = s.scale;
    let prev_layout = s.keyboard_layout.clone();
    let prev_autostart = s.autostart;

    patch.normalize();
    s.scale = patch.scale;
    s.opacity = patch.opacity;
    s.always_on_top = patch.always_on_top;
    s.click_through_locked = patch.click_through_locked;
    s.purr_pet = patch.purr_pet;
    s.purr_sleep = patch.purr_sleep;
    s.autostart = patch.autostart;
    s.privacy_mode = patch.privacy_mode;
    s.left_mouse = patch.left_mouse;
    s.coat = patch.coat;
    if !cats::is_builtin_coat(&s.coat) {
        let known = state
            .custom_cats
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.id == s.coat);
        if !known {
            s.coat = "tuxedo".to_string();
        }
    }
    s.sleep_minutes = patch.sleep_minutes;
    s.idle_actions = patch.idle_actions;
    s.show_key_labels = patch.show_key_labels;
    s.keyboard_layout = patch.keyboard_layout;
    s.show_tray_icon = patch.show_tray_icon;
    s.mcp_enabled = patch.mcp_enabled;
    s.run_as_admin = patch.run_as_admin;
    s.reminder_enabled = patch.reminder_enabled;
    s.reminder_text = patch.reminder_text;
    s.reminder_interval_minutes = patch.reminder_interval_minutes;

    if prev_autostart != s.autostart {
        let _ = set_autostart(s.autostart);
    }

    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_always_on_top(s.always_on_top);
        #[cfg(all(windows, feature = "win7"))]
        win7::set_always_on_top(s.always_on_top);
        if (s.scale - prev_scale).abs() > 0.0005 || s.keyboard_layout != prev_layout {
            if let Some((x, y)) = resize_anchored_bottom_center(&win, &s.keyboard_layout, s.scale) {
                s.x = Some(x);
                s.y = Some(y);
            }
        }
    }

    // Never leave the app with both the pet and the tray hidden.
    if !s.show_tray_icon {
        if !main_window_is_visible(&app) {
            set_main_window_visible(&app, true);
        }
    }

    save_settings(&s);
    let out = s.clone();
    drop(s);

    let _ = app.emit("cat-settings", out.clone());
    out
}

#[tauri::command]
fn set_dragging(state: State<'_, AppState>, dragging: bool) {
    state.dragging.store(dragging, Ordering::Relaxed);
}

fn place_settings_window(app: &AppHandle, win: &WebviewWindow) {
    let Some((pet_x, pet_y, pet_w, pet_h)) = main_window_rect(app) else {
        return;
    };
    let sf = app
        .get_webview_window("main")
        .and_then(|pet| pet.scale_factor().ok())
        .unwrap_or(1.0);
    let (sw, sh) = win
        .outer_size()
        .map(|size| (size.width as i32, size.height as i32))
        .unwrap_or_else(|_| {
            (
                (SETTINGS_WIDTH * sf).round() as i32,
                (SETTINGS_HEIGHT * sf).round() as i32,
            )
        });
    let gap = (4.0 * sf).round() as i32;
    let monitor = app
        .get_webview_window("main")
        .and_then(|pet| pet.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());
    let (ax, ay, aw, ah) = if let Some(m) = monitor {
        let wa = m.work_area();
        (
            wa.position.x,
            wa.position.y,
            wa.size.width as i32,
            wa.size.height as i32,
        )
    } else {
        (0, 0, 1920, 1080)
    };
    // Anchor to the visible cat/keyboard/mouse bounds instead of the window edge:
    // both layouts leave a wide transparent margin around the drawing.
    let regions = app
        .state::<AppState>()
        .hit_regions
        .lock()
        .map(|list| list.clone())
        .unwrap_or_default();
    let (nx0, nx1, ny1) = if regions.is_empty() {
        (0.0, 1.0, 1.0)
    } else {
        let mut nx0 = 1.0f64;
        let mut nx1 = 0.0f64;
        let mut ny1 = 0.0f64;
        for [x, y, w, h] in &regions {
            nx0 = nx0.min(*x);
            nx1 = nx1.max(x + w);
            ny1 = ny1.max(y + h);
        }
        (nx0, nx1, ny1)
    };
    let pet_w = pet_w as f64;
    let pet_h = pet_h as f64;
    let content_left = pet_x as f64 + nx0 * pet_w;
    let content_right = pet_x as f64 + nx1 * pet_w;
    let content_bottom = pet_y as f64 + ny1 * pet_h;

    let mut x = content_right + gap as f64;
    if x + sw as f64 > (ax + aw) as f64 {
        x = content_left - gap as f64 - sw as f64;
    }
    let x = (x.round() as i32).clamp(ax, (ax + aw - sw).max(ax));
    let y = ((content_bottom - sh as f64).round() as i32).clamp(ay, (ay + ah - sh).max(ay));
    #[cfg(all(windows, feature = "win7"))]
    win7::debug_log(format!(
        "settings:place pet={pet_x},{pet_y},{pet_w},{pet_h} size={sw}x{sh} work={ax},{ay},{aw},{ah} target={x},{y}"
    ));
    let _ = win.set_position(PhysicalPosition::new(x, y));
    #[cfg(all(windows, feature = "win7"))]
    win7::set_window_position(win, x, y);
}

// Webview windows must not be created inside an IPC/tray callback on the main
// thread: the inline creation deadlocks WebView2 initialization.
static SETTINGS_CREATE_LOCK: Mutex<()> = Mutex::new(());

fn point_in_rect(pos: PhysicalPosition<i32>, size: tauri::PhysicalSize<u32>, x: f64, y: f64) -> bool {
    x >= pos.x as f64
        && x <= (pos.x + size.width as i32) as f64
        && y >= pos.y as f64
        && y <= (pos.y + size.height as i32) as f64
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn settings_modal_active(app: &AppHandle) -> bool {
    app.state::<AppState>()
        .settings_modal_until
        .load(Ordering::Relaxed)
        > now_millis()
}

#[cfg(windows)]
fn settings_window_disabled(win: &WebviewWindow) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    win.hwnd()
        .map(|handle| unsafe { IsWindowEnabled(handle.0 as _) } == 0)
        .unwrap_or(false)
}

/// Closes on outside clicks are only safe when no native dialog owns the
/// settings window; destroying the webview underneath a live file picker can
/// corrupt the heap.
fn settings_auto_close_allowed(app: &AppHandle, win: &WebviewWindow) -> bool {
    if settings_modal_active(app) {
        return false;
    }
    #[cfg(windows)]
    if settings_window_disabled(win) {
        return false;
    }
    #[cfg(not(windows))]
    let _ = win;
    true
}

pub(crate) fn close_settings_if_clicked_outside(app: &AppHandle) {
    let Some(settings) = app.get_webview_window("settings") else {
        return;
    };
    if !settings.is_visible().unwrap_or(false) {
        return;
    }
    if !settings_auto_close_allowed(app, &settings) {
        return;
    }
    let Some((cx, cy)) = cursor_pos_for(&settings) else {
        return;
    };
    let (Ok(pos), Ok(size)) = (settings.outer_position(), settings.outer_size()) else {
        return;
    };
    if point_in_rect(pos, size, cx, cy) {
        return;
    }
    if let Some((pet_x, pet_y, pet_w, pet_h)) = main_window_rect(app) {
        if main_window_is_visible(app)
            && point_in_rect(
                PhysicalPosition::new(pet_x, pet_y),
                tauri::PhysicalSize::new(pet_w as u32, pet_h as u32),
                cx,
                cy,
            )
        {
            let _ = settings.destroy();
            return;
        }
    }
    // Native <select> popups live outside the window but keep it focused, so
    // wait for the click to settle and only close if focus actually moved.
    let app = app.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(160));
        if let Some(settings) = app.get_webview_window("settings") {
            if settings.is_visible().unwrap_or(false)
                && !settings.is_focused().unwrap_or(true)
                && settings_auto_close_allowed(&app, &settings)
            {
                let _ = settings.destroy();
            }
        }
    });
}

pub(crate) fn show_settings_window(app: &AppHandle) {
    let app = app.clone();
    thread::spawn(move || {
        let Ok(_guard) = SETTINGS_CREATE_LOCK.try_lock() else {
            return;
        };
        show_settings_window_blocking(&app);
    });
}

fn show_and_place_settings_window(app: &AppHandle, win: &WebviewWindow) {
    #[cfg(not(all(windows, feature = "win7")))]
    {
        place_settings_window(app, win);
        let _ = win.show();
        let _ = win.set_focus();
    }
    #[cfg(all(windows, feature = "win7"))]
    {
        let _ = win.show();
        place_settings_window(app, win);
        win7::force_tool_window(win);
        let _ = win.set_focus();
        place_settings_window(app, win);
    }
}

fn show_settings_window_blocking(app: &AppHandle) {
    app.state::<AppState>()
        .settings_modal_until
        .store(0, Ordering::Relaxed);
    if let Some(win) = app.get_webview_window("settings") {
        show_and_place_settings_window(app, &win);
        return;
    }
    let built = tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title("7paws 设置")
    .inner_size(SETTINGS_WIDTH, SETTINGS_HEIGHT)
    .resizable(false)
    .decorations(false)
    .transparent(cfg!(not(all(windows, feature = "win7"))))
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    // Must match the main window: WebView2 rejects a second webview that
    // requests different browser arguments against the shared environment.
    .additional_browser_args(BROWSER_ARGS)
    .visible(false)
    .build();
    match built {
        Ok(win) => show_and_place_settings_window(app, &win),
        Err(e) => eprintln!("failed to open settings window: {e}"),
    }
}

#[tauri::command]
fn open_settings(app: AppHandle) {
    show_settings_window(&app);
}

#[tauri::command]
async fn close_settings(app: AppHandle) {
    // Destroy rather than hide so the settings WebView process exits;
    // show_settings_window recreates the window on demand.
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.destroy();
    }
}

#[tauri::command]
fn set_settings_modal(state: State<'_, AppState>, open: bool) {
    let until = if open {
        now_millis() + SETTINGS_MODAL_TIMEOUT_MS
    } else {
        0
    };
    state.settings_modal_until.store(until, Ordering::Relaxed);
}

#[tauri::command]
fn get_custom_cats(state: State<'_, AppState>) -> Vec<cats::CustomCat> {
    state.custom_cats.lock().unwrap().clone()
}

#[tauri::command]
fn save_custom_cat(
    app: AppHandle,
    state: State<'_, AppState>,
    mut cat: cats::CustomCat,
    activate: bool,
) -> cats::CustomCat {
    let mut list = state.custom_cats.lock().unwrap();
    if cat.id.trim().is_empty() {
        cat.id = cats::unique_cat_id(&cat.name, &list);
    }
    let saved = cats::upsert_custom_cat(&mut list, cat);
    let snapshot = list.clone();
    drop(list);

    if activate {
        let mut s = state.settings.lock().unwrap();
        s.coat = saved.id.clone();
        save_settings(&s);
        let out = s.clone();
        drop(s);
        let _ = app.emit("cat-settings", out);
    }
    let _ = app.emit("cat-cats", snapshot);
    saved
}

#[tauri::command]
fn delete_custom_cat(app: AppHandle, state: State<'_, AppState>, id: String) -> bool {
    let mut list = state.custom_cats.lock().unwrap();
    let removed = cats::delete_custom_cat(&mut list, &id);
    let snapshot = list.clone();
    drop(list);
    if !removed {
        return false;
    }
    let fallback = {
        let mut s = state.settings.lock().unwrap();
        if s.coat == id {
            s.coat = "tuxedo".to_string();
            save_settings(&s);
            Some(s.clone())
        } else {
            None
        }
    };
    if let Some(out) = fallback {
        let _ = app.emit("cat-settings", out);
    }
    let _ = app.emit("cat-cats", snapshot);
    true
}

#[tauri::command]
fn get_mcp_command() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "7paws.exe".to_string())
}

#[tauri::command]
fn is_portable_install() -> bool {
    config::is_portable_install()
}

#[tauri::command]
fn create_desktop_shortcut() -> Result<String, String> {
    config::create_desktop_shortcut()
}

#[tauri::command]
fn open_project_page() {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", "https://github.com/7wdn/7paws"])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("https://github.com/7wdn/7paws")
            .spawn();
    }
}

#[tauri::command]
fn open_release_page() {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", "https://github.com/7wdn/7paws/releases/latest"])
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("https://github.com/7wdn/7paws/releases/latest")
            .spawn();
    }
}

#[tauri::command]
fn set_hit_regions(state: State<'_, AppState>, regions: Vec<[f64; 4]>) {
    let mut sanitized: Vec<[f64; 4]> = Vec::new();
    for rect in regions.into_iter().take(8) {
        let x = rect[0].clamp(0.0, 1.0);
        let y = rect[1].clamp(0.0, 1.0);
        let w = rect[2].clamp(0.0, 1.0 - x);
        let h = rect[3].clamp(0.0, 1.0 - y);
        if w > 0.001 && h > 0.001 {
            sanitized.push([x, y, w, h]);
        }
    }
    if sanitized.is_empty() {
        sanitized = default_regions();
    }
    *state.hit_regions.lock().unwrap() = sanitized;
}

#[tauri::command]
fn reset_position(app: AppHandle, state: State<'_, AppState>) {
    let mut s = state.settings.lock().unwrap();
    if let Some(win) = app.get_webview_window("main") {
        let sf = win.scale_factor().unwrap_or(1.0);
        let pos = bottom_right_position(&app, &s.keyboard_layout, s.scale, sf);
        #[cfg(not(all(windows, feature = "win7")))]
        let _ = win.set_position(pos);
        #[cfg(all(windows, feature = "win7"))]
        win7::set_position(pos.x, pos.y);
        s.x = Some(pos.x);
        s.y = Some(pos.y);
        save_settings(&s);
        let out = s.clone();
        drop(s);
        let _ = app.emit("cat-settings", out);
    }
}

#[tauri::command]
fn get_app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
fn is_win7_mode() -> bool {
    cfg!(all(windows, feature = "win7"))
}

#[tauri::command]
fn win7_present_frame(request: tauri::ipc::Request<'_>) {
    #[cfg(all(windows, feature = "win7"))]
    {
        let width = request
            .headers()
            .get("x-w")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let height = request
            .headers()
            .get("x-h")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        if let tauri::ipc::InvokeBody::Raw(bytes) = request.body() {
            win7::submit_rgba_frame(width, height, bytes);
        }
    }
    #[cfg(not(all(windows, feature = "win7")))]
    {
        let _ = request;
    }
}

#[tauri::command]
fn win7_debug_log(message: String) {
    #[cfg(all(windows, feature = "win7"))]
    {
        if message.len() <= 512 {
            win7::debug_log(message);
        }
    }
    #[cfg(not(all(windows, feature = "win7")))]
    {
        let _ = message;
    }
}

#[tauri::command]
fn win7_begin_drag() {
    #[cfg(all(windows, feature = "win7"))]
    win7::begin_drag();
}

#[tauri::command]
fn win7_set_over_head(over: bool) {
    #[cfg(all(windows, feature = "win7"))]
    win7::set_over_head(over);
    #[cfg(not(all(windows, feature = "win7")))]
    {
        let _ = over;
    }
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    #[cfg(all(windows, feature = "win7"))]
    win7::shutdown();
    app.exit(0);
}

fn file_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn settings_equal_ignoring_position(a: &AppSettings, b: &AppSettings) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    a.x = None;
    b.x = None;
    a.y = None;
    b.y = None;
    a == b
}

// The MCP stdio server and hand edits write these files directly; poll them so
// a running pet picks the change up live. Parsing failures are treated as a
// half-written file and skipped until the next tick.
fn start_storage_watcher(
    app: AppHandle,
    settings: Arc<Mutex<AppSettings>>,
    custom_cats: Arc<Mutex<Vec<cats::CustomCat>>>,
) {
    thread::spawn(move || {
        let mut last_settings = file_mtime(&config_path());
        let mut last_cats = file_mtime(&cats::cats_path());
        loop {
            thread::sleep(Duration::from_millis(400));
            let now_cats = file_mtime(&cats::cats_path());
            if now_cats != last_cats {
                last_cats = now_cats;
                if let Some(disk) = cats::try_load_custom_cats() {
                    let changed = {
                        let mut current = custom_cats.lock().unwrap();
                        if *current == disk {
                            false
                        } else {
                            *current = disk.clone();
                            true
                        }
                    };
                    if changed {
                        let fallback = {
                            let mut s = settings.lock().unwrap();
                            if !cats::is_builtin_coat(&s.coat)
                                && !disk.iter().any(|c| c.id == s.coat)
                            {
                                s.coat = "tuxedo".to_string();
                                save_settings(&s);
                                Some(s.clone())
                            } else {
                                None
                            }
                        };
                        let _ = app.emit("cat-cats", disk);
                        if let Some(out) = fallback {
                            let _ = app.emit("cat-settings", out);
                        }
                    }
                }
            }
            let now_settings = file_mtime(&config_path());
            if now_settings != last_settings {
                last_settings = now_settings;
                if let Some(mut disk) = try_load_settings() {
                    let adopted = {
                        let mut s = settings.lock().unwrap();
                        if settings_equal_ignoring_position(&s, &disk) {
                            None
                        } else {
                            if !cats::is_builtin_coat(&disk.coat) {
                                let known = custom_cats
                                    .lock()
                                    .unwrap()
                                    .iter()
                                    .any(|c| c.id == disk.coat);
                                if !known {
                                    disk.coat = "tuxedo".to_string();
                                }
                            }
                            // Window position stays runtime-owned.
                            disk.x = s.x;
                            disk.y = s.y;
                            let prev_scale = s.scale;
                            let prev_layout = s.keyboard_layout.clone();
                            let prev_top = s.always_on_top;
                            *s = disk.clone();
                            Some((disk, prev_scale, prev_layout, prev_top))
                        }
                    };
                    if let Some((out, prev_scale, prev_layout, prev_top)) = adopted {
                        if prev_top != out.always_on_top {
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = win.set_always_on_top(out.always_on_top);
                                #[cfg(all(windows, feature = "win7"))]
                                win7::set_always_on_top(out.always_on_top);
                            }
                        }
                        if (out.scale - prev_scale).abs() > 0.0005
                            || out.keyboard_layout != prev_layout
                        {
                            if let Some(win) = app.get_webview_window("main") {
                                let _ = resize_anchored_bottom_center(
                                    &win,
                                    &out.keyboard_layout,
                                    out.scale,
                                );
                            }
                        }
                        let _ = app.emit("cat-settings", out);
                    }
                }
            }
        }
    });
}

pub fn run_mcp_stdio() {
    mcp::run_stdio();
}

#[cfg(windows)]
pub fn wait_for_process_exit(process_id: u32) {
    windows_zorder::wait_for_process_exit(process_id);
}

#[tauri::command]
fn is_elevated() -> bool {
    #[cfg(windows)]
    {
        windows_zorder::is_elevated_now()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[tauri::command]
fn restart_as_admin(app: AppHandle) -> Result<String, String> {
    #[cfg(windows)]
    {
        windows_zorder::restart_as_admin(app)
    }
    #[cfg(not(windows))]
    {
        let _ = app;
        Err("只有 Windows 支持以管理员身份重启".to_string())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    run_inner(false);
}

#[cfg(windows)]
pub fn run_after_restart() {
    std::thread::sleep(std::time::Duration::from_millis(1_000));
    run_inner(true);
}

fn run_inner(_restarting: bool) {
    #[cfg(windows)]
    let Some(_single_instance_guard) = acquire_single_instance(_restarting) else {
        return;
    };

    #[cfg(all(windows, feature = "win7"))]
    configure_win7_runtime();

    let mut initial = load_settings();
    #[cfg(windows)]
    if windows_zorder::request_elevated_startup(initial.run_as_admin) {
        std::process::exit(0);
    }
    let custom_cats = Arc::new(Mutex::new(cats::load_custom_cats()));
    if !cats::is_builtin_coat(&initial.coat)
        && !custom_cats
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.id == initial.coat)
    {
        initial.coat = "tuxedo".to_string();
        save_settings(&initial);
    }
    let settings = Arc::new(Mutex::new(initial.clone()));
    let dragging = Arc::new(AtomicBool::new(false));
    let hit_regions = Arc::new(Mutex::new(default_regions()));
    let settings_modal_until = Arc::new(AtomicU64::new(0));

    let settings_setup = settings.clone();
    let cats_setup = custom_cats.clone();
    let drag_setup = dragging.clone();
    let regions_setup = hit_regions.clone();

    tauri::Builder::default()
        .manage(AppState {
            settings: settings.clone(),
            custom_cats: custom_cats.clone(),
            dragging,
            hit_regions,
            settings_modal_until,
        })
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            {
                // Tray-only pet: no Dock icon, no app menu bar.
                let _ = app
                    .handle()
                    .set_activation_policy(tauri::ActivationPolicy::Accessory);
            }

            let handle = app.handle().clone();
            let s = settings_setup.lock().unwrap().clone();

            if s.autostart {
                // Re-register under the new name after the 7paws rename.
                let _ = set_autostart(true);
            }

            if let Some(win) = app.get_webview_window("main") {
                #[cfg(all(windows, feature = "win7"))]
                win7::debug_log("setup:main_window_found");
                let (w, h) = window_dimensions(&s.keyboard_layout, s.scale);
                let _ = win.set_size(Size::Logical(LogicalSize::new(w, h)));
                let _ = win.set_always_on_top(s.always_on_top);

                let sf = win.scale_factor().unwrap_or(1.0);
                let pw = (w * sf).round() as i32;
                let ph = (h * sf).round() as i32;
                let pos = if let (Some(x), Some(y)) = (s.x, s.y) {
                    clamp_to_primary(&handle, x, y, pw, ph)
                } else {
                    bottom_right_position(&handle, &s.keyboard_layout, s.scale, sf)
                };
                #[cfg(not(all(windows, feature = "win7")))]
                let _ = win.set_position(pos);
                #[cfg(all(windows, feature = "win7"))]
                {
                    if !win7::init(handle.clone(), pos.x, pos.y, pw as u32, ph as u32, s.always_on_top) {
                        win7::debug_log("setup:presenter_init_failed");
                        eprintln!("failed to create the Windows 7 layered presenter");
                    }
                    let _ = win.set_position(PhysicalPosition::new(-30000, -30000));
                    let _ = win.set_ignore_cursor_events(true);
                    let _ = win.show();
                    win7::force_tool_window(&win);
                }
            } else {
                #[cfg(all(windows, feature = "win7"))]
                win7::debug_log("setup:main_window_missing");
            }

            tray::setup_tray(&handle, settings_setup.clone())?;
            input_hook::start_global_hooks(handle.clone(), settings_setup.clone());
            #[cfg(windows)]
            windows_zorder::start_guard(handle.clone(), settings_setup.clone(), drag_setup.clone());
            start_hit_test_loop(
                handle.clone(),
                settings_setup.clone(),
                drag_setup.clone(),
                regions_setup.clone(),
            );
            start_position_watcher(handle.clone(), settings_setup.clone());
            start_storage_watcher(handle.clone(), settings_setup.clone(), cats_setup.clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            update_settings,
            open_settings,
            close_settings,
            set_settings_modal,
            get_custom_cats,
            save_custom_cat,
            delete_custom_cat,
            get_mcp_command,
            is_portable_install,
            create_desktop_shortcut,
            is_elevated,
            restart_as_admin,
            open_project_page,
            open_release_page,
            set_dragging,
            set_hit_regions,
            reset_position,
            get_app_version,
            is_win7_mode,
            win7_present_frame,
            win7_debug_log,
            win7_begin_drag,
            win7_set_over_head,
            quit_app,
        ])
        .run(tauri::generate_context!())
        .expect("error while running 7paws");
}
