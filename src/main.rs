#![windows_subsystem = "windows"]

mod gpu;
mod libmpv;
mod subs;
mod tray;

use eframe::egui;
use libmpv::MpvPlayer;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const TWITCH_API: &str = "https://api.twitch.tv/helix/streams";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const WINDOW_TITLE: &str = "Mini Twitch Player (Rust)";
/// Заголовок собран один раз: `format!` на каждый кадр — лишняя аллокация.
const HEADER: &str = concat!("Mini Twitch Player (Rust + libmpv)  v", env!("CARGO_PKG_VERSION"));
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DEFAULT_QUALITIES: [&str; 6] = [
    "audio_only",
    "160p30",
    "360p30",
    "480p30",
    "720p60",
    "1080p60",
];

/// Сколько секунд можно переиспользовать ссылки из ответа streamlink.
/// 15 минут: столько живут HLS-ссылки Twitch на практике, поэтому перезапуск
/// streamlink (это Python, ~1–2 с) почти никогда не нужен. Если ссылка всё же
/// истекла, плеер сам получит свежую (см. `launched_from_cache`).
const CACHE_TTL: Duration = Duration::from_secs(900);
/// Защита от «вечного» подключения, если streamlink завис.
const PLAY_TIMEOUT: Duration = Duration::from_secs(45);
/// Столько секунд пустого кэша терпим, прежде чем снизить качество.
const STALL_STEP_DOWN_AFTER: Duration = Duration::from_secs(3);
/// Столько стабильного воспроизведения нужно, чтобы «Авто» подняло качество.
const STALL_RESTORE_AFTER: Duration = Duration::from_secs(30);
/// После такой остановки переразрешаем поток (свежий CDN-узел).
const STALL_REFRESH_AFTER: Duration = Duration::from_secs(12);
/// Не чаще этого обновляем ссылку принудительно.
const REFRESH_COOLDOWN: Duration = Duration::from_secs(300);
/// Не чаще этого «Авто» меняет качество (в любую сторону).
const AUTO_STEP_COOLDOWN: Duration = Duration::from_secs(30);
/// Шаг громкости на одно деление колеса мыши над видео.
const VOLUME_STEP: i32 = 5;

/// Минимальные Win32-вызовы для энергосбережения.
/// В egui 0.28 нет `ViewportInfo::minimized`, поэтому спрашиваем у Windows.
#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowLongPtrW, GetWindowTextLengthW,
        GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow,
        SetWindowPos, ShowWindow, GWL_EXSTYLE, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_RESTORE, SW_SHOW,
    };

    static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

    struct Search {
        pid: u32,
        found: HWND,
        /// Видимое окно процесса с заголовком — на случай незнакомого класса.
        fallback: HWND,
    }

    /// Класс окна, которое winit создаёт для egui. Служебные окна winit
    /// («Winit Thread Event Target») тоже видны — их брать нельзя.
    const MAIN_CLASS: &str = "Window Class";

    unsafe fn is_main_window(hwnd: HWND) -> bool {
        let mut buf = [0u16; 64];
        let n = GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if n <= 0 {
            return false;
        }
        let name = String::from_utf16_lossy(&buf[..n as usize]);
        name == MAIN_CLASS
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let search = &mut *(lparam as *mut Search);
        if IsWindowVisible(hwnd) != 0 {
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == search.pid {
                if is_main_window(hwnd) {
                    search.found = hwnd;
                    return 0; // нашли — перебор не нужен
                }
                if search.fallback == 0 && GetWindowTextLengthW(hwnd) > 0 {
                    search.fallback = hwnd;
                }
            }
        }
        1
    }

    fn find_main_window() -> HWND {
        let mut search = Search {
            pid: unsafe { GetCurrentProcessId() },
            found: 0,
            fallback: 0,
        };
        unsafe {
            EnumWindows(Some(enum_proc), &mut search as *mut Search as LPARAM);
        }
        if search.found != 0 {
            search.found
        } else {
            search.fallback
        }
    }

    /// Свёрнуто ли главное окно приложения. Дескриптор кешируется.
    pub fn window_minimized() -> bool {
        let hwnd = main_window();
        hwnd != 0 && unsafe { IsIconic(hwnd) } != 0
    }

    /// Дескриптор главного окна (0 — ещё не создано). Кешируется.
    pub fn main_window() -> HWND {
        let mut hwnd = MAIN_HWND.load(Ordering::Relaxed);
        if hwnd == 0 || unsafe { IsWindow(hwnd) } == 0 {
            hwnd = find_main_window();
            MAIN_HWND.store(hwnd, Ordering::Relaxed);
        }
        hwnd
    }

    /// Окно видимо (не скрыто в трей).
    pub fn is_main_visible() -> bool {
        let hwnd = main_window();
        hwnd != 0 && unsafe { IsWindowVisible(hwnd) } != 0
    }

    /// Окно скрыто: звук идёт, но показывать нечего — режим как у свёрнутого.
    pub fn window_hidden() -> bool {
        let hwnd = main_window();
        hwnd != 0 && unsafe { IsWindowVisible(hwnd) } == 0
    }

    /// Показать окно из трея (развернуть, если было свёрнуто).
    pub fn show_main() {
        let hwnd = main_window();
        if hwnd == 0 {
            return;
        }
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else {
                ShowWindow(hwnd, SW_SHOW);
            }
            SetForegroundWindow(hwnd);
        }
    }

    /// Скрыть окно в трей.
    pub fn hide_main() {
        let hwnd = main_window();
        if hwnd != 0 {
            unsafe {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }

    /// Показать/скрыть окно (левый клик по иконке в трее).
    pub fn toggle_main() {
        if is_main_visible() {
            hide_main();
        } else {
            show_main();
        }
    }

    /// Окно поверх всех остальных (`top = false` — обычный порядок).
    /// Позиция и размер не меняются.
    pub fn set_topmost(top: bool) {
        let hwnd = main_window();
        if hwnd == 0 {
            return;
        }
        let after = if top { HWND_TOPMOST } else { HWND_NOTOPMOST };
        let ok = unsafe {
            SetWindowPos(
                hwnd,
                after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        };
        crate::debug_log(&format!(
            "win: topmost={top} hwnd={hwnd} ok={ok} ex={:X}",
            unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) }
        ));
    }
}

/// Профили отрисовки видео: (название, масштаб разрешения кадра, максимум FPS).
/// Меньше разрешение и FPS — меньше работы для программного масштабирования
/// mpv и для загрузки текстуры, т.е. ниже нагрузка на ЦП.
const RENDER_PROFILES: [(&str, f32, u32); 3] = [
    ("Высокое (60 FPS)", 1.0, 60),
    ("Сбалансированное", 0.85, 60),
    ("Экономное (30 FPS)", 0.65, 30),
];

/// Значение `render_profile`, означающее «Авто»: профиль подбирается под ПК
/// (хранится как индекс сразу за списком профилей).
const AUTO_PROFILE: usize = RENDER_PROFILES.len();

/// Время кадра интерфейса (мс), после которого «Авто»-отрисовка переходит на
/// экономный профиль: значит, программное масштабирование не успевает.
const AUTO_PROFILE_SLOW_MS: f32 = 14.0;

/// Версия схемы настроек. При её повышении «разработческие» настройки
/// (декод, отрисовка, авто-качество) сбрасываются на лучшие значения по
/// умолчанию, а пользовательские (канал, громкость, окно) остаются как были.
/// 3 — добавлен захват звука процесса для субтитров (`subs_capture`).
const SETTINGS_VERSION: u32 = 3;

/// Задержка от эфира, которую плеер считает комфортной (с).
const LATENCY_TARGET_SECS: f64 = 4.0;
/// Если запас буфера разросся сильнее этого (сеть проснулась после просадки) —
/// догоняем эфир, чтобы задержка не осталась навсегда.
const CATCHUP_ABOVE_SECS: f64 = 8.0;
/// Не чаще этого догоняем эфир (иначе картинка «дёргается»).
const CATCHUP_COOLDOWN: Duration = Duration::from_secs(10);

fn render_scale(profile: usize) -> f32 {
    RENDER_PROFILES.get(profile).map(|p| p.1).unwrap_or(1.0)
}

/// Отладочный лог включён (`TW_DEBUG_LOG`)?
pub fn debug_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TW_DEBUG_LOG").is_some())
}

/// Отладочный лог в `%TEMP%\twitch_mini_player_debug.txt` (только если задан
/// `TW_DEBUG_LOG=1`). Помогает разбирать проблемы рендера/сети по фактам.
pub fn debug_log(msg: &str) {
    if !debug_enabled() {
        return;
    }
    use std::io::Write as _;
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    let path = std::env::temp_dir().join("twitch_mini_player_debug.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{:>7.1} ms] {}", start.elapsed().as_secs_f64() * 1000.0, msg);
    }
}

/// Подбор профиля для «Авто»: с GPU-рендером кадр почти бесплатен (полное
/// разрешение), а в программном пути включаем экономный, если время кадра
/// интерфейса уже не укладывается в 60 FPS.
fn auto_profile(gpu_render: bool, ui_cost_ms: f32) -> usize {
    if gpu_render {
        return 0;
    }
    if ui_cost_ms >= AUTO_PROFILE_SLOW_MS {
        RENDER_PROFILES.len() - 1
    } else {
        1
    }
}

fn render_interval(profile: usize) -> Duration {
    let fps = RENDER_PROFILES.get(profile).map(|p| p.2).unwrap_or(60).max(1);
    Duration::from_millis((1000 / fps as u64).max(1))
}

/// Округляет размер кадра вверх до кратности 8 (и не меньше 8).
/// Так размер не «дрожит» от пиксельных колебаний вёрстки, и текстура
/// не пересоздаётся на каждом кадре.
fn align8(v: u32) -> u32 {
    ((v.max(8) + 7) / 8) * 8
}

/// «Только звук» — качество без видеодорожки (экономит трафик в разы).
fn is_audio_only(quality: &str) -> bool {
    quality.eq_ignore_ascii_case("audio_only") || quality.eq_ignore_ascii_case("audio")
}

/// Чип-подпись поверх кадра (пауза, громкость, буферизация).
/// Возвращает высоту чипа, чтобы следующий встал ровно под ним.
fn chip(ui: &egui::Ui, rect: egui::Rect, top: f32, text: &str) -> f32 {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::proportional(14.0),
        egui::Color32::from_gray(235),
    );
    let pad = egui::vec2(12.0, 7.0);
    let size = galley.size() + pad * 2.0;
    let area = egui::Rect::from_min_size(egui::pos2(rect.left() + 14.0, top), size);
    ui.painter()
        .rect_filled(area, egui::Rounding::same(9.0), egui::Color32::from_black_alpha(190));
    ui.painter()
        .galley(area.min + pad, galley, egui::Color32::from_gray(235));
    size.y
}

/// Заставка буферизации поверх кадра: затемнение, крутящийся индикатор и
/// подпись. Показывается, когда поток ждёт данные (кэш пуст) или ещё грузится,
/// — по ней сразу видно, что плеер жив, а не «завис».
fn buffering_splash(
    ui: &egui::Ui,
    rect: egui::Rect,
    rounding: egui::Rounding,
    title: &str,
    secs: f32,
) {
    let painter = ui.painter();
    painter.rect_filled(rect, rounding, egui::Color32::from_black_alpha(150));

    // Спиннер: дуга из коротких отрезков с плавно растущей яркостью.
    let center = rect.center() - egui::vec2(0.0, 12.0);
    let radius = 22.0;
    let start = ui.input(|i| i.time) as f32 * 3.2;
    const SEGMENTS: usize = 20;
    let step = std::f32::consts::TAU / SEGMENTS as f32;
    for i in 0..SEGMENTS {
        let a0 = start + i as f32 * step;
        let a1 = a0 + step * 0.45;
        let alpha = (25.0 + 230.0 * i as f32 / SEGMENTS as f32) as u8;
        painter.line_segment(
            [
                center + egui::vec2(a0.cos(), a0.sin()) * radius,
                center + egui::vec2(a1.cos(), a1.sin()) * radius,
            ],
            egui::Stroke::new(3.0_f32, egui::Color32::from_white_alpha(alpha)),
        );
    }

    let text = if secs >= 1.0 {
        format!("{}… {:.0} с", title, secs)
    } else {
        format!("{}…", title)
    };
    painter.text(
        center + egui::vec2(0.0, radius + 24.0),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(15.0),
        egui::Color32::from_gray(235),
    );
}

/// интерфейс (настройки, статистика) не рисуется — виден только кадр.
/// Переключается двойным кликом по видео, `Esc` выключает.
fn set_video_fullscreen(ctx: &egui::Context, on: bool) {
    crate::debug_log(&format!("ui: полный экран видео {}", on));
    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
}

/// Сообщения из фонового потока (streamlink и Twitch API).
enum WorkerMsg {
    Resolved {
        job: u64,
        channel: String,
        json: String,
    },
    Failed {
        job: u64,
        error: String,
    },
    Meta {
        job: u64,
        meta: Option<StreamMeta>,
    },
}

#[derive(Debug, Clone, Default)]
struct StreamMeta {
    title: String,
    game: String,
    viewers: u64,
    started_at: String,
}

#[derive(Deserialize)]
struct TwitchStreamResponse {
    data: Vec<TwitchStream>,
}

#[derive(Deserialize)]
struct TwitchStream {
    title: String,
    game_name: String,
    viewer_count: u64,
    started_at: String,
}

/// Сохраняемые настройки (`%APPDATA%\twitch_mini_player\settings.json`).
/// Отсутствующие поля берут значения из `Default`, поэтому старые файлы
/// продолжают работать после добавления новых настроек.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    /// Версия схемы настроек (см. `SETTINGS_VERSION`).
    version: u32,
    channel: String,
    quality: String,
    volume: u8,
    render_profile: usize,
    hwdec: bool,
    auto_quality: bool,
    /// GPU-рендер: mpv рисует кадр прямо в текстуру, без копий через RAM.
    gpu_render: bool,
    /// Иконка в системном трее.
    tray: bool,
    /// Окно поверх остальных.
    pinned: bool,
    /// Захват звука нашего процесса для субтитров (этап 0.18.0).
    subs_capture: bool,
    client_id: String,
    /// Последняя геометрия окна (позиция и размер), чтобы всё открывалось как было.
    window: Option<WindowRect>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            channel: String::new(),
            quality: "720p60".to_string(),
            volume: 100,
            // «Лучшие настройки» по умолчанию: кадр декодирует и рисует
            // видеокарта, профиль и качество подбираются автоматически.
            render_profile: AUTO_PROFILE,
            hwdec: true,
            auto_quality: true,
            gpu_render: true,
            tray: true,
            pinned: false,
            // Захват звука включается осознанно: он нужен только ради субтитров.
            subs_capture: false,
            client_id: String::new(),
            window: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct WindowRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

struct TwitchMiniPlayer {
    channel: String,
    quality: String,
    status: String,
    meta: Option<StreamMeta>,
    client_id: String,
    streamlink_path: String,
    player: Option<MpvPlayer>,
    texture: Option<egui::TextureHandle>,
    tex_size: (u32, u32),
    volume: u8,
    error: Option<String>,
    stream_url: Option<String>,
    qualities: Vec<String>,
    streams_json: Option<String>,
    favorites: Vec<String>,
    rx: Receiver<WorkerMsg>,
    tx: Sender<WorkerMsg>,
    /// Номер текущего запроса: результаты старых запросов игнорируются.
    job: u64,
    /// Идёт фоновое подключение (кнопка Play заблокирована).
    busy: bool,
    busy_since: Option<Instant>,
    /// (канал, JSON streamlink, время получения)
    cached: Option<(String, String, Instant)>,
    /// Канал, который сейчас воспроизводится.
    playing_channel: Option<String>,
    /// Текущий фоновый запрос должен сразу запустить воспроизведение.
    pending_play: bool,
    /// Момент последнего изменения поля «Канал» (для отложенной предзагрузки).
    prefetch_at: Option<Instant>,
    /// Результат спекулятивной предзагрузки (не влияет на текущий поток).
    prefetched: Option<(String, String, Instant)>,
    /// Поток запущен по ссылке из кэша (нужно для авто-повтора при истечении).
    launched_from_cache: bool,
    /// Канал, для которого сейчас идёт фоновое разрешение потока.
    inflight_channel: Option<String>,
    /// Индекс профиля отрисовки в `RENDER_PROFILES` (или `AUTO_PROFILE`).
    render_profile: usize,
    /// Профиль, которым рисуем сейчас: «Авто» подбирается под ПК.
    profile_now: usize,
    /// Сглаженное время кадра интерфейса (мс) — вход для «Авто»-профиля.
    ui_cost_ms: f32,
    /// Открыта консоль разработчика (декод, отрисовка, статистика, ссылка).
    dev_panel: bool,
    /// Аппаратное (GPU) декодирование вместо софтверного — меньше нагрузка на ЦП.
    hwdec: bool,
    /// Момент последней отрисовки кадра (для ограничения FPS).
    last_render: Option<Instant>,
    /// Одиночный клик по видео ждёт 250 мс: возможно, это первый клик двойного.
    pending_pause: Option<Instant>,
    /// Момент изменения громкости (чип «Громкость N%» поверх кадра).
    volume_osd: Option<Instant>,
    /// Качество «Авто»: уровень подбирается по скорости сети и просадкам.
    auto_quality: bool,
    /// Последние замеры скорости сети (байт/с) — основа режима «Авто».
    net_rate: Vec<f64>,
    /// Сколько замеров подряд просят качество ниже текущего (гистерезис).
    auto_down_votes: u32,
    /// Начало текущей остановки из-за пустого кэша.
    stall_since: Option<Instant>,
    /// С какого момента поток стабильно играет (для возврата качества).
    stable_since: Option<Instant>,
    /// Последнее авто-понижение качества.
    last_auto_quality: Option<Instant>,
    /// Последнее принудительное обновление ссылки из-за долгой остановки.
    last_refresh: Option<Instant>,
    /// Когда текущий поток реально начал играть (чтобы не считать старт просадкой).
    playing_since: Option<Instant>,
    /// GPU-рендер кадра (mpv → текстура напрямую), без конвертации в RAM.
    gpu_render: bool,
    /// Иконка в системном трее.
    tray: bool,
    /// Окно поверх остальных.
    pinned: bool,
    /// Уровень окна уже применён к Win32 (чтобы не дёргать его каждый кадр).
    pinned_applied: Option<bool>,
    /// «Полный экран» — но только для видео: интерфейс скрыт, кадр занимает
    /// всё окно (включается двойным кликом по видео, `Esc` выходит).
    video_only: bool,
    /// До какого момента показывать подсказку «двойной клик — выйти».
    video_only_hint: Option<Instant>,
    /// Настройки изменились и ждут записи на диск.
    settings_dirty: bool,
    /// Когда последний раз писали settings.json (троттлинг записи).
    last_save: Option<Instant>,
    /// Последняя сохранённая геометрия окна.
    saved_window: Option<WindowRect>,
    /// Окно свёрнуто: mpv рендерим мелко и редко (звук продолжается).
    minimized: bool,
    /// Последние замеры буфера (по одному каждые 2 с) — для прогноза просадки.
    buf_trend: Vec<f32>,
    /// Когда последний раз снимали замер буфера.
    buf_sample_at: Option<Instant>,
    /// GPU-состояние: текстура/FBO/шейдер (живут в paint-колбэке egui).
    gpu: gpu::GpuVideo,
    /// Видеодорожка отключена (окно свёрнуто): работает только звук.
    video_disabled: bool,
    /// О завершении потока уже сообщили (не спамим статусом каждый кадр).
    ended_reported: bool,
    /// URL, который ждёт готовности GL-контекста mpv (GPU-режим).
    pending_url: Option<String>,
    /// Счётчик обновлений UI (диагностика частоты перерисовки).
    updates: u64,
    /// Момент предыдущего обновления UI (диагностика).
    last_update_at: Option<Instant>,
    /// Контекст egui — нужен, чтобы будить UI из колбэка mpv.
    egui_ctx: Option<egui::Context>,
    /// Иконка в трее создана.
    tray_started: bool,
    /// Канал в эфире (`None` — ещё не проверяли).
    online: Option<bool>,
    /// Когда последний раз проверяли статус канала.
    checked_at: Option<Instant>,
    /// Когда последний раз догоняли эфир (прыжок вперёд по буферу).
    catchup_at: Option<Instant>,
    /// Кадров UI за текущее окно измерения.
    ui_frames: u32,
    /// Начало измерения частоты отрисовки UI.
    ui_frames_at: Instant,
    /// Измеренная частота отрисовки UI (к/с).
    ui_fps: f32,
    /// Готовая строка статистики (собирается не чаще 2 раз в секунду).
    stats_text: String,
    /// Когда собирали строку статистики.
    stats_at: Option<Instant>,
    /// Субтитры: захват звука процесса (0.18.0 — уровень и тестовый WAV).
    subs: subs::Subs,
}

impl Default for TwitchMiniPlayer {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        let s = load_settings();
        Self {
            channel: s.channel,
            quality: if s.quality.is_empty() {
                "720p60".to_string()
            } else {
                s.quality
            },
            status: "Готово".to_string(),
            meta: None,
            client_id: s.client_id,
            streamlink_path: find_streamlink(),
            player: None,
            texture: None,
            tex_size: (0, 0),
            volume: s.volume,
            error: None,
            stream_url: None,
            qualities: Vec::new(),
            streams_json: None,
            favorites: load_favorites(),
            rx,
            tx,
            job: 0,
            busy: false,
            busy_since: None,
            cached: None,
            playing_channel: None,
            pending_play: false,
            prefetch_at: None,
            prefetched: None,
            launched_from_cache: false,
            inflight_channel: None,
            render_profile: if s.render_profile <= AUTO_PROFILE {
                s.render_profile
            } else {
                AUTO_PROFILE
            },
            profile_now: 0,
            ui_cost_ms: 0.0,
            dev_panel: false,
            hwdec: s.hwdec,
            last_render: None,
            pending_pause: None,
            volume_osd: None,
            auto_quality: s.auto_quality,
            net_rate: Vec::new(),
            auto_down_votes: 0,
            stall_since: None,
            stable_since: None,
            last_auto_quality: None,
            last_refresh: None,
            playing_since: None,
            gpu_render: s.gpu_render,
            tray: s.tray,
            pinned: s.pinned,
            pinned_applied: None,
            video_only: false,
            video_only_hint: None,
            settings_dirty: false,
            last_save: None,
            saved_window: s.window,
            minimized: false,
            buf_trend: Vec::new(),
            buf_sample_at: None,
            gpu: gpu::GpuVideo::new(),
            video_disabled: false,
            ended_reported: false,
            pending_url: None,
            updates: 0,
            last_update_at: None,
            egui_ctx: None,
            tray_started: false,
            online: None,
            checked_at: None,
            catchup_at: None,
            ui_frames: 0,
            ui_frames_at: Instant::now(),
            ui_fps: 0.0,
            stats_text: String::new(),
            stats_at: None,
            subs: subs::Subs::new(s.subs_capture),
        }
    }
}

impl eframe::App for TwitchMiniPlayer {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Время кадра интерфейса: по нему «Авто»-профиль понимает, тянет ли ПК
        // программную отрисовку кадра.
        let frame_start = Instant::now();
        // Контекст нужен колбэкам mpv (пробуждение UI по новому кадру).
        if self.egui_ctx.is_none() {
            self.egui_ctx = Some(ctx.clone());
        }
        // Иконка в трее: создаётся один раз, после того как окно уже есть.
        if self.tray && !self.tray_started {
            self.tray_started = tray::start(ctx);
            if !self.tray_started {
                debug_log("tray: не удалось создать иконку");
            }
        }
        // Команда из меню трея (играть/остановить).
        if let Some(tray::Action::TogglePlay) = tray::poll() {
            let playing = self.player.as_ref().map(|p| p.is_playing()).unwrap_or(false);
            if playing {
                self.stop_stream();
            } else {
                self.start_play();
            }
        }
        // «Поверх всех окон»: Win32-вызов только при изменении настройки.
        if self.pinned_applied != Some(self.pinned) && win::main_window() != 0 {
            win::set_topmost(self.pinned);
            self.pinned_applied = Some(self.pinned);
        }
        // Esc выключает полный экран видео (двойной клик по видео — включает).
        // Если окно ещё в полноэкранном режиме, а режим уже снят (например,
        // окно развернули системно), тоже выходим из полноэкранного режима.
        if self.video_only && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.video_only = false;
            set_video_fullscreen(ctx, false);
        }
        // Отладочный запуск сразу в полноэкранном режиме видео (проверка
        // раскладки без ручных кликов): TW_VIDEO_ONLY=1.
        if !self.video_only
            && self.video_only_hint.is_none()
            && std::env::var("TW_VIDEO_ONLY").map(|v| !v.is_empty()).unwrap_or(false)
        {
            self.video_only = true;
            self.video_only_hint = Some(Instant::now());
            set_video_fullscreen(ctx, true);
        }
        // Одиночный клик по видео = пауза. Ждём 250 мс: если придёт второй
        // клик, сработает только полный экран, а не пауза с продолжением.
        if let Some(t) = self.pending_pause {
            if t.elapsed() >= Duration::from_millis(250) {
                self.pending_pause = None;
                self.toggle_pause();
            }
        }
        // Частота отрисовки UI (для строки статистики).
        self.ui_frames += 1;
        if self.ui_frames_at.elapsed() >= Duration::from_secs(1) {
            self.ui_fps = self.ui_frames as f32 / self.ui_frames_at.elapsed().as_secs_f32();
            self.ui_frames = 0;
            self.ui_frames_at = Instant::now();
        }
        // Фоновые задачи (streamlink, метаданные Twitch): ничего не блокируем.
        self.poll_worker();
        // Статус канала («в эфире / не в эфире») — заодно греет кэш ссылок.
        self.check_channel_status();

        // GPU-режим: файл грузим только после создания GL-контекста mpv
        // (он появляется в paint-колбэке на следующем кадре), иначе mpv
        // не сможет инициализировать видеовыход и завершит поток ошибкой.
        if self.pending_url.is_some() && self.gpu.ready() {
            if let Some(url) = self.pending_url.take() {
                if let Some(p) = self.player.as_mut() {
                    if let Err(e) = p.load(&url) {
                        self.error = Some(format!("mpv load: {}", e));
                        self.status = "Ошибка".to_string();
                    }
                }
            }
        }

        // Отложенная спекулятивная предзагрузка канала (когда ничего не играет).
        let playing_now = self.player.as_ref().map(|p| p.is_playing()).unwrap_or(false);
        if let Some(t) = self.prefetch_at {
            if t.elapsed() > Duration::from_millis(900) {
                self.prefetch_at = None;
                if !playing_now {
                    self.start_prefetch();
                }
            }
        }

        // Опрос событий libmpv (например, конец потока).
        let mut need_refresh = false;
        if let Some(player) = self.player.as_mut() {
            player.poll_events();
            if player.ended() && !self.ended_reported {
                self.ended_reported = true;
                debug_log(&format!(
                    "mpv: поток завершён (failed={}, gpu={})",
                    player.failed(),
                    self.gpu_render
                ));
                self.status = "Поток завершён".to_string();
            }
            need_refresh = player.failed() && self.launched_from_cache && !self.busy;
        }
        if need_refresh {
            // Ссылка из кэша устарела — молча получаем свежую и продолжаем.
            self.cached = None;
            self.prefetched = None;
            self.launched_from_cache = false;
            self.start_play();
        }

        // Буферизация/просадки сети: индикатор, снижение качества, переподключение.
        self.monitor_stream();

        // Отладочный запуск по ссылке из окружения (проверка рендера без Twitch):
        // TW_DEMO_URL="av://lavfi:testsrc=size=640x360:rate=30"
        if self.player.is_none() {
            if let Ok(url) = std::env::var("TW_DEMO_URL") {
                if !url.is_empty() {
                    let created = if self.gpu_render {
                        MpvPlayer::new_opengl(self.hwdec)
                    } else {
                        MpvPlayer::new(self.hwdec)
                    };
                    match created {
                        Ok(mut p) => {
                            p.set_volume(self.volume as i64);
                            if self.gpu_render {
                                // Загрузим, когда появится GL-контекст mpv.
                                self.pending_url = Some(url);
                                self.player = Some(p);
                                self.status = "Демо-поток (TW_DEMO_URL)".to_string();
                            } else if p.load(&url).is_ok() {
                                self.player = Some(p);
                                self.status = "Демо-поток (TW_DEMO_URL)".to_string();
                            }
                        }
                        Err(e) => self.error = Some(format!("libmpv: {}", e)),
                    }
                    self.attach_frame_wake();
                }
            }
        }

        // Свёрнутое или скрытое в трей окно: UI не строим, видео отключаем —
        // идёт только звук.
        self.minimized = win::window_minimized() || win::window_hidden();
        if self.minimized {
            self.update_minimized();
            self.track_window_and_save(ctx);
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }
        self.restore_video();

        // GPU-рендер не поднялся (нет GL 3.3, ошибка mpv) — молча на ЦП.
        if self.gpu_render && self.gpu.has_failed() {
            let why = self.gpu.error().unwrap_or_default();
            debug_log(&format!("gpu: отказ, возврат на ЦП: {why}"));
            self.error = Some(format!(
                "GPU-рендер недоступен, включена отрисовка на ЦП: {}",
                why
            ));
            self.gpu_render = false;
            self.gpu = gpu::GpuVideo::new();
            self.recreate_player();
        }

        // Профиль отрисовки: явный выбор пользователя или «Авто» под ПК.
        self.profile_now = self.resolve_profile();

        // В полноэкранном режиме видео панель тянется на всё окно без отступов.
        let panel_frame = if self.video_only {
            egui::Frame::none()
        } else {
            egui::Frame::central_panel(&ctx.style())
        };
        let panel = egui::CentralPanel::default().frame(panel_frame);
        panel.show(ctx, |ui| {
            // В полноэкранном режиме видео все настройки скрыты — только кадр.
            if !self.video_only {
                // Статический заголовок: без format! на каждый кадр.
                ui.heading(HEADER);
                ui.separator();

                // Качество: «Авто» (по скорости сети) либо явный вариант. Явный
                // выбор выключает «Авто» — поведение всегда предсказуемо.
                // Комбобокс стоит в строке канала, сразу справа от звёздочки:
                // отдельной строки настроек у пользователя нет.
                // Снимок выбора строкой: Option<&str> держал бы заимствование
                // self на весь кадр и мешал изменять другие поля плеера.
                let mut choice: Option<String> = if self.auto_quality {
                    None
                } else {
                    Some(self.quality.clone())
                };
                let quality_label = if self.auto_quality {
                    format!("Авто — {}", self.quality)
                } else {
                    self.quality.clone()
                };

                ui.horizontal(|ui| {
                    ui.label("Канал Twitch:");
                    if ui.text_edit_singleline(&mut self.channel).changed() {
                        // Пользователь печатает — через мгновение прогреем поток заранее.
                        self.prefetch_at = Some(Instant::now());
                        self.online = None;
                    }
                    // Статус канала: точка видно сразу, подробности — в подсказке.
                    // Круг рисуем сами: у шрифта нет глифов «●»/«○», и вместо них
                    // получался пустой квадратик.
                    if let Some(live) = self.online {
                        let (dot, resp) =
                            ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                        let center = dot.center();
                        if live {
                            ui.painter().circle_filled(
                                center,
                                4.5,
                                // Красная точка — как индикатор «LIVE» на Twitch.
                                egui::Color32::from_rgb(235, 60, 60),
                            );
                            resp.on_hover_text(format!(
                                "Канал в эфире.\nДоступно качеств: {}",
                                self.qualities.len()
                            ));
                        } else {
                            let stroke = egui::Stroke::new(1.4_f32, egui::Color32::from_gray(130));
                            ui.painter().circle_stroke(center, 4.0, stroke);
                            resp.on_hover_text("Канал не в эфире (или недоступен)");
                        }
                    }
                    // Сравнения без to_lowercase(): логины Twitch — ASCII.
                    let key = self.channel.trim();
                    let is_fav = !key.is_empty() && self.favorites.iter().any(|f| f.eq_ignore_ascii_case(key));
                    let star = if is_fav { "★" } else { "☆" };
                    if ui.button(star).clicked() && !key.is_empty() {
                        if is_fav {
                            self.favorites.retain(|f| !f.eq_ignore_ascii_case(key));
                        } else {
                            self.favorites.push(key.to_lowercase());
                        }
                        save_favorites(&self.favorites);
                    }
                    ui.separator();
                    // Список берём из ответа streamlink, а до первого запуска —
                    // базовый набор-подсказку (без лишних аллокаций в кадре).
                    egui::ComboBox::from_id_source("quality")
                        .width(150.0)
                        .selected_text(quality_label)
                        .show_ui(ui, |ui| {
                            // selectable_label вместо selectable_value: сравнение
                            // по &str не требует String на каждый пункт в кадре.
                            if ui
                                .selectable_label(choice.is_none(), "Авто (по скорости сети)")
                                .on_hover_text(
                                    "Уровень подбирается по замеренной скорости сети.\n\
                                     Качество растёт после 30 с стабильной работы и снижается,\n\
                                     если буфер тает или mpv встал на паузу из-за пустого кэша.",
                                )
                                .clicked()
                            {
                                choice = None;
                            }
                            if self.qualities.is_empty() {
                                for q in DEFAULT_QUALITIES {
                                    if ui
                                        .selectable_label(choice.as_deref() == Some(q), q)
                                        .clicked()
                                    {
                                        choice = Some(q.to_string());
                                    }
                                }
                            } else {
                                for q in &self.qualities {
                                    if ui
                                        .selectable_label(
                                            choice.as_deref() == Some(q.as_str()),
                                            q.as_str(),
                                        )
                                        .clicked()
                                    {
                                        choice = Some(q.clone());
                                    }
                                }
                            }
                        });
                    // Всё «разработческое» — в отдельном окне, чтобы не мешать.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button("Консоль")
                            .on_hover_text(
                                "Консоль разработчика: декод, отрисовка, статистика,\n\
                                 ссылка на поток, Client ID, метаданные.",
                            )
                            .clicked()
                        {
                            self.dev_panel = !self.dev_panel;
                        }
                    });
                });

                if !self.favorites.is_empty() {
                    // Клик обрабатываем после отрисовки списка: без клонирования
                    // Vec<String> на каждый кадр.
                    let mut clicked: Option<String> = None;
                    // Канал, который просят убрать из избранного (ПКМ по кнопке).
                    let mut remove: Option<String> = None;
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Избранные:");
                        for f in &self.favorites {
                            let resp = ui.button(f).on_hover_text(
                                "Левый клик — включить канал\nПравый клик — убрать из избранного",
                            );
                            if resp.clicked() {
                                clicked = Some(f.clone());
                            }
                            // Удаление из избранного: контекстное меню по ПКМ.
                            // Отдельного крестика нет — иначе список «плывёт».
                            resp.context_menu(|ui| {
                                ui.set_min_width(170.0);
                                if ui.button("Убрать из избранного").clicked() {
                                    remove = Some(f.clone());
                                    ui.close_menu();
                                }
                            });
                        }
                    });
                    if let Some(f) = remove {
                        self.favorites.retain(|x| !x.eq_ignore_ascii_case(&f));
                        save_favorites(&self.favorites);
                        self.status = format!("«{}» убран из избранного", f);
                        debug_log(&format!("ui: из избранного убран {}", f));
                    }
                    if let Some(f) = clicked {
                        self.channel = f;
                        // Клик по избранному — сразу греем поток в фоне.
                        self.start_prefetch();
                    }
                }

                let mut quality_changed = false;
                // choice уже владеющая строка — просто забираем выбор.
                match choice {
                    None if !self.auto_quality => {
                        self.auto_quality = true;
                        // Старые замеры не нужны: подбор начнётся с текущей скорости.
                        self.net_rate.clear();
                        self.auto_down_votes = 0;
                        self.last_auto_quality = None;
                        self.status = "Авто: подбираю качество по скорости сети…".to_string();
                        self.settings_dirty = true;
                    }
                    Some(q) if q != self.quality => {
                        self.quality = q;
                        self.auto_quality = false;
                        quality_changed = true;
                        self.settings_dirty = true;
                    }
                    _ => {}
                }
                if quality_changed && self.player.as_ref().map(|p| p.is_playing()).unwrap_or(false) {
                    self.switch_quality();
                }

                ui.separator();

                let playing = self.player.as_ref().map(|p| p.is_playing()).unwrap_or(false);
                // Сравнение без to_lowercase(): логины Twitch — ASCII.
                let channel_changed = playing
                    && self
                        .playing_channel
                        .as_deref()
                        .map(|c| !self.channel.trim().eq_ignore_ascii_case(c))
                        .unwrap_or(true);
                ui.horizontal(|ui| {
                    if self.busy && self.pending_play {
                        // Идёт подключение: повторные клики невозможны.
                        let _ = ui.add_enabled(false, egui::Button::new("Подключение…"));
                        if ui.button("Отмена").clicked() {
                            self.cancel_pending();
                        }
                    } else if playing {
                        if ui.button("Остановить").clicked() {
                            self.stop_stream();
                        }
                        // Пока играет другой канал — переключаемся одним кликом,
                        // без предварительного «Остановить».
                        if channel_changed && ui.button("Воспроизвести").clicked() {
                            self.start_play();
                        }
                    } else if ui.button("Воспроизвести").clicked() {
                        self.start_play();
                    }
                    ui.separator();
                    if ui
                        .checkbox(&mut self.pinned, "Поверх окон")
                        .on_hover_text("Окно всегда сверху — удобно смотреть поверх других программ")
                        .changed()
                    {
                        self.settings_dirty = true;
                    }
                    ui.separator();
                    // Громкость меняется колесом мыши над видео — слайдера нет.
                    ui.label(format!("Громкость: {}%", self.volume))
                        .on_hover_text("Колесо мыши над видео — громкость");
                });
            }

            // Область видео. Кадр вписывается по своим пропорциям (из mpv, пока
            // потока нет — 16:9) и не растягивается: если после разворота окна
            // высоты не хватает, уменьшается и ширина, а по бокам остаются поля.
            // В полноэкранном режиме видео кадр занимает всё окно по центру.
            let aspect = self
                .player
                .as_ref()
                .and_then(|p| p.video_aspect())
                .unwrap_or(16.0 / 9.0);
            let rect;
            let rounding;
            if self.video_only {
                let full = ui.max_rect();
                ui.painter()
                    .rect_filled(full, egui::Rounding::ZERO, egui::Color32::BLACK);
                let size = if full.width() / full.height() > aspect {
                    egui::vec2(full.height() * aspect, full.height())
                } else {
                    egui::vec2(full.width(), full.width() / aspect)
                };
                rect = egui::Rect::from_center_size(full.center(), size);
                rounding = egui::Rounding::ZERO;
            } else {
                ui.add_space(4.0);
                // Под видео остаётся одна строка статуса — место под неё
                // резервируем заранее, иначе она уезжает за край окна.
                let line = ui.text_style_height(&egui::TextStyle::Body);
                let reserve = 10.0 + line; // разделитель + строка статуса
                let avail_w = ui.available_width();
                let avail_h = (ui.available_height() - reserve).max(0.0);
                let mut area_w = avail_w;
                let mut area_h = area_w / aspect;
                if avail_h > 0.0 && area_h > avail_h {
                    area_h = avail_h;
                    area_w = area_h * aspect;
                }
                let (row, _) =
                    ui.allocate_exact_size(egui::vec2(avail_w, area_h), egui::Sense::hover());
                rect = egui::Rect::from_center_size(row.center(), egui::vec2(area_w, area_h));
                rounding = egui::Rounding::same(12.0);
            }
            ui.painter()
                .rect_filled(rect, rounding, egui::Color32::from_rgb(12, 12, 16));

            let mut rendered: Option<Result<egui::ColorImage, String>> = None;
            if let Some(player) = self.player.as_mut() {
                // В GPU-режиме колбэк нужен и до загрузки файла: именно он
                // создаёт GL-контекст mpv, без которого поток не запустится.
                if self.gpu_render || player.is_playing() {
                    let ppp = ctx.pixels_per_point();
                    // GPU-путь масштабирует на видеокарте, поэтому снижать
                    // разрешение кадра незачем: рендерим в полном размере.
                    // `profile_now` — это либо явный выбор, либо «Авто».
                    let profile = self.profile_now;
                    let scale = if self.gpu_render {
                        1.0
                    } else {
                        render_scale(profile)
                    };
                    let (max_w, max_h) = if self.gpu_render {
                        (1920, 1080)
                    } else {
                        (1600, 900)
                    };
                    let pw = align8(((rect.width() * ppp * scale).round() as u32).min(max_w));
                    let ph = align8(((rect.height() * ppp * scale).round() as u32).min(max_h));
                    let interval = render_interval(profile);
                    // Троттлинг: не чаще, чем позволяет профиль (меньше ЦП на
                    // программное масштабирование и загрузку текстуры).
                    let due = self
                        .last_render
                        .map(|t| t.elapsed() >= interval)
                        .unwrap_or(true);
                    let resized = self.tex_size != (pw, ph);

                    if self.gpu_render {
                        // Кадр рисует paint-колбэк (там текущий GL-контекст).
                        // Колбэк нужно добавлять в каждый кадр отрисовки egui:
                        // в отличие от текстуры, он не «живёт» между кадрами.
                        // Новый кадр просим у mpv только когда он есть и когда
                        // подошёл интервал профиля — иначе просто перерисовываем
                        // уже готовую текстуру (почти бесплатно).
                        let first = resized;
                        let produce =
                            first || (due && (player.frame_ready() || player.take_dirty()));
                        let ptr = gpu::PlayerPtr(player as *mut MpvPlayer);
                        ui.painter().add(self.gpu.callback(
                            rect,
                            ptr,
                            (pw, ph),
                            rounding.nw * ppp,
                            produce,
                        ));
                        self.tex_size = (pw, ph);
                        if produce {
                            self.last_render = Some(Instant::now());
                        }
                    } else if resized
                        || self.texture.is_none()
                        || (due && (player.frame_ready() || player.take_dirty()))
                    {
                        rendered = Some(player.render(pw, ph));
                        self.tex_size = (pw, ph);
                        self.last_render = Some(Instant::now());
                    }
                }
            }
            if let Some(res) = rendered {
                match res {
                    Ok(img) => {
                        if let Some(tex) = &mut self.texture {
                            tex.set(img, egui::TextureOptions::LINEAR);
                        } else {
                            self.texture =
                                Some(ctx.load_texture("video", img, egui::TextureOptions::LINEAR));
                        }
                        self.error = None;
                    }
                    Err(e) => {
                        self.error = Some(format!("Рендер libmpv: {}", e));
                    }
                }
            }

            if let Some(tex) = &self.texture {
                // В GPU-режиме кадр уже нарисован paint-колбэком.
                if !self.gpu_render {
                    let sized = egui::load::SizedTexture::new(tex.id(), rect.size());
                    let image = egui::Image::new(sized).rounding(rounding);
                    ui.put(rect, image);
                }
            }

            // Нет кадра (ничего не играет, «только звук» или канал оффлайн) —
            // тёмный фон и подпись состояния. Пока поток ждёт данные, эту
            // подпись закрывает заставка буферизации.
            let splash = self.buffering_overlay();
            let showing_video = if self.gpu_render {
                self.gpu.ready() && self.player.is_some() && !self.video_disabled
            } else {
                self.texture.is_some()
            };
            if !showing_video && !self.video_disabled && splash.is_none() {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    self.placeholder_text(),
                    egui::FontId::proportional(18.0),
                    egui::Color32::from_gray(150),
                );
            }
            if let Some((title, secs)) = splash {
                // Заставка: видно, что плеер жив и ждёт сеть, а не «завис».
                buffering_splash(ui, rect, rounding, title, secs);
                // Индикатор крутится даже без новых кадров потока.
                ui.ctx().request_repaint_after(Duration::from_millis(80));
            }

            // Чипы поверх кадра: пауза и громкость (после колеса).
            let mut chip_top = rect.top() + 14.0;
            if self.video_only
                && self
                    .video_only_hint
                    .map(|t| t.elapsed() < Duration::from_millis(2500))
                    .unwrap_or(false)
            {
                chip_top += chip(ui, rect, chip_top, "Двойной клик или Esc — выйти") + 8.0;
            }
            if self.video_paused() {
                chip_top += chip(ui, rect, chip_top, "Пауза") + 8.0;
            }
            if self
                .volume_osd
                .map(|t| t.elapsed() < Duration::from_millis(1200))
                .unwrap_or(false)
            {
                let text = format!("Громкость {}%", self.volume);
                chip(ui, rect, chip_top, &text);
            }

            // Мышь над видео: колесо — громкость, клик — пауза,
            // двойной клик — полный экран только для видео.
            // Интерактивную область регистрируем последней: в программном пути
            // кадр рисуется виджетом-картинкой, и он иначе перехватывает hover.
            // В полноэкранном режиме область — всё окно, включая чёрные поля.
            let hit = if self.video_only { ui.max_rect() } else { rect };
            let video_resp =
                ui.interact(hit, egui::Id::new("video_area"), egui::Sense::click_and_drag());
            if video_resp.hovered() {
                let scroll = ui.input(|i| i.raw_scroll_delta.y);
                if scroll.abs() > 0.0 {
                    let delta = if scroll > 0.0 { VOLUME_STEP } else { -VOLUME_STEP };
                    self.change_volume(delta);
                }
            }
            if video_resp.clicked() {
                self.pending_pause = Some(Instant::now());
                // Перерисовка через 260 мс: иначе одиночный клик применится
                // только к следующему кадру или таймеру.
                ui.ctx().request_repaint_after(Duration::from_millis(260));
            }
            if video_resp.double_clicked() {
                self.pending_pause = None;
                // Двойной клик разворачивает только видео: интерфейс скрывается,
                // кадр занимает весь экран (не «программа целиком»).
                self.video_only = !self.video_only;
                self.video_only_hint = Some(Instant::now());
                set_video_fullscreen(ui.ctx(), self.video_only);
            }

            if !self.video_only {
                if let Some(err) = &self.error {
                    ui.add_space(4.0);
                    ui.colored_label(egui::Color32::from_rgb(255, 80, 80), err);
                }

                // Под плеером — только строка состояния. Статистика, декод,
                // ссылка на поток и метаданные живут в консоли разработчика.
                ui.separator();
                ui.label(&self.status);
            }
        });

        // Консоль разработчика — отдельным окном поверх плеера.
        if !self.video_only {
            self.dev_console(ctx);
        }

        // Субтитры: захват звука процесса стартует/останавливается по настройке.
        self.subs.tick();
        if self.subs.enabled {
            // Индикатор уровня живой даже без кадров видео (audio-only поток).
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        // Геометрия окна и сохранение настроек (запись с троттлингом).
        self.track_window_and_save(ctx);

        // Состояние в подсказке иконки трея и в пункте меню «Играть/Остановить».
        if self.tray_started {
            let playing = self
                .player
                .as_ref()
                .map(|p| p.is_playing())
                .unwrap_or(false);
            tray::set_playing(playing);
            let what = if playing {
                format!("{} · {}", self.channel.trim(), self.quality)
            } else if self.online == Some(false) {
                format!("{} — не в эфире", self.channel.trim())
            } else {
                "не воспроизводится".to_string()
            };
            tray::set_tooltip(&format!("Mini Twitch Player — {}", what));
        }

        // Диагностика частоты перерисовки (только при TW_DEBUG_LOG).
        self.updates = self.updates.wrapping_add(1);
        if self.updates % 300 == 0 && debug_enabled() {
            let dt = self
                .last_update_at
                .map(|t| t.elapsed().as_secs_f64() * 1000.0)
                .unwrap_or(0.0);
            debug_log(&format!(
                "ui: обновлений {}, шаг {:.2} мс, кадр {:.2} мс, интервал {:?}, причины: {:?}",
                self.updates,
                dt,
                self.ui_cost_ms,
                render_interval(self.profile_now),
                ctx.repaint_causes()
            ));
        }
        self.last_update_at = Some(Instant::now());
        // Время кадра интерфейса (сглаженное): вход для «Авто»-профиля отрисовки.
        // Применится на следующем кадре (см. resolve_profile в начале update).
        let frame_ms = frame_start.elapsed().as_secs_f32() * 1000.0;
        self.ui_cost_ms = self.ui_cost_ms * 0.9 + frame_ms * 0.1;

        if self.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        if self
            .player
            .as_ref()
            .map(|p| p.is_playing())
            .unwrap_or(false)
        {
            // Кадры будит сам mpv (см. `attach_frame_wake`), поэтому частый
            // таймер не нужен: только редкий — для надписей вроде «Буфер».
            ctx.request_repaint_after(Duration::from_millis(500));
        } else if self.player.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    /// GL-контекст здесь ещё текущий — освобождаем текстуры/шейдеры mpv.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.gpu.release();
        self.player = None;
        if self.tray_started {
            tray::remove();
        }
    }
}

impl TwitchMiniPlayer {
    /// Текущее состояние в виде сохраняемой структуры.
    fn current_settings(&self) -> Settings {
        Settings {
            version: SETTINGS_VERSION,
            channel: if self.playing_channel.is_some() {
                self.channel.trim().to_lowercase()
            } else {
                self.channel.clone()
            },
            quality: self.quality.clone(),
            volume: self.volume,
            render_profile: self.render_profile,
            hwdec: self.hwdec,
            auto_quality: self.auto_quality,
            gpu_render: self.gpu_render,
            tray: self.tray,
            pinned: self.pinned,
            subs_capture: self.subs.enabled,
            client_id: self.client_id.clone(),
            window: self.saved_window,
        }
    }

    /// Запоминает геометрию окна и помечает настройки изменёнными.
    /// Вызывается каждый кадр, но запись на диск — не чаще раза в 1.5 с.
    fn track_window_and_save(&mut self, ctx: &egui::Context) {
        // Свёрнутое окно и полноэкранный режим отдают «мусорную» геометрию —
        // её не сохраняем, иначе плеер откроется на весь экран.
        let rect = ctx.input(|i| i.viewport().outer_rect);
        let usable = !self.video_only
            && !ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        if let Some(r) = rect.filter(|_| usable) {
            if r.width() >= 320.0 && r.height() >= 240.0 {
                let cur = WindowRect {
                    x: r.min.x,
                    y: r.min.y,
                    w: r.width(),
                    h: r.height(),
                };
                let changed = match self.saved_window {
                    Some(p) => {
                        (p.x - cur.x).abs() > 2.0
                            || (p.y - cur.y).abs() > 2.0
                            || (p.w - cur.w).abs() > 2.0
                            || (p.h - cur.h).abs() > 2.0
                    }
                    None => true,
                };
                if changed {
                    self.saved_window = Some(cur);
                    self.settings_dirty = true;
                }
            }
        }

        // При закрытии пишем сразу, иначе — с задержкой, чтобы не мешать UI.
        let closing = ctx.input(|i| i.viewport().close_requested());
        let due = self
            .last_save
            .map(|t| t.elapsed() >= Duration::from_millis(1500))
            .unwrap_or(true);
        if self.settings_dirty && (closing || due) {
            save_settings(&self.current_settings());
            self.settings_dirty = false;
            self.last_save = Some(Instant::now());
        }
    }

    /// Запускает воспроизведение, не блокируя UI: streamlink и Twitch API
    /// работают в фоновом потоке, результат приходит через канал.
    fn start_play(&mut self) {
        let channel = self.channel.trim().to_lowercase();
        // Запоминаем последний канал: при следующем запуске он уже в поле ввода.
        self.settings_dirty = true;
        self.ended_reported = false;
        if self.busy {
            if self.inflight_channel.as_deref() != Some(channel.as_str()) {
                // Предзагрузка/подключение было для другого канала — перезапускаем.
                self.job = self.job.wrapping_add(1);
                self.busy = false;
                self.spawn_resolve(channel, true);
            } else {
                // Уже идёт разрешение нужного канала — просто дождёмся его.
                self.pending_play = true;
                self.status = "Подключаюсь…".to_string();
            }
            return;
        }
        if channel.is_empty() {
            self.status = "Введите имя канала".to_string();
            return;
        }

        let sl_exe = self.streamlink_path.clone();
        if sl_exe != "streamlink" && !Path::new(&sl_exe).exists() {
            self.error = Some(format!(
                "streamlink не найден по пути: {}\nУстановите streamlink: https://streamlink.github.io/",
                sl_exe
            ));
            self.status = "Ошибка: streamlink не найден".to_string();
            return;
        }

        // Свежая предзагрузка (пользователь только что выбрал канал) — старт мгновенный.
        if let Some((ch, json, at)) = self.prefetched.clone() {
            if ch == channel && at.elapsed() < CACHE_TTL {
                self.prefetched = None;
                self.error = None;
                self.launched_from_cache = true;
                self.apply_resolved(&ch, json);
                return;
            }
        }
        // Свежий кэш — тоже без запуска streamlink.
        if let Some((ch, json, at)) = self.cached.clone() {
            if ch == channel && at.elapsed() < CACHE_TTL {
                self.error = None;
                self.launched_from_cache = true;
                self.apply_resolved(&ch, json);
                return;
            }
        }

        self.spawn_resolve(channel, true);
    }

    /// Фоновое разрешение потока. `play = false` — только прогрев кэша.
    fn spawn_resolve(&mut self, channel: String, play: bool) {
        self.job = self.job.wrapping_add(1);
        let job = self.job;
        self.busy = true;
        self.pending_play = play;
        self.busy_since = Some(Instant::now());
        self.inflight_channel = Some(channel.clone());
        if play {
            self.error = None;
            self.launched_from_cache = false;
            self.status = format!("Подключаюсь к {}…", channel);
        }

        // Метаданные нужны только при реальном запуске.
        let client_id = if play {
            self.client_id.clone()
        } else {
            String::new()
        };
        let sl_exe = self.streamlink_path.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            match run_streamlink(&sl_exe, &channel) {
                Ok(json) => {
                    let _ = tx.send(WorkerMsg::Resolved {
                        job,
                        channel: channel.clone(),
                        json,
                    });
                    if !client_id.is_empty() {
                        let meta = fetch_metadata(&client_id, &channel).ok();
                        let _ = tx.send(WorkerMsg::Meta { job, meta });
                    }
                }
                Err(error) => {
                    let _ = tx.send(WorkerMsg::Failed { job, error });
                }
            }
        });
    }

    /// Спекулятивно разрешает поток выбранного канала, чтобы Play сработал мгновенно.
    fn start_prefetch(&mut self) {
        if self.busy {
            return;
        }
        let channel = self.channel.trim().to_lowercase();
        if !is_valid_channel(&channel) {
            return;
        }
        let sl_exe = self.streamlink_path.clone();
        if sl_exe != "streamlink" && !Path::new(&sl_exe).exists() {
            return;
        }
        if let Some((ch, _, at)) = &self.prefetched {
            if ch == &channel && at.elapsed() < CACHE_TTL {
                return;
            }
        }
        if let Some((ch, _, at)) = &self.cached {
            if ch == &channel && at.elapsed() < CACHE_TTL {
                return;
            }
        }
        self.spawn_resolve(channel, false);
    }

    /// Применяет готовый JSON streamlink: обновляет качества и грузит поток в mpv.
    fn apply_resolved(&mut self, channel: &str, json: String) {
        let key = match pick_stream_key(&json, &self.quality) {
            Some(k) => k,
            None => {
                self.error = Some("В потоке нет доступного качества (канал оффлайн?)".to_string());
                self.status = "Ошибка".to_string();
                return;
            }
        };
        let url = match parse_streamlink_json(&json, &key) {
            Some(u) => u,
            None => {
                self.error = Some("Не удалось извлечь HLS-ссылку из ответа streamlink".to_string());
                self.status = "Ошибка".to_string();
                return;
            }
        };
        self.qualities = parse_streams_keys(&json);
        self.quality = key.clone();
        self.online = Some(true);
        self.stream_url = Some(url.clone());
        self.streams_json = Some(json.clone());
        self.cached = Some((channel.to_string(), json, Instant::now()));

        if self.player.is_none() {
            let created = if self.gpu_render {
                MpvPlayer::new_opengl(self.hwdec)
            } else {
                MpvPlayer::new(self.hwdec)
            };
            match created {
                Ok(p) => self.player = Some(p),
                Err(e) => {
                    let _ = std::fs::write(
                        std::env::temp_dir().join("tw_libmpv2.txt"),
                        format!("{}\n{}", e, libmpv::probe()),
                    );
                    self.error = Some(format!("libmpv: {}", e));
                    self.status = "Ошибка libmpv".to_string();
                    return;
                }
            }
        }
        // «Только звук»: видеодорожку выключаем сразу, чтобы mpv не тратил
        // силы на кадры, которых всё равно нет в этом качестве.
        let wants_video = self.wants_video();
        if let Some(player) = self.player.as_mut() {
            player.set_volume(self.volume as i64);
            if !wants_video {
                player.set_video_enabled(false);
                self.video_disabled = false;
            }
        }
        if self.gpu_render {
            // Грузим, когда GL-контекст mpv будет создан (см. update()).
            self.pending_url = Some(url);
        } else if let Some(player) = self.player.as_mut() {
            if let Err(e) = player.load(&url) {
                self.error = Some(format!("mpv load: {}", e));
                self.status = "Ошибка".to_string();
                return;
            }
        }
        self.attach_frame_wake();
        self.playing_channel = Some(channel.to_lowercase());
        self.pending_play = false;
        self.prefetched = None;
        self.texture = None;
        self.tex_size = (0, 0);
        self.last_render = None;
        self.playing_since = Some(Instant::now());
        // Новый поток — старые замеры скорости к нему не относятся.
        self.net_rate.clear();
        self.auto_down_votes = 0;
        self.buf_trend.clear();
        self.catchup_at = None;
        self.status = format!("Воспроизведение ({})…", key);
    }

    /// Забирает результаты фоновых задач (не блокирует UI).
    fn poll_worker(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(WorkerMsg::Resolved { job, channel, json }) => {
                    if job != self.job {
                        continue; // результат устаревшего запроса
                    }
                    self.busy = false;
                    self.busy_since = None;
                    self.inflight_channel = None;
                    if self.pending_play {
                        self.pending_play = false;
                        self.apply_resolved(&channel, json);
                    } else {
                        // Это была предзагрузка: греем кэш (список качеств не трогаем,
                        // если сейчас играет другой канал).
                        let playing = self
                            .player
                            .as_ref()
                            .map(|p| p.is_playing())
                            .unwrap_or(false);
                        if !playing {
                            self.qualities = parse_streams_keys(&json);
                            self.streams_json = Some(json.clone());
                        }
                        self.online = Some(true);
                        self.prefetched = Some((channel, json, Instant::now()));
                    }
                }
                Ok(WorkerMsg::Failed { job, error }) => {
                    if job != self.job {
                        continue;
                    }
                    self.busy = false;
                    self.busy_since = None;
                    self.inflight_channel = None;
                    // «Канал оффлайн» — это статус, а не ошибка плеера: показываем
                    // точку у поля «Канал» и заглушку вместо потока.
                    if error.contains("оффлайн") || error.contains("No playable streams") {
                        self.online = Some(false);
                    }
                    if self.pending_play {
                        self.pending_play = false;
                        self.error = Some(error);
                        self.status = "Ошибка streamlink".to_string();
                    }
                }
                Ok(WorkerMsg::Meta { job, meta }) => {
                    if job == self.job {
                        self.meta = meta;
                    }
                }
                Err(_) => break,
            }
        }
        if let Some(started) = self.busy_since {
            if started.elapsed() > PLAY_TIMEOUT {
                self.job = self.job.wrapping_add(1);
                let was_play = self.pending_play;
                self.busy = false;
                self.pending_play = false;
                self.busy_since = None;
                self.inflight_channel = None;
                if was_play {
                    self.error = Some("streamlink не ответил вовремя".to_string());
                    self.status = "Таймаут подключения".to_string();
                }
            }
        }
    }

    fn stop_stream(&mut self) {
        // Отменяем незавершённые фоновые запросы (их результаты будут отброшены).
        self.job = self.job.wrapping_add(1);
        self.busy = false;
        self.busy_since = None;
        self.pending_play = false;
        self.playing_channel = None;
        self.inflight_channel = None;
        self.stall_since = None;
        self.stable_since = None;
        self.playing_since = None;
        self.buf_trend.clear();
        self.buf_sample_at = None;
        self.net_rate.clear();
        self.auto_down_votes = 0;
        self.pending_pause = None;
        if let Some(player) = self.player.as_mut() {
            player.stop();
        }
        self.texture = None;
        self.tex_size = (0, 0);
        self.last_render = None;
        self.pending_url = None;
        self.catchup_at = None;
        self.status = "Остановлено".to_string();
    }

    /// Отменяет фоновое подключение, не трогая текущее воспроизведение.
    fn cancel_pending(&mut self) {
        self.job = self.job.wrapping_add(1);
        self.busy = false;
        self.busy_since = None;
        self.pending_play = false;
        self.inflight_channel = None;
        let playing = self
            .player
            .as_ref()
            .map(|p| p.is_playing())
            .unwrap_or(false);
        self.status = if playing {
            "Воспроизведение".to_string()
        } else {
            "Отменено".to_string()
        };
    }

    /// Пересоздаёт mpv (нужно при смене GPU-декода) и продолжает текущий поток.
    fn recreate_player(&mut self) {
        let url = self.stream_url.clone();
        let was_playing = self
            .player
            .as_ref()
            .map(|p| p.is_playing())
            .unwrap_or(false);
        self.player = None; // Drop освобождает контекст mpv
        self.texture = None;
        self.tex_size = (0, 0);
        self.last_render = None;
        self.catchup_at = None;
        self.gpu.reset();
        self.video_disabled = false;
        let Some(url) = url else {
            return;
        };
        match if self.gpu_render {
            MpvPlayer::new_opengl(self.hwdec)
        } else {
            MpvPlayer::new(self.hwdec)
        } {
            Ok(mut p) => {
                p.set_volume(self.volume as i64);
                if self.gpu_render {
                    // Загрузим после создания GL-контекста (см. update()).
                    self.pending_url = Some(url);
                } else if let Err(e) = p.load(&url) {
                    self.error = Some(format!("mpv load: {}", e));
                    self.status = "Ошибка".to_string();
                    return;
                }
                self.player = Some(p);
                if was_playing {
                    self.status = "Перезапуск с новыми настройками…".to_string();
                }
            }
            Err(e) => {
                self.error = Some(format!("libmpv: {}", e));
                self.status = "Ошибка libmpv".to_string();
            }
        }
        self.attach_frame_wake();
    }

    /// Подключает пробуждение UI по событию «у mpv готов новый кадр».
    /// Так отрисовка идёт ровно по кадрам потока — без таймеров и лишних
    /// перерисовок (это заметно снижает нагрузку на ЦП).
    fn attach_frame_wake(&mut self) {
        let Some(ctx) = self.egui_ctx.clone() else {
            return;
        };
        if let Some(p) = self.player.as_ref() {
            p.set_frame_wake(move || ctx.request_repaint());
        }
    }

    /// Свёрнутое окно: видео не нужно, но звук должен продолжаться.
    /// Отключаем видеодорожку (`vid=no`) — mpv перестаёт отдавать кадры,
    /// поэтому ЦП почти не тратится (и в GPU-, и в программном режиме).
    fn update_minimized(&mut self) {
        if !self.video_disabled {
            if let Some(p) = self.player.as_ref() {
                p.set_video_enabled(false);
            }
            self.video_disabled = true;
        }
    }

    /// Вернулись из свёрнутого состояния — включаем видео обратно.
    fn restore_video(&mut self) {
        if self.video_disabled {
            if let Some(p) = self.player.as_ref() {
                // Для «только звук» видео не включаем: дорожки всё равно нет.
                p.set_video_enabled(self.wants_video());
            }
            self.video_disabled = false;
        }
        // Кадр нужен сразу, не ждём окончания интервала троттлинга.
        self.last_render = None;
        self.tex_size = (0, 0);
    }

    /// Нужно ли видео: для качества «только звук» (audio_only) — нет.
    fn wants_video(&self) -> bool {
        !is_audio_only(&self.quality)
    }

    /// Изменяет громкость на `delta` и показывает чип поверх кадра
    /// (слайдера в интерфейсе нет: громкость меняется колесом мыши).
    fn change_volume(&mut self, delta: i32) {
        self.volume_osd = Some(Instant::now());
        let next = (self.volume as i32 + delta).clamp(0, 100) as u8;
        if next == self.volume {
            return;
        }
        self.volume = next;
        if let Some(p) = &self.player {
            p.set_volume(next as i64);
        }
        debug_log(&format!("ui: громкость {}% (колесо)", next));
        self.settings_dirty = true;
    }

    /// Видео на паузе (плеер есть и поток идёт).
    fn video_paused(&self) -> bool {
        self.player
            .as_ref()
            .map(|p| p.is_playing() && p.is_paused())
            .unwrap_or(false)
    }

    /// Пауза / продолжить (одиночный клик по видео).
    fn toggle_pause(&mut self) {
        let Some(p) = self.player.as_ref() else {
            return;
        };
        if !p.is_playing() {
            return;
        }
        let paused = !p.is_paused();
        p.set_paused(paused);
        if !paused {
            // Снятие с паузы не продолжает с того же кадра: иначе картинка
            // отстаёт от эфира на всё время паузы. Перескакиваем к актуальному
            // live-edge HLS-окна. Для тестовых полос перескакивать некуда.
            if self.playing_channel.is_some() {
                match p.seek_to_live() {
                    Ok(()) => debug_log("ui: пауза снята, перескок к эфиру"),
                    Err(e) => debug_log(&format!("ui: перескок к эфиру не удался: {e}")),
                }
            }
        }
        debug_log(&format!("ui: пауза {}", paused));
        self.status = if paused {
            "Пауза".to_string()
        } else {
            format!("Воспроизведение ({})", self.quality)
        };
    }

    /// Подпись на заглушке: что сейчас происходит.
    fn placeholder_text(&self) -> &'static str {
        if is_audio_only(&self.quality) && self.player.is_some() {
            "Только звук (экономия трафика)"
        } else if self.online == Some(false) {
            "Канал не в эфире"
        } else {
            "Область плеера"
        }
    }

    /// Заставка вместо кадра: (подпись, сколько это уже длится).
    /// Показывается, когда mpv ждёт данные (кэш пуст), пока идёт подключение
    /// и пока не пришёл первый кадр потока.
    fn buffering_overlay(&self) -> Option<(&'static str, f32)> {
        // «Только звук» — ждать нечего, кадра не будет по определению.
        if self.video_disabled || !self.wants_video() {
            return None;
        }
        let secs = |t: Option<Instant>| t.map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
        // Кэш пуст — mpv сам поставил воспроизведение на паузу.
        if let Some(t) = self.stall_since {
            return Some(("Буферизация", t.elapsed().as_secs_f32()));
        }
        let Some(player) = self.player.as_ref() else {
            // Плеер ещё не создан: идёт разрешение потока через streamlink.
            if self.busy && self.pending_play {
                return Some(("Подключение", secs(self.busy_since)));
            }
            return None;
        };
        if player.is_playing() {
            return None;
        }
        if self.busy && self.pending_play {
            return Some(("Подключение", secs(self.busy_since)));
        }
        // Файл загружается (ссылка уже есть, но первый кадр не пришёл).
        if self.playing_channel.is_some() && !self.ended_reported && self.error.is_none() {
            return Some(("Загрузка потока", secs(self.playing_since)));
        }
        None
    }

    /// Каким профилем реально рисовать кадр. Явный выбор пользователя не
    /// трогаем, «Авто» подбирается под ПК (см. `auto_profile`).
    fn resolve_profile(&self) -> usize {
        if self.render_profile < RENDER_PROFILES.len() {
            self.render_profile
        } else {
            auto_profile(self.gpu_render, self.ui_cost_ms)
        }
    }

    /// Догоняет эфир, если запас буфера разросся (после просадки сети).
    /// Прыжок идёт по уже скачанным данным, поэтому мгновенен: задержка
    /// возвращается к live, а не остаётся навсегда.
    fn catchup_live(&mut self, buffered: f64, paused: bool) {
        if paused || self.stall_since.is_some() || buffered < CATCHUP_ABOVE_SECS {
            return;
        }
        let ready = self
            .catchup_at
            .map(|t| t.elapsed() >= CATCHUP_COOLDOWN)
            .unwrap_or(true);
        let delta = buffered - LATENCY_TARGET_SECS;
        if !ready || delta < 1.0 {
            return;
        }
        let Some(player) = self.player.as_ref() else {
            return;
        };
        self.catchup_at = Some(Instant::now());
        match player.seek_forward(delta) {
            Ok(()) => debug_log(&format!(
                "live: догон эфира на {:.1} с (буфер {:.1} с)",
                delta, buffered
            )),
            Err(e) => debug_log(&format!("live: догон эфира не удался: {e}")),
        }
    }

    /// «Лучшие настройки»: включает всё, что даёт минимум нагрузки и задержки.
    /// Вызывается кнопкой в консоли разработчика.
    fn apply_best_settings(&mut self) {
        self.hwdec = true;
        self.gpu_render = true;
        self.render_profile = AUTO_PROFILE;
        self.auto_quality = true;
        self.settings_dirty = true;
        // Смена backend'а рендера требует нового render-контекста mpv.
        self.gpu = gpu::GpuVideo::new();
        self.texture = None;
        self.recreate_player();
        self.status = "Настройки производительности сброшены к авто".to_string();
    }

    /// Консоль разработчика: декод, отрисовка, статистика, ссылка, Client ID
    /// и метаданные. Обычному пользователю она не нужна и по умолчанию закрыта.
    fn dev_console(&mut self, ctx: &egui::Context) {
        if !self.dev_panel {
            return;
        }
        let mut open = true;
        egui::Window::new("Консоль разработчика")
            .open(&mut open)
            .default_pos(egui::pos2(120.0, 140.0))
            .default_width(470.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Декодирование и отрисовка").strong());
                let mut hwdec_changed = false;
                let mut gpu_changed = false;
                let mut profile_changed = false;

                if ui
                    .checkbox(&mut self.hwdec, "GPU-декод (mpv hwdec=auto-copy)")
                    .on_hover_text(
                        "Декодирование на видеокарте: сильно снижает нагрузку на ЦП.\n\
                         При артефактах картинки выключите — mpv перейдёт на софт.",
                    )
                    .changed()
                {
                    hwdec_changed = true;
                }
                if ui
                    .checkbox(&mut self.gpu_render, "GPU-рендер кадра (OpenGL)")
                    .on_hover_text(
                        "mpv рисует кадр прямо в текстуру видеокарты: нет копий кадра\n\
                         через оперативную память, масштабирует видеокарта.\n\
                         Требует OpenGL 3.3; при проблемах плеер сам вернётся на ЦП.",
                    )
                    .changed()
                {
                    gpu_changed = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Профиль отрисовки:");
                    let current = if self.render_profile >= AUTO_PROFILE {
                        format!("Авто ({})", RENDER_PROFILES[self.profile_now].0)
                    } else {
                        RENDER_PROFILES[self.profile_now].0.to_string()
                    };
                    egui::ComboBox::from_id_source("dev_render_profile")
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_value(
                                    &mut self.render_profile,
                                    AUTO_PROFILE,
                                    "Авто (по нагрузке ПК)",
                                )
                                .changed()
                            {
                                profile_changed = true;
                            }
                            for (i, p) in RENDER_PROFILES.iter().enumerate() {
                                if ui
                                    .selectable_value(&mut self.render_profile, i, p.0)
                                    .changed()
                                {
                                    profile_changed = true;
                                }
                            }
                        });
                });
                if profile_changed {
                    // Размер кадра изменился — текстуру пересоздаём на следующем тике.
                    self.profile_now = self.resolve_profile();
                    self.tex_size = (0, 0);
                    self.last_render = None;
                }
                if hwdec_changed {
                    self.recreate_player();
                }
                if gpu_changed {
                    self.gpu = gpu::GpuVideo::new();
                    self.texture = None;
                    self.recreate_player();
                }
                if hwdec_changed || gpu_changed || profile_changed {
                    self.settings_dirty = true;
                }

                ui.separator();
                ui.label(egui::RichText::new("Субтитры · захват звука").strong());
                let mut subs_on = self.subs.enabled;
                if ui
                    .checkbox(&mut subs_on, "Захватывать звук стрима (эксперимент)")
                    .on_hover_text(
                        "Windows отдаёт звук только нашего процесса: музыка из браузера,\n\
                         игра и уведомления в субтитры не попадают.\n\
                         Нужна Windows 11 build 20348 или новее.",
                    )
                    .changed()
                {
                    self.subs.set_enabled(subs_on);
                    self.settings_dirty = true;
                }
                ui.horizontal(|ui| {
                    ui.label("Уровень:");
                    // RMS речи обычно 0.02–0.2 — растягиваем на всю полосу.
                    let level = (self.subs.level() * 6.0).clamp(0.0, 1.0);
                    let state = if self.subs.is_running() {
                        "захват идёт"
                    } else {
                        "нет данных"
                    };
                    ui.add(
                        egui::ProgressBar::new(level)
                            .desired_width(180.0)
                            .text(state),
                    );
                });
                ui.label(
                    egui::RichText::new(self.subs.stats_line())
                        .small()
                        .color(egui::Color32::from_gray(170)),
                );
                ui.horizontal(|ui| {
                    let recording = self.subs.recording_secs();
                    let text = match recording {
                        Some(secs) => format!("Пишу… {secs:.1} с"),
                        None => format!("Записать {:.0} с в WAV", subs::RECORD_SECS),
                    };
                    if ui
                        .add_enabled(recording.is_none(), egui::Button::new(text))
                        .on_hover_text(
                            "Проверка захвата: в файле должен быть только звук стримера.\n\
                             Файл: %APPDATA%\\twitch_mini_player\\capture.wav",
                        )
                        .clicked()
                    {
                        self.subs.start_recording();
                    }
                    if let Some(secs) = recording {
                        ui.add(
                            egui::ProgressBar::new(secs / subs::RECORD_SECS)
                                .desired_width(110.0),
                        );
                    }
                });
                if let Some(err) = &self.subs.last_error {
                    ui.colored_label(egui::Color32::from_rgb(255, 120, 120), err);
                }
                if let Some(note) = &self.subs.last_note {
                    ui.label(egui::RichText::new(note).small().weak());
                }

                ui.separator();
                ui.label(egui::RichText::new("Диагностика").strong());
                self.refresh_stats();
                ui.label(
                    egui::RichText::new(&self.stats_text)
                        .small()
                        .color(egui::Color32::from_gray(170)),
                );

                ui.separator();
                ui.label(egui::RichText::new("Поток").strong());
                match &self.stream_url {
                    Some(url) => {
                        ui.add(egui::Label::new(
                            egui::RichText::new(url.as_str()).small().monospace(),
                        ));
                        if ui.button("Копировать ссылку").clicked() {
                            ui.output_mut(|o| o.copied_text = url.clone());
                        }
                    }
                    None => {
                        ui.label("—");
                    }
                }
                if let Some(meta) = &self.meta {
                    ui.label(format!("Тайтл: {}", meta.title));
                    ui.label(format!("Игра: {}", meta.game));
                    ui.label(format!("Зрителей: {}", meta.viewers));
                    ui.label(format!("Начало: {}", meta.started_at));
                }

                ui.separator();
                ui.label(egui::RichText::new("Twitch API").strong());
                ui.horizontal(|ui| {
                    ui.label("Client ID:");
                    if ui
                        .add(egui::TextEdit::singleline(&mut self.client_id).desired_width(260.0))
                        .changed()
                    {
                        self.settings_dirty = true;
                    }
                });
                ui.label(
                    egui::RichText::new("Нужен только для метаданных (тайтл, игра, зрители)")
                        .small()
                        .weak(),
                );

                ui.separator();
                if ui
                    .button("Лучшие настройки")
                    .on_hover_text(
                        "GPU-декод, GPU-рендер, «Авто»-отрисовка и авто-качество —\n\
                         минимум нагрузки на ПК и минимум задержки.",
                    )
                    .clicked()
                {
                    self.apply_best_settings();
                }
                ui.label(
                    egui::RichText::new(format!(
                        "v{} · лог: %TEMP%\\twitch_mini_player_debug.txt (TW_DEBUG_LOG=1)",
                        APP_VERSION
                    ))
                    .small()
                    .weak(),
                );
            });
        if !open {
            self.dev_panel = false;
        }
    }

    /// Собирает строку статистики (не чаще 2 раз в секунду).
    fn refresh_stats(&mut self) {
        if self
            .stats_at
            .map(|t| t.elapsed() < Duration::from_millis(500))
            .unwrap_or(false)
        {
            return;
        }
        self.stats_at = Some(Instant::now());
        let mut parts: Vec<String> = Vec::with_capacity(7);
        parts.push(format!("UI {:.0} к/с", self.ui_fps));
        if let Some(p) = self.player.as_ref() {
            if let Some(fps) = p.video_fps() {
                parts.push(format!("видео {:.0} к/с", fps));
            }
            let stream_size = p.video_size();
            let tex = self.tex_size;
            match (stream_size, tex.0 > 0) {
                (Some((w, h)), true) => parts.push(format!("{w}×{h} → {}×{}", tex.0, tex.1)),
                (Some((w, h)), false) => parts.push(format!("{w}×{h}")),
                _ => {}
            }
            if let Some(buf) = p.cache_duration() {
                parts.push(format!("буфер {:.1} с", buf));
            }
            if let Some(hw) = p.hwdec_current() {
                parts.push(format!(
                    "декод {}",
                    if hw == "no" { "софт" } else { hw.as_str() }
                ));
            }
            if let Some(dropped) = p.dropped_frames() {
                if dropped > 0 {
                    parts.push(format!("потеряно {dropped}"));
                }
            }
        }
        if self.gpu_render {
            parts.push(format!("GPU-кадры {}", self.gpu.frames()));
        }
        // Какой профиль отрисовки реально работает (в «Авто» он подбирается).
        if let Some(p) = RENDER_PROFILES.get(self.profile_now) {
            parts.push(format!("отрисовка {}", p.0));
        }
        if self.auto_quality {
            if let Some(rate) = smoothed_rate(&self.net_rate) {
                // Байты/с → Мбит/с.
                parts.push(format!("сеть {:.1} Мбит/с", rate * 8.0 / 1_000_000.0));
            }
            if let Some(target) = self.auto_target_quality() {
                parts.push(format!("авто→{target}"));
            }
        }
        self.stats_text = parts.join(" · ");
    }

    /// Статус канала: пробуем разрешить поток в фоне (раз в 2 минуты, а если
    /// канал не в эфире — раз в 5). Успех = «в эфире» + свежий список качеств,
    /// ошибка = «не в эфире». Заодно это прогрев кэша: Play сработает мгновенно.
    fn check_channel_status(&mut self) {
        if self.busy {
            return;
        }
        // Во время воспроизведения статус уже известен — не тратим процесс.
        if self.player.as_ref().map(|p| p.is_playing()).unwrap_or(false) {
            return;
        }
        let channel = self.channel.trim().to_lowercase();
        if !is_valid_channel(&channel) {
            return;
        }
        let known = self
            .cached
            .as_ref()
            .map(|(ch, _, at)| ch == &channel && at.elapsed() < CACHE_TTL)
            .unwrap_or(false);
        if known {
            return;
        }
        // Оффлайн-канал проверяем реже: каждый опрос — это запуск streamlink.
        let interval = if self.online == Some(false) { 300 } else { 120 };
        let due = self
            .checked_at
            .map(|t| t.elapsed() >= Duration::from_secs(interval))
            .unwrap_or(true);
        if !due {
            return;
        }
        self.checked_at = Some(Instant::now());
        self.start_prefetch();
    }

    /// Следит за кэшем: показывает остановку, при долгой просадке переподключает
    /// поток, а при включённом «Авто-качестве» — понижает и возвращает качество.
    fn monitor_stream(&mut self) {
        let (playing, stalled) = match self.player.as_ref() {
            Some(p) if p.is_playing() => (true, p.paused_for_cache()),
            _ => (false, false),
        };
        if !playing {
            self.stall_since = None;
            self.stable_since = None;
            self.net_rate.clear();
            self.auto_down_votes = 0;
            return;
        }

        // Авто-режим имеет смысл только для того канала, что реально играет:
        // иначе вместо смены качества мы бы переключили канал.
        let same_channel = self
            .playing_channel
            .as_deref()
            .map(|c| self.channel.trim().eq_ignore_ascii_case(c))
            .unwrap_or(false);
        // На ручной паузе сеть почти не качает — качество не трогаем.
        let paused = self
            .player
            .as_ref()
            .map(|p| p.is_paused())
            .unwrap_or(false);
        // Первые 10 секунд после старта кэш только заполняется — не трогаем качество.
        let warm = self
            .playing_since
            .map(|t| t.elapsed() >= Duration::from_secs(10))
            .unwrap_or(false);

        // Замеры раз в 2 с: запас буфера (прогноз) и скорость сети («Авто»).
        if self
            .buf_sample_at
            .map(|t| t.elapsed() >= Duration::from_secs(2))
            .unwrap_or(true)
        {
            self.buf_sample_at = Some(Instant::now());
            let (buf, rate) = match self.player.as_ref() {
                Some(p) => (p.cache_duration(), p.input_rate()),
                None => (None, None),
            };
            if let Some(d) = buf {
                self.buf_trend.push(d as f32);
                if self.buf_trend.len() > 6 {
                    self.buf_trend.remove(0);
                }
            }
            if let Some(r) = rate {
                self.net_rate.push(r);
                if self.net_rate.len() > 6 {
                    self.net_rate.remove(0);
                }
            }
            // Накопленное после просадки отставание от live убираем сразу:
            // иначе картинка осталась бы позади эфира на весь буфер.
            if let Some(d) = buf {
                self.catchup_live(d, paused);
            }
        }

        if !stalled {
            self.stall_since = None;
            let cooldown_ok = self
                .last_auto_quality
                .map(|t| t.elapsed() >= AUTO_STEP_COOLDOWN)
                .unwrap_or(true);
            // Прогноз: запас таял, но фриза ещё нет — снижаем качество заранее,
            // чтобы картинка не замирала вовсе.
            if self.auto_quality
                && warm
                && same_channel
                && !paused
                && cooldown_ok
                && buffer_draining(&self.buf_trend)
                && self.step_quality_down("буфер тает")
            {
                self.last_auto_quality = Some(Instant::now());
                self.buf_trend.clear();
                self.stable_since = Some(Instant::now());
                return;
            }
            // «Авто»: цель по замеренной скорости сети.
            if self.auto_quality && warm && same_channel && !paused {
                self.auto_quality_by_rate(cooldown_ok);
            }
            self.stable_since.get_or_insert_with(Instant::now);
            return;
        }

        // Кэш пуст: сеть не успевает.
        self.stable_since = None;
        let stalled_for = self
            .stall_since
            .get_or_insert_with(Instant::now)
            .elapsed();

        // 1) Очень долгая остановка — один раз меняем CDN-узел (свежая ссылка).
        let can_refresh = self
            .last_refresh
            .map(|t| t.elapsed() >= REFRESH_COOLDOWN)
            .unwrap_or(true);
        if stalled_for >= STALL_REFRESH_AFTER && can_refresh && !self.busy && same_channel {
            self.last_refresh = Some(Instant::now());
            self.stall_since = None;
            self.cached = None;
            self.prefetched = None;
            self.error = None;
            self.status = "Переподключаюсь к потоку…".to_string();
            self.start_play();
            return;
        }

        // 2) «Авто»: шаг на уровень ниже, чтобы картинка не замирала.
        if self.auto_quality && warm && same_channel && stalled_for >= STALL_STEP_DOWN_AFTER {
            let allowed = self
                .last_auto_quality
                .map(|t| t.elapsed() >= AUTO_STEP_COOLDOWN)
                .unwrap_or(true);
            if allowed && self.step_quality_down("кэш пуст") {
                self.last_auto_quality = Some(Instant::now());
                self.stall_since = None;
            }
        }
    }

    /// «Авто» по скорости сети: два замера ниже цели — шаг вниз, долгая
    /// стабильная работа с запасом полосы — шаг вверх.
    fn auto_quality_by_rate(&mut self, cooldown_ok: bool) {
        let Some(target) = self.auto_target_quality() else {
            return;
        };
        let cur = quality_rank(&self.quality);
        let tgt = quality_rank(&target);
        if tgt < cur {
            self.auto_down_votes = self.auto_down_votes.saturating_add(1);
            // Два замера подряд (≈4 с) — скорость правда не тянет уровень.
            if self.auto_down_votes >= 2 && cooldown_ok && self.step_quality_down("скорость сети") {
                self.auto_down_votes = 0;
                self.last_auto_quality = Some(Instant::now());
                self.buf_trend.clear();
                self.stable_since = Some(Instant::now());
            }
            return;
        }
        self.auto_down_votes = 0;
        if tgt == cur {
            return;
        }
        // Вверх — только после долгой стабильной работы.
        let stable = self.stable_since.get_or_insert_with(Instant::now).elapsed();
        if cooldown_ok && stable >= STALL_RESTORE_AFTER && self.step_quality_up() {
            self.last_auto_quality = Some(Instant::now());
            self.stable_since = Some(Instant::now());
        }
    }

    /// Качество, которое проходит по замеренной скорости сети.
    fn auto_target_quality(&self) -> Option<String> {
        let rate = smoothed_rate(&self.net_rate)?;
        // 65% полосы: остальное — запас на рывки HLS и соседний трафик.
        let budget = rate * 8.0 * 0.65;
        let mut best: Option<&String> = None;
        for q in self.qualities.iter().rev() {
            if is_audio_only(q) {
                continue;
            }
            if quality_bitrate_mbps(q) * 1_000_000.0 <= budget {
                best = Some(q);
                break;
            }
        }
        match best {
            Some(q) => Some(q.clone()),
            None => self
                .qualities
                .iter()
                .find(|q| is_audio_only(q.as_str()))
                .cloned(),
        }
    }

    /// Понижает качество на один уровень. `true`, если переключили.
    fn step_quality_down(&mut self, reason: &str) -> bool {
        let list = self.qualities.clone();
        let Some(next) = neighbor_quality(&list, &self.quality, true) else {
            return false;
        };
        debug_log(&format!("авто: {} — {} → {}", reason, self.quality, next));
        self.quality = next.clone();
        self.status = format!("Качество снижено до {} ({})", next, reason);
        self.switch_quality();
        true
    }

    /// Повышает качество на один уровень. `true`, если переключили.
    fn step_quality_up(&mut self) -> bool {
        let list = self.qualities.clone();
        let Some(next) = neighbor_quality(&list, &self.quality, false) else {
            return false;
        };
        debug_log(&format!("авто: сеть тянет — {} → {}", self.quality, next));
        self.quality = next.clone();
        self.status = format!("Качество поднято до {} (сеть тянет)", next);
        self.switch_quality();
        true
    }

    /// Мгновенно переключает качество на уже разрешённом потоке (без streamlink).
    fn switch_quality(&mut self) {
        // Качество меняем у канала, который реально играет: поле ввода может
        // содержать другое имя (пользователь печатает следующий канал).
        let channel = self
            .playing_channel
            .clone()
            .unwrap_or_else(|| self.channel.trim().to_lowercase());
        let fresh = self
            .cached
            .as_ref()
            .map(|(ch, _, at)| ch == &channel && at.elapsed() < CACHE_TTL)
            .unwrap_or(false);
        if !fresh {
            // Ссылки Twitch живут недолго: обновляем их в фоне для играющего
            // канала, выбранное качество применится автоматически.
            self.cached = None;
            self.prefetched = None;
            self.spawn_resolve(channel, true);
            return;
        }
        let json = match &self.streams_json {
            Some(j) => j.clone(),
            None => return,
        };
        let url = match parse_streamlink_json(&json, &self.quality) {
            Some(u) => u,
            None => {
                self.error = Some(format!("Качество «{}» недоступно", self.quality));
                return;
            }
        };
        self.error = None;
        self.launched_from_cache = true;
        self.stream_url = Some(url.clone());
        if let Some(player) = self.player.as_mut() {
            if let Err(e) = player.load(&url) {
                self.error = Some(format!("mpv load: {}", e));
                return;
            }
        }
        // Старый кадр оставляем до прихода нового (без «мигания»).
        self.playing_since = Some(Instant::now());
        self.buf_trend.clear();
        self.catchup_at = None;
        self.status = format!("Переключаю качество: {}…", self.quality);
    }
}

/// Запускает streamlink и возвращает JSON со списком потоков.
fn run_streamlink(exe: &str, channel: &str) -> Result<String, String> {
    match run_streamlink_inner(exe, channel, true) {
        // Старые версии streamlink могут не знать про --twitch-disable-ads.
        Err(e) if e.contains("unrecognized arguments") || e.contains("no such option") => {
            run_streamlink_inner(exe, channel, false)
        }
        other => other,
    }
}

fn run_streamlink_inner(exe: &str, channel: &str, disable_ads: bool) -> Result<String, String> {
    let url_page = format!("https://www.twitch.tv/{}", channel);
    let mut cmd = Command::new(exe);
    cmd.arg("--json");
    if disable_ads {
        // Реклама в HLS заметно удлиняет старт и вызывает «залипания».
        cmd.args(["--twitch-disable-ads", "--stream-timeout", "20"]);
    }
    let output = cmd
        .arg(&url_page)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Не удалось запустить streamlink: {}", e))?;

    if !output.status.success() {
        // streamlink --json при ошибке печатает {"error": "..."} в stdout.
        let out = String::from_utf8_lossy(&output.stdout);
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&out) {
            if let Some(msg) = v.get("error").and_then(|e| e.as_str()) {
                return Err(format!("streamlink: {}", msg));
            }
        }
        let err = String::from_utf8_lossy(&output.stderr);
        let msg = err.trim();
        let msg = if msg.contains("No playable streams") {
            "канал оффлайн или недоступен"
        } else {
            msg
        };
        return Err(format!(
            "streamlink: {}",
            if msg.is_empty() {
                "канал оффлайн или недоступен"
            } else {
                msg
            }
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn fetch_metadata(
    client_id: &str,
    channel: &str,
) -> Result<StreamMeta, Box<dyn std::error::Error>> {
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let url = format!("{}?user_login={}", TWITCH_API, channel);
    let resp = client
        .get(&url)
        .header("Client-ID", client_id)
        .header("Accept", "application/vnd.twitchtv.v5+json")
        .send()?;

    if !resp.status().is_success() {
        return Err(format!("Twitch API error: {}", resp.status()).into());
    }

    let data: TwitchStreamResponse = resp.json()?;
    match data.data.into_iter().next() {
        Some(stream) => Ok(StreamMeta {
            title: stream.title,
            game: stream.game_name,
            viewers: stream.viewer_count,
            started_at: stream.started_at,
        }),
        None => Err("Канал оффлайн или не найден".into()),
    }
}

/// Возвращает URL потока для конкретного качества из JSON streamlink.
fn parse_streamlink_json(json_text: &str, quality: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let streams = value.get("streams")?.as_object()?;
    let entry = streams.get(quality)?;
    let url = entry.get("url")?.as_str()?;
    Some(url.to_string())
}

/// Список доступных качеств (ключей `streams`) из JSON streamlink,
/// отсортированный по возрастанию разрешения.
fn parse_streams_keys(json_text: &str) -> Vec<String> {
    let value: serde_json::Value = match serde_json::from_str(json_text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let streams = match value.get("streams").and_then(|s| s.as_object()) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut keys: Vec<String> = streams
        .keys()
        // "best"/"worst" — алиасы максимального/минимального качества,
        // в списке они лишь дублируют реальные варианты.
        .filter(|k| k.as_str() != "best" && k.as_str() != "worst")
        .cloned()
        .collect();
    keys.sort_by_key(|k| quality_rank(k));
    keys
}

/// Простая валидация имени канала Twitch (для спекулятивной предзагрузки).
fn is_valid_channel(name: &str) -> bool {
    (3..=25).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Буфер быстро тает: запас был приличный (≥4 с) и почти закончился (<2 с).
/// Такой тренд виден за несколько секунд до остановки — качество можно снизить
/// заранее, и картинка не замрёт вовсе.
fn buffer_draining(samples: &[f32]) -> bool {
    if samples.len() < 4 {
        return false;
    }
    let first = samples[0];
    let last = *samples.last().unwrap();
    first >= 4.0 && last < 2.0 && last < first - 3.0
}

/// Соседнее качество в списке (список отсортирован по возрастанию разрешения).
/// `down = true` — на уровень ниже, `false` — на уровень выше.
fn neighbor_quality(list: &[String], current: &str, down: bool) -> Option<String> {
    let idx = list.iter().position(|q| q == current)?;
    if down {
        idx.checked_sub(1).and_then(|i| list.get(i).cloned())
    } else {
        list.get(idx + 1).cloned()
    }
}

/// Высота качества из имени (`720p60` → 720). `None` для `source`/`best`.
fn quality_height(q: &str) -> Option<u32> {
    let pos = q.find('p')?;
    q[..pos].parse::<u32>().ok()
}

/// Сколько Мбит/с нужно качеству (оценка Twitch по высоте кадра).
/// Нужно режиму «Авто»: сравнить полосу сети с требованиями уровня.
fn quality_bitrate_mbps(q: &str) -> f64 {
    if is_audio_only(q) {
        return 0.16;
    }
    match quality_height(q) {
        Some(160) => 0.25,
        Some(360) => 0.75,
        Some(480) => 1.4,
        Some(720) => 3.2,
        Some(1080) => 6.2,
        Some(1440) => 9.0,
        // Незнакомая высота — квадратичная оценка (p ~ h²).
        Some(h) => (h as f64 * h as f64) / 155_000.0,
        // source/best — считаем как 1080p60.
        None => 6.2,
    }
}

/// Медиана последних замеров скорости (байт/с): одиночные рывки HLS
/// не должны «переключать» качество.
fn smoothed_rate(samples: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = samples.iter().copied().filter(|r| *r > 0.0).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

fn quality_rank(q: &str) -> i64 {
    if q == "audio_only" {
        return -1;
    }
    if let Some(pos) = q.find('p') {
        if let Ok(h) = q[..pos].parse::<i64>() {
            return h;
        }
    }
    match q {
        "worst" => 0,
        "source" => 9_000,
        "best" => 10_000,
        _ => 5_000,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "streams": {
            "best": {"url": "https://cdn/best.m3u8"},
            "720p60": {"url": "https://cdn/720.m3u8"},
            "audio_only": {"url": "https://cdn/audio.m3u8"},
            "480p30": {"url": "https://cdn/480.m3u8"}
        }
    }"#;

    #[test]
    fn lists_only_available_qualities_sorted() {
        assert_eq!(
            parse_streams_keys(SAMPLE),
            vec!["audio_only", "480p30", "720p60"]
        );
    }

    #[test]
    fn aliases_best_worst_are_filtered_out() {
        let json = r#"{"streams":{"worst":{"url":"w"},"best":{"url":"b"},"360p30":{"url":"x"}}}"#;
        assert_eq!(parse_streams_keys(json), vec!["360p30"]);
    }

    #[test]
    fn channel_names_are_validated() {
        assert!(is_valid_channel("shroud"));
        assert!(is_valid_channel("a_b1"));
        assert!(!is_valid_channel("ab"));
        assert!(!is_valid_channel("with space"));
        assert!(!is_valid_channel("https://twitch.tv/x"));
    }

    #[test]
    fn unavailable_quality_is_rejected() {
        assert!(parse_streamlink_json(SAMPLE, "1080p60").is_none());
        assert_eq!(
            parse_streamlink_json(SAMPLE, "720p60").as_deref(),
            Some("https://cdn/720.m3u8")
        );
    }

    #[test]
    fn empty_or_broken_json_yields_no_qualities() {
        assert!(parse_streams_keys("{}").is_empty());
        assert!(parse_streams_keys("not json").is_empty());
    }

    #[test]
    fn buffer_trend_predicts_stall() {
        // Запас таял: 12 → 8 → 4 → 1.6 с — просадка видна заранее.
        assert!(buffer_draining(&[12.0, 8.0, 4.0, 1.6]));
        // Запас стабилен — снижать качество нельзя.
        assert!(!buffer_draining(&[12.0, 12.0, 11.5, 11.0]));
        // Запас всегда был небольшим (сеть еле тянет с самого старта).
        assert!(!buffer_draining(&[2.5, 2.0, 1.5, 1.0]));
        // Слишком мало замеров — прогноза нет.
        assert!(!buffer_draining(&[9.0, 1.0]));
        assert!(!buffer_draining(&[]));
    }

    #[test]
    fn quality_steps_down_and_back_up() {
        let list: Vec<String> = ["audio_only", "480p30", "720p60", "1080p60"]
            .iter()
            .map(|q| q.to_string())
            .collect();
        // Понижение/повышение на один уровень.
        assert_eq!(neighbor_quality(&list, "720p60", true).as_deref(), Some("480p30"));
        assert_eq!(neighbor_quality(&list, "480p30", false).as_deref(), Some("720p60"));
        // На краях списка шаг невозможен — авто-режим не зацикливается.
        assert_eq!(neighbor_quality(&list, "audio_only", true), None);
        assert_eq!(neighbor_quality(&list, "1080p60", false), None);
        assert_eq!(neighbor_quality(&list, "1080p60", true).as_deref(), Some("720p60"));
        // Один шаг за раз: выше 720p60 «Авто» не перепрыгнет сразу на 1080p60.
        assert_eq!(neighbor_quality(&list, "720p60", false).as_deref(), Some("1080p60"));
    }

    #[test]
    fn auto_quality_matches_bandwidth() {
        // Требования уровней растут монотонно.
        assert!(quality_bitrate_mbps("audio_only") < quality_bitrate_mbps("160p30"));
        assert!(quality_bitrate_mbps("360p30") < quality_bitrate_mbps("480p30"));
        assert!(quality_bitrate_mbps("480p30") < quality_bitrate_mbps("720p60"));
        assert!(quality_bitrate_mbps("720p60") < quality_bitrate_mbps("1080p60"));
        // source/best считаем как 1080p60; незнакомая высота — по формуле.
        assert_eq!(quality_bitrate_mbps("source"), quality_bitrate_mbps("1080p60"));
        assert!(quality_bitrate_mbps("900p60") > quality_bitrate_mbps("720p60"));
        assert_eq!(quality_height("720p60"), Some(720));
        assert_eq!(quality_height("source"), None);
    }

    #[test]
    fn network_rate_is_smoothed_by_median() {
        // Один рывок HLS (очень высокая скорость) не должен менять решение.
        assert_eq!(smoothed_rate(&[1.0e6, 1.1e6, 9.0e6, 0.9e6, 1.2e6]), Some(1.1e6));
        assert_eq!(smoothed_rate(&[]), None);
        assert_eq!(smoothed_rate(&[0.0, 0.0]), None);
    }

    #[test]
    fn audio_only_quality_disables_video() {
        assert!(is_audio_only("audio_only"));
        assert!(is_audio_only("AUDIO_ONLY"));
        assert!(is_audio_only("audio"));
        assert!(!is_audio_only("720p60"));
        assert!(!is_audio_only("1080p60"));
        assert!(!is_audio_only(""));
    }

    #[test]
    fn render_profiles_throttle_as_expected() {
        assert_eq!(render_scale(0), 1.0);
        assert!(render_scale(2) < render_scale(1));
        assert_eq!(render_interval(0), Duration::from_millis(16));
        assert_eq!(render_interval(2), Duration::from_millis(33));
        // Неизвестный профиль — безопасный дефолт (полное качество, 60 FPS).
        assert_eq!(render_scale(99), 1.0);
        assert_eq!(render_interval(99), Duration::from_millis(16));
    }

    #[test]
    fn auto_profile_prefers_gpu_then_falls_back_by_frame_cost() {
        // С GPU-рендером кадр почти бесплатен — всегда полное разрешение.
        assert_eq!(auto_profile(true, 0.0), 0);
        assert_eq!(auto_profile(true, 40.0), 0);
        // Программный путь: сначала «сбалансированное», на слабом ЦП — экономное.
        assert_eq!(auto_profile(false, 0.0), 1);
        assert_eq!(auto_profile(false, AUTO_PROFILE_SLOW_MS - 0.1), 1);
        assert_eq!(
            auto_profile(false, AUTO_PROFILE_SLOW_MS),
            RENDER_PROFILES.len() - 1
        );
    }
}

/// Возвращает имя доступного качества (ключа) из JSON streamlink:
/// сначала запрошенное, иначе первый из приоритетного списка.
fn pick_stream_key(json_text: &str, preferred: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json_text).ok()?;
    let streams = value.get("streams")?.as_object()?;
    if streams.contains_key(preferred) {
        return Some(preferred.to_string());
    }
    for q in [
        "720p60", "720p30", "720p", "source", "1080p60", "480p30", "480p", "360p30", "360p",
        "160p30", "audio_only", "best", "worst",
    ] {
        if streams.contains_key(q) {
            return Some(q.to_string());
        }
    }
    streams.keys().next().cloned()
}

/// Ищет streamlink.exe в типичных местах установки Windows.
fn find_streamlink() -> String {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let pf = std::env::var("PROGRAMFILES").unwrap_or_else(|_| "C:\\Program Files".to_string());
    let pf86 =
        std::env::var("PROGRAMFILES(X86)").unwrap_or_else(|_| "C:\\Program Files (x86)".to_string());
    let candidates = [
        format!("{}\\Streamlink\\bin\\streamlink.exe", pf),
        format!("{}\\Streamlink\\bin\\streamlink.exe", pf86),
        format!("{}\\Programs\\Streamlink\\bin\\streamlink.exe", local),
        "C:\\tools\\streamlink\\bin\\streamlink.exe".to_string(),
    ];
    for c in &candidates {
        if Path::new(c).exists() {
            return c.clone();
        }
    }
    "streamlink".to_string()
}

/// Каталог настроек: `%APPDATA%\twitch_mini_player` (создаётся при первом запуске).
fn config_dir() -> std::path::PathBuf {
    let base = std::env::var("APPDATA")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let mut p = std::path::PathBuf::from(base);
    p.push("twitch_mini_player");
    let _ = std::fs::create_dir_all(&p);
    p
}

fn favorites_path() -> std::path::PathBuf {
    let mut p = config_dir();
    p.push("favorites.txt");
    p
}

fn settings_path() -> std::path::PathBuf {
    let mut p = config_dir();
    p.push("settings.json");
    p
}

/// Читает настройки; битый/старый файл не мешает запуску — берём значения по умолчанию.
/// Файл старой версии схемы обновляет «разработческие» настройки на лучшие
/// (GPU-декод, GPU-рендер, «Авто»-отрисовка и авто-качество): пользователю
/// ничего не нужно настраивать вручную. Канал, громкость и окно сохраняются.
fn load_settings() -> Settings {
    let mut s: Settings = std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if s.version < SETTINGS_VERSION {
        let best = Settings::default();
        s.render_profile = best.render_profile;
        s.hwdec = best.hwdec;
        s.auto_quality = best.auto_quality;
        s.gpu_render = best.gpu_render;
        s.version = SETTINGS_VERSION;
    }
    s
}

fn save_settings(s: &Settings) {
    if let Ok(text) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(settings_path(), text);
    }
}

fn load_favorites() -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(s) = std::fs::read_to_string(favorites_path()) {
        for line in s.lines() {
            let t = line.trim();
            if !t.is_empty() {
                v.push(t.to_lowercase());
            }
        }
    }
    v
}

fn save_favorites(favs: &[String]) {
    let _ = std::fs::write(favorites_path(), favs.join("\n"));
}

fn main() -> eframe::Result {
    install_panic_log();
    // Геометрия окна из прошлого запуска: открываемся там же и такого же размера.
    let saved = load_settings();
    let mut viewport = egui::ViewportBuilder::default().with_inner_size([760.0, 720.0]);
    if let Some(w) = saved.window {
        if w.w >= 320.0 && w.h >= 240.0 {
            viewport = viewport
                .with_inner_size([w.w, w.h])
                .with_position([w.x, w.y]);
        }
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|_cc| Ok(Box::new(TwitchMiniPlayer::default()))),
    )
}

/// У GUI-приложения нет консоли: без этого лога падение выглядит как
/// «программа просто закрылась». Файл: `%TEMP%\twitch_mini_player_crash.txt`.
fn install_panic_log() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let path = std::env::temp_dir().join("twitch_mini_player_crash.txt");
        let text = format!(
            "v{} panic: {}\nbacktrace:\n{}\n",
            APP_VERSION,
            info,
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::fs::write(&path, text);
        prev(info);
    }));
}
