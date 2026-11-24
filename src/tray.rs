//! Иконка в системном трее (Windows, `Shell_NotifyIcon`).
//!
//! Работает в отдельном потоке со своим скрытым окном и циклом сообщений,
//! поэтому UI-поток egui не блокируется, а клики по иконке приходят в наш
//! `wndproc`. Команды, которым нужно состояние плеера (играть/стоп),
//! передаются через атомарный флаг: UI забирает его в начале кадра (`poll`).
//!
//! Иконка информационная: левый клик — показать/скрыть окно, правый — меню
//! («Показать/Скрыть», «Играть/Остановить», «Выход»).

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    ExtractIconExW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW, PostMessageW,
    RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, IDI_APPLICATION,
    MF_SEPARATOR, MF_STRING, MSG, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
};

/// Сообщение «клик по иконке» (в `lParam` — событие мыши).
const WM_TRAY: u32 = WM_APP + 1;
/// Сообщение «обнови подсказку из `TOOLTIP`».
const WM_TRAY_TIP: u32 = WM_APP + 2;
const TRAY_ID: u32 = 1;
const CMD_TOGGLE_WINDOW: usize = 1;
const CMD_TOGGLE_PLAY: usize = 2;
const CMD_QUIT: usize = 3;

/// Действие для UI: забирается в начале кадра.
pub enum Action {
    TogglePlay,
}

static HWND_TRAY: AtomicIsize = AtomicIsize::new(0);
static ICON: AtomicIsize = AtomicIsize::new(0);
/// Нажатие «Играть/Остановить» из меню (1 — есть непрочитанное).
static ACTION: AtomicI32 = AtomicI32::new(0);
static PLAYING: AtomicBool = AtomicBool::new(false);
static TOOLTIP: Mutex<String> = Mutex::new(String::new());
static CTX: Mutex<Option<eframe::egui::Context>> = Mutex::new(None);

/// Забирает отложенное действие из меню трея.
pub fn poll() -> Option<Action> {
    if ACTION.swap(0, Ordering::SeqCst) != 0 {
        Some(Action::TogglePlay)
    } else {
        None
    }
}

/// Подсказка при наведении на иконку (обрезается до 120 символов).
pub fn set_tooltip(text: &str) {
    if let Ok(mut guard) = TOOLTIP.lock() {
        if guard.as_str() == text {
            return;
        }
        *guard = text.chars().take(120).collect();
    }
    let hwnd = HWND_TRAY.load(Ordering::SeqCst) as HWND;
    if hwnd != 0 {
        unsafe {
            PostMessageW(hwnd, WM_TRAY_TIP, 0, 0);
        }
    }
}

/// Запоминает, идёт ли воспроизведение (для пункта меню).
pub fn set_playing(on: bool) {
    PLAYING.store(on, Ordering::Relaxed);
}

/// Создаёт иконку в трее (один раз). `false` — трей недоступен.
pub fn start(ctx: &eframe::egui::Context) -> bool {
    if HWND_TRAY.load(Ordering::SeqCst) != 0 {
        return true;
    }
    if let Ok(mut guard) = CTX.lock() {
        *guard = Some(ctx.clone());
    }
    // Поток живёт до выхода из приложения: цикл сообщений завершится
    // по WM_DESTROY, который приходит из `remove()`.
    std::thread::spawn(move || unsafe { run() });
    // Ждём, пока поток создаст окно (не дольше 1 с): UI должен знать,
    // удалось ли создать иконку.
    for _ in 0..100 {
        if HWND_TRAY.load(Ordering::SeqCst) != 0 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

/// Убирает иконку при выходе.
pub fn remove() {
    let hwnd = HWND_TRAY.swap(0, Ordering::SeqCst) as HWND;
    if hwnd != 0 {
        unsafe {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Разбудить UI-поток (иконка и меню живут в своём потоке).
fn wake() {
    if let Ok(guard) = CTX.lock() {
        if let Some(ctx) = guard.as_ref() {
            ctx.request_repaint();
        }
    }
}

/// Поток трея: скрытое окно → иконка → цикл сообщений.
unsafe fn run() {
    let hinstance = GetModuleHandleW(std::ptr::null());
    let class = wide("TwitchMiniPlayerTray");
    let wndclass = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: 0,
        hCursor: 0,
        hbrBackground: 0,
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    RegisterClassW(&wndclass);

    let title = wide("Twitch Mini Player (трей)");
    // Обычное невидимое окно: ему приходят сообщения иконки и меню.
    let hwnd = CreateWindowExW(
        0,
        class.as_ptr(),
        title.as_ptr(),
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        hinstance,
        std::ptr::null(),
    );
    if hwnd == 0 {
        crate::debug_log("tray: CreateWindowExW не удался");
        return;
    }
    HWND_TRAY.store(hwnd as isize, Ordering::SeqCst);

    // Иконка: сначала пробуем взять её из своего exe, иначе — стандартную.
    let mut large: isize = 0;
    let mut small: isize = 0;
    let exe = wide(&std::env::current_exe().unwrap_or_default().to_string_lossy());
    let count = ExtractIconExW(exe.as_ptr(), 0, &mut large, &mut small, 1);
    let mut icon = if count > 0 && small != 0 { small } else { large };
    if icon == 0 {
        icon = LoadIconW(0, IDI_APPLICATION) as isize;
    }
    ICON.store(icon, Ordering::SeqCst);

    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = icon;
    fill_tip(&mut nid.szTip, "Mini Twitch Player");
    if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
        crate::debug_log("tray: Shell_NotifyIcon(NIM_ADD) не удался");
    } else {
        crate::debug_log("tray: иконка создана");
    }

    let mut msg: MSG = std::mem::zeroed();
    while GetMessageW(&mut msg, 0, 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    // Чистим иконку (если поток завершился не через `remove()`).
    Shell_NotifyIconW(NIM_DELETE, &nid);
    if icon != 0 {
        DestroyIcon(icon);
    }
    if large != 0 && large != icon {
        DestroyIcon(large);
    }
}

fn fill_tip(dst: &mut [u16; 128], text: &str) {
    let w: Vec<u16> = text.encode_utf16().collect();
    let n = w.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&w[..n]);
    dst[n] = 0;
}

unsafe fn apply_tooltip(hwnd: HWND) {
    let text = TOOLTIP
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| "Mini Twitch Player".to_string());
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ID;
    nid.uFlags = NIF_TIP;
    fill_tip(
        &mut nid.szTip,
        if text.is_empty() {
            "Mini Twitch Player"
        } else {
            &text
        },
    );
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn show_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu == 0 {
        return;
    }
    let visible = crate::win::is_main_visible();
    let window_label = wide(if visible { "Скрыть" } else { "Показать" });
    let play_label = wide(if PLAYING.load(Ordering::Relaxed) {
        "Остановить"
    } else {
        "Воспроизвести"
    });
    let quit_label = wide("Выход");
    AppendMenuW(menu, MF_STRING, CMD_TOGGLE_WINDOW, window_label.as_ptr());
    AppendMenuW(menu, MF_STRING, CMD_TOGGLE_PLAY, play_label.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    AppendMenuW(menu, MF_STRING, CMD_QUIT, quit_label.as_ptr());

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    // Без SetForegroundWindow меню не закроется при клике мимо него.
    SetForegroundWindow(hwnd);
    TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    DestroyMenu(menu);
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            let event = (lparam & 0xFFFF) as u32;
            if event == WM_LBUTTONUP {
                crate::win::toggle_main();
                wake();
            } else if event == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            0
        }
        WM_TRAY_TIP => {
            apply_tooltip(hwnd);
            0
        }
        WM_COMMAND => {
            match (wparam & 0xFFFF) as usize {
                CMD_TOGGLE_WINDOW => {
                    crate::win::toggle_main();
                    wake();
                }
                CMD_TOGGLE_PLAY => {
                    ACTION.store(1, Ordering::SeqCst);
                    wake();
                }
                CMD_QUIT => {
                    if let Ok(guard) = CTX.lock() {
                        if let Some(ctx) = guard.as_ref() {
                            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                        }
                    }
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = TRAY_ID;
            Shell_NotifyIconW(NIM_DELETE, &nid);
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            HWND_TRAY.store(0, Ordering::SeqCst);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
