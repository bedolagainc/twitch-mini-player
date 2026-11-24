//! Динамическая обёртка над `libmpv-2.dll` и её Render API (программный backend).
//!
//! Видео рендерится в память (`MPV_RENDER_API_TYPE_SW`, формат `rgb0`), а затем
//! превращается в `egui::ColorImage`. Благодаря этому кадр рисуется обычной
//! текстурой egui: работают плавные скругления углов и меню поверх видео,
//! без отдельного нативного окна mpv.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type MpvHandle = *mut c_void;
type RenderCtx = *mut c_void;

const MPV_FORMAT_DOUBLE: c_int = 5;
const MPV_FORMAT_STRING: c_int = 1;
const MPV_FORMAT_FLAG: c_int = 3;
const MPV_FORMAT_INT64: c_int = 4;

// mpv_render_param_type (значения 1–4 стабильны во всех версиях mpv)
const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
const MPV_RENDER_PARAM_OPENGL_INIT_PARAMS: c_int = 2;
const MPV_RENDER_PARAM_OPENGL_FBO: c_int = 3;
// Значение 4 (MPV_RENDER_PARAM_FLIP_Y) не используется вовсе: см. `render_opengl`.
const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;

/// GL_RGBA8 — внутренний формат текстуры, в которую рисует mpv.
const GL_RGBA8: c_int = 0x8058;

// mpv_event_id
const MPV_EVENT_NONE: c_int = 0;
const MPV_EVENT_LOG_MESSAGE: c_int = 2;
const MPV_EVENT_END_FILE: c_int = 7;
const MPV_EVENT_FILE_LOADED: c_int = 8;

// mpv_end_file_reason
const MPV_END_FILE_REASON_EOF: c_int = 0;
const MPV_END_FILE_REASON_ERROR: c_int = 4;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[repr(C)]
struct MpvRenderParam {
    type_: c_int,
    data: *mut c_void,
}

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

/// `mpv_opengl_init_params` — как mpv получает указатели на GL-функции.
#[repr(C)]
pub struct MpvOpenGlInitParams {
    /// Функция вида `glXGetProcAddress`/`wglGetProcAddress` (см. `gpu.rs`).
    pub get_proc_address: Option<unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void>,
    pub get_proc_address_ctx: *mut c_void,
}

/// `mpv_opengl_fbo` — целевой FBO, в который mpv рисует кадр.
#[repr(C)]
struct MpvOpenGlFbo {
    fbo: c_int,
    w: c_int,
    h: c_int,
    internal_format: c_int,
}

type FnCreate = unsafe extern "C" fn() -> MpvHandle;
type FnInit = unsafe extern "C" fn(MpvHandle) -> c_int;
type FnSetOpt = unsafe extern "C" fn(MpvHandle, *const c_char, *const c_char) -> c_int;
type FnSetPropStr = unsafe extern "C" fn(MpvHandle, *const c_char, *const c_char) -> c_int;
type FnGetProp = unsafe extern "C" fn(MpvHandle, *const c_char, c_int, *mut c_void) -> c_int;
type FnCommand = unsafe extern "C" fn(MpvHandle, *mut *const c_char) -> c_int;
type FnCommandStr = unsafe extern "C" fn(MpvHandle, *const c_char) -> c_int;
type FnWaitEvent = unsafe extern "C" fn(MpvHandle, f64) -> *mut MpvEvent;
type FnReqLog = unsafe extern "C" fn(MpvHandle, *const c_char) -> c_int;
type FnTerminate = unsafe extern "C" fn(MpvHandle);
type FnErrStr = unsafe extern "C" fn(c_int) -> *const c_char;
type FnRcCreate = unsafe extern "C" fn(*mut RenderCtx, MpvHandle, *mut MpvRenderParam) -> c_int;
type FnRcRender = unsafe extern "C" fn(RenderCtx, *mut MpvRenderParam) -> c_int;
type FnRcFree = unsafe extern "C" fn(RenderCtx);
type FnRcSetCb =
    unsafe extern "C" fn(RenderCtx, Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void);
type FnRcUpdate = unsafe extern "C" fn(RenderCtx) -> u64;
type FnRcSwap = unsafe extern "C" fn(RenderCtx);
type FnFree = unsafe extern "C" fn(*mut c_void);

/// Набор указателей на функции из `libmpv-2.dll`.
struct Api {
    create: FnCreate,
    initialize: FnInit,
    set_option_string: FnSetOpt,
    set_property_string: FnSetPropStr,
    get_property: FnGetProp,
    command: FnCommand,
    command_string: FnCommandStr,
    wait_event: FnWaitEvent,
    request_log_messages: Option<FnReqLog>,
    terminate_destroy: FnTerminate,
    error_string: Option<FnErrStr>,
    rc_create: FnRcCreate,
    rc_render: FnRcRender,
    rc_free: FnRcFree,
    rc_set_update_cb: FnRcSetCb,
    rc_update: FnRcUpdate,
    rc_swap: FnRcSwap,
    free: FnFree,
}

fn wide(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

unsafe fn get_sym(module: isize, name: &str) -> *const c_void {
    let cs = match CString::new(name) {
        Ok(c) => c,
        Err(_) => return std::ptr::null(),
    };
    match GetProcAddress(module, cs.as_ptr() as *const u8) {
        Some(fp) => fp as usize as *const c_void,
        None => std::ptr::null(),
    }
}

fn load_library() -> Result<isize, String> {
    let mut cands: Vec<String> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("libmpv-2.dll").to_string_lossy().to_string());
        }
    }
    cands.push("libmpv-2.dll".to_string());
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("libmpv-2.dll").to_string_lossy().to_string());
    }
    cands.push("C:\\Program Files\\MPV Player\\libmpv-2.dll".to_string());
    for c in &cands {
        let w = wide(c);
        let h = unsafe { LoadLibraryW(w.as_ptr()) };
        if h != 0 {
            return Ok(h);
        }
    }
    Err("libmpv-2.dll не найден (положите его рядом с .exe)".to_string())
}

unsafe fn load_api(module: isize) -> Result<Api, String> {
    let mut missing: Vec<&str> = Vec::new();
    macro_rules! sym {
        ($name:literal) => {{
            let p = get_sym(module, $name);
            if p.is_null() {
                missing.push($name);
            }
            p
        }};
    }
    let p_create = sym!("mpv_create");
    let p_init = sym!("mpv_initialize");
    let p_setopt = sym!("mpv_set_option_string");
    let p_setprop = sym!("mpv_set_property_string");
    let p_getprop = sym!("mpv_get_property");
    let p_cmd = sym!("mpv_command");
    let p_cmdstr = sym!("mpv_command_string");
    let p_wait = sym!("mpv_wait_event");
    let p_term = sym!("mpv_terminate_destroy");
    let p_rc_create = sym!("mpv_render_context_create");
    let p_rc_render = sym!("mpv_render_context_render");
    let p_rc_free = sym!("mpv_render_context_free");
    let p_rc_cb = sym!("mpv_render_context_set_update_callback");
    let p_rc_update = sym!("mpv_render_context_update");
    let p_rc_swap = sym!("mpv_render_context_report_swap");
    let p_free = sym!("mpv_free");
    let p_err = get_sym(module, "mpv_error_string");
    let p_log = get_sym(module, "mpv_request_log_messages");

    if !missing.is_empty() {
        return Err(format!(
            "в libmpv-2.dll нет нужных символов: {}",
            missing.join(", ")
        ));
    }

    Ok(Api {
        create: std::mem::transmute(p_create),
        initialize: std::mem::transmute(p_init),
        set_option_string: std::mem::transmute(p_setopt),
        set_property_string: std::mem::transmute(p_setprop),
        get_property: std::mem::transmute(p_getprop),
        command: std::mem::transmute(p_cmd),
        command_string: std::mem::transmute(p_cmdstr),
        wait_event: std::mem::transmute(p_wait),
        request_log_messages: if p_log.is_null() {
            None
        } else {
            Some(std::mem::transmute(p_log))
        },
        terminate_destroy: std::mem::transmute(p_term),
        error_string: if p_err.is_null() {
            None
        } else {
            Some(std::mem::transmute(p_err))
        },
        rc_create: std::mem::transmute(p_rc_create),
        rc_render: std::mem::transmute(p_rc_render),
        rc_free: std::mem::transmute(p_rc_free),
        rc_set_update_cb: std::mem::transmute(p_rc_cb),
        rc_update: std::mem::transmute(p_rc_update),
        rc_swap: std::mem::transmute(p_rc_swap),
        free: std::mem::transmute(p_free),
    })
}

/// Один экземпляр libmpv с включённым программным Render API.
pub struct MpvPlayer {
    api: Api,
    handle: MpvHandle,
    rctx: RenderCtx,
    /// Общее с mpv состояние: флаг «есть новый кадр» + пробуждение UI.
    wake: Arc<WakeHolder>,
    buf: Vec<u8>,
    size: (u32, u32),
    loaded: bool,
    ended: bool,
    /// Поток завершился ошибкой (истёкшая ссылка, обрыв CDN) — можно обновить URL.
    error_end: bool,
}

/// Разделяемое с колбэком mpv состояние.
struct WakeHolder {
    dirty: AtomicBool,
    /// Вызывается из потока mpv при появлении нового кадра.
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// Колбэк mpv «готов новый кадр»: ставит флаг и будит UI-поток.
unsafe extern "C" fn frame_cb(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    let holder = &*(ctx as *const WakeHolder);
    holder.dirty.store(true, Ordering::SeqCst);
    // «Пробуждалку» забираем под блокировкой, а вызываем уже без неё:
    // так исключены взаимные блокировки с UI-потоком.
    let wake = holder.wake.lock().ok().and_then(|guard| guard.clone());
    if let Some(f) = wake {
        f();
    }
}

impl MpvPlayer {
    /// `hwdec = true` — аппаратное декодирование с копированием кадра в RAM
    /// (`auto-copy`): декодирует GPU, поэтому ЦП почти не нагружается, а
    /// программный Render API продолжает получать обычные кадры.
    /// Если подходящего декодера нет, mpv сам откатывается на софт.
    pub fn new(hwdec: bool) -> Result<Self, String> {
        Self::create(hwdec, false)
    }

    /// Вариант для GPU-рендера: кадр отдаётся сразу в OpenGL-текстуру.
    /// GL-контекст создаётся позже — из paint-колбэка eframe
    /// (`init_opengl_render`), где контекст гарантированно текущий.
    pub fn new_opengl(hwdec: bool) -> Result<Self, String> {
        Self::create(hwdec, true)
    }

    fn create(hwdec: bool, opengl: bool) -> Result<Self, String> {
        let module = load_library()?;
        unsafe {
            let api = load_api(module)?;
            let handle = (api.create)();
            if handle.is_null() {
                return Err("mpv_create() вернул NULL".to_string());
            }
            let set = |name: &str, val: &str| {
                let n = CString::new(name).unwrap();
                let v = CString::new(val).unwrap();
                (api.set_option_string)(handle, n.as_ptr(), v.as_ptr())
            };
            // vo=libmpv обязателен: он отдаёт кадры в Render API, а не в своё окно.
            set("vo", "libmpv");
            set("idle", "yes");
            // Если сборка mpv не знает "auto-copy" — спокойно откатываемся на софт.
            if set("hwdec", if hwdec { "auto-copy" } else { "no" }) < 0 {
                set("hwdec", "no");
            }
            set("video-timing-offset", "0"); // не блокировать UI на таймингах кадра
            // Стартующая точка live-HLS задаётся lavf-опцией `live_start_index`
            // (-1 = последний сегмент плейлиста, это значение по умолчанию), так
            // что отдельная настройка не нужна: `hls-live-edge` — параметр
            // streamlink/yt-dlp, а не mpv.
            set("cache", "yes");
            // Буфер — компромисс между «не замирать» и «не отставать от live»:
            // 12 с запаса гасят короткие просадки сети, а накопленное после
            // просадки отставание убирает догон эфира (см. `seek_forward`).
            // Старт не ждёт заполнения кэша (`cache-pause-initial=no`).
            set("cache-secs", "12");
            set("cache-pause-initial", "no");
            // 32 МБ вперёд — это ~12 с даже для 1080p60 (8 Мбит/с ≈ 12 МБ),
            // поэтому запас по времени не страдает, а память меньше.
            set("demuxer-max-bytes", "32MiB");
            set("demuxer-max-back-bytes", "4MiB");
            set("network-timeout", "30");
            // Не тратим запросы на поиск внешних субтитров/аудиодорожек в HLS.
            set("sub-auto", "no");
            set("audio-file-auto", "no");

            set("user-agent", UA);
            set("referrer", "https://www.twitch.tv/");
            if opengl {
                // Render API типа OpenGL требует OpenGL-бэкенд самого mpv:
                // иначе на Windows он попытается взять D3D11 и контекст не создастся.
                set("gpu-api", "opengl");
            }

            let rc = (api.initialize)(handle);
            if rc < 0 {
                let msg = err_string(&api, rc);
                (api.terminate_destroy)(handle);
                return Err(format!("mpv_initialize: {}", msg));
            }

            // Сообщения mpv (warn и выше) складываем в отладочный лог — по ним
            // видно, почему поток или GPU-рендер отказал.
            if let Some(req) = api.request_log_messages {
                req(handle, b"warn\0".as_ptr() as *const c_char);
            }

            let api_type = b"sw\0";
            let mut params = [
                MpvRenderParam {
                    type_: MPV_RENDER_PARAM_API_TYPE,
                    data: api_type.as_ptr() as *mut c_void,
                },
                MpvRenderParam {
                    type_: 0,
                    data: std::ptr::null_mut(),
                },
            ];
            let mut rctx: RenderCtx = std::ptr::null_mut();
            // Для GPU-пути контекст создаётся позже, из paint-колбэка: только там
            // гарантированно текущий GL-контекст окна.
            if !opengl {
                let rc = (api.rc_create)(&mut rctx, handle, params.as_mut_ptr());
                if rc < 0 {
                    let msg = err_string(&api, rc);
                    (api.terminate_destroy)(handle);
                    return Err(format!("mpv_render_context_create(sw): {}", msg));
                }
            }

            let wake = Arc::new(WakeHolder {
                dirty: AtomicBool::new(true),
                wake: Mutex::new(None),
            });
            if !rctx.is_null() {
                (api.rc_set_update_cb)(rctx, Some(frame_cb), Arc::as_ptr(&wake) as *mut c_void);
            }

            Ok(Self {
                api,
                handle,
                rctx,
                wake,
                buf: Vec::new(),
                size: (0, 0),
                loaded: false,
                ended: false,
                error_end: false,
            })
        }
    }

    fn set_prop(&self, name: &str, val: &str) {
        let n = CString::new(name).unwrap();
        let v = CString::new(val).unwrap();
        unsafe {
            (self.api.set_property_string)(self.handle, n.as_ptr(), v.as_ptr());
        }
    }

    /// Выполняет команду mpv (`mpv_command`) с аргументами.
    fn command(&self, args: &[&str]) -> Result<(), String> {
        let owned: Vec<CString> = args
            .iter()
            .map(|a| CString::new(*a).unwrap_or_default())
            .collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let rc = unsafe { (self.api.command)(self.handle, ptrs.as_mut_ptr()) };
        if rc < 0 {
            return Err(err_string(&self.api, rc));
        }
        Ok(())
    }

    /// Перескакивает к актуальному эфиру (live-edge).
    ///
    /// После паузы mpv продолжает с того же места, и картинка отстаёт от эфира
    /// на всю длительность паузы. У live-HLS `duration` — это длина
    /// seekable-окна, поэтому `100%` = актуальный эфир; mpv сам ограничивает
    /// прыжок тем, что реально доступно, так что перескок безопасен.
    /// Если длительность неизвестна, берём конец буфера `demuxer-cache-time`
    /// (проверено на живом Twitch: `demuxer-cache-state/cache-end` в mpv нет).
    pub fn seek_to_live(&self) -> Result<(), String> {
        let pos = self.get_double("time-pos");
        let duration = self.get_double("duration");
        if duration.map(|d| d.is_finite() && d > 0.0).unwrap_or(false) {
            crate::debug_log(&format!(
                "mpv: перескок к эфиру: 100% окна (было {:.1} из {:.1})",
                pos.unwrap_or(-1.0),
                duration.unwrap_or(-1.0)
            ));
            if self.command(&["seek", "100", "absolute-percent"]).is_ok() {
                return Ok(());
            }
        }
        if let Some(end) = self.get_double("demuxer-cache-time") {
            if end.is_finite() && end > 0.0 {
                let target = format!("{:.3}", end);
                crate::debug_log(&format!("mpv: перескок к концу буфера {} с", target));
                if self.command(&["seek", &target, "absolute"]).is_ok() {
                    return Ok(());
                }
            }
        }
        Err("не удалось определить конец эфира".to_string())
    }

    /// Перескакивает вперёд на `secs` секунд — догон эфира после просадки сети.
    ///
    /// Если сеть «проснулась», воспроизведение продолжается с того же кадра и
    /// навсегда остаётся позади live на весь накопленный буфер. Прыжок вперёд
    /// идёт по уже скачанным данным, поэтому выполняется мгновенно и без
    /// повторной буферизации.
    pub fn seek_forward(&self, secs: f64) -> Result<(), String> {
        let d = format!("{:.2}", secs.clamp(0.1, 60.0));
        self.command(&["seek", &d, "relative"])
    }

    /// Ставит обработчик «у mpv готов новый кадр».
    /// Вызывается из потока mpv, поэтому внутри нужен потокобезопасный wake-up
    /// (например, `egui::Context::request_repaint`).
    pub fn set_frame_wake(&self, f: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut guard) = self.wake.wake.lock() {
            *guard = Some(Arc::new(f));
        }
    }

    pub fn load(&mut self, url: &str) -> Result<(), String> {
        let c_url = CString::new(url).map_err(|e| e.to_string())?;
        let c_load = CString::new("loadfile").unwrap();
        let mut args: [*const c_char; 3] = [c_load.as_ptr(), c_url.as_ptr(), std::ptr::null()];
        let rc = unsafe { (self.api.command)(self.handle, args.as_mut_ptr()) };
        if rc < 0 {
            return Err(err_string(&self.api, rc));
        }
        self.loaded = true;
        self.ended = false;
        self.error_end = false;
        self.wake.dirty.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn stop(&mut self) {
        let c = CString::new("stop").unwrap();
        unsafe {
            (self.api.command_string)(self.handle, c.as_ptr());
        }
        self.loaded = false;
        self.ended = false;
        self.size = (0, 0);
    }

    pub fn set_volume(&self, v: i64) {
        self.set_prop("volume", &v.to_string());
    }

    /// Пауза / продолжить (свойство `pause`).
    pub fn set_paused(&self, paused: bool) {
        self.set_prop("pause", if paused { "yes" } else { "no" });
    }

    /// Текущее состояние паузы.
    pub fn is_paused(&self) -> bool {
        self.get_flag("pause").unwrap_or(false)
    }

    /// Включает/выключает видеодорожку (`vid=auto` / `vid=no`).
    /// При `false` mpv не отдаёт кадры — звук идёт, ЦП почти не тратится.
    pub fn set_video_enabled(&self, on: bool) {
        self.set_prop("vid", if on { "auto" } else { "no" });
    }

    pub fn poll_events(&mut self) {
        loop {
            let ev = unsafe { (self.api.wait_event)(self.handle, 0.0) };
            if ev.is_null() {
                break;
            }
            let e = unsafe { &*ev };
            if e.event_id == MPV_EVENT_NONE {
                break;
            }
            match e.event_id {
                MPV_EVENT_LOG_MESSAGE => {
                    // mpv_event_log_message { const char *prefix; const char *level;
                    //                        const char *text; int log_level; }
                    if !e.data.is_null() {
                        let text_ptr = unsafe {
                            *((e.data as *const u8).add(16) as *const *const c_char)
                        };
                        if !text_ptr.is_null() {
                            let text = unsafe { CStr::from_ptr(text_ptr) }.to_string_lossy();
                            let text = text.trim_end();
                            if !text.is_empty() {
                                crate::debug_log(&format!("mpv: {}", text));
                            }
                        }
                    }
                }
                MPV_EVENT_FILE_LOADED => {
                    self.loaded = true;
                    self.ended = false;
                    self.error_end = false;
                }
                MPV_EVENT_END_FILE => {
                    // reason: 0 = EOF, 4 = ERROR. Остальные значения (STOP,
                    // QUIT, REDIRECT) возникают при смене файла/остановке/редиректе
                    // и не означают, что стрим закончился.
                    let (reason, code) = if e.data.is_null() {
                        (-1, 0)
                    } else {
                        unsafe {
                            (
                                *(e.data as *const c_int),
                                *((e.data as *const u8).add(4) as *const c_int),
                            )
                        }
                    };
                    if reason == MPV_END_FILE_REASON_EOF || reason == MPV_END_FILE_REASON_ERROR {
                        crate::debug_log(&format!(
                            "mpv: конец файла reason={} error={} ({})",
                            reason,
                            code,
                            err_string(&self.api, code)
                        ));
                        self.loaded = false;
                        self.ended = true;
                        self.error_end = reason == MPV_END_FILE_REASON_ERROR;
                    }
                }
                _ => {}
            }
        }
    }

    pub fn is_playing(&self) -> bool {
        self.loaded && !self.ended
    }

    pub fn ended(&self) -> bool {
        self.ended
    }

    /// `true`, если поток оборвался ошибкой (а не штатным концом).
    pub fn failed(&self) -> bool {
        self.error_end
    }

    pub fn take_dirty(&self) -> bool {
        self.wake.dirty.swap(false, Ordering::SeqCst)
    }

    /// Опрашивает render-контекст. Возвращает `true`, если готов новый кадр
    /// (`MPV_RENDER_UPDATE_FRAME`). Обязательно вызывать каждый тик: это
    /// сбрасывает внутренние флаги и «перевзводит» update-callback.
    pub fn frame_ready(&self) -> bool {
        if self.rctx.is_null() {
            // GPU-контекст ещё не создан: считаем, что кадр «есть», чтобы
            // первый же paint-колбэк инициализировал рендер.
            return true;
        }
        unsafe { ((self.api.rc_update)(self.rctx) & 1) != 0 }
    }

    /// Создаёт OpenGL render-контекст mpv.
    ///
    /// Вызывать **только** при текущем GL-контексте окна — т.е. из
    /// `egui::PaintCallback` (там же создаются текстура и FBO).
    pub fn init_opengl_render(&mut self, init: &mut MpvOpenGlInitParams) -> Result<(), String> {
        if !self.rctx.is_null() {
            return Ok(());
        }
        let api_type = b"opengl\0";
        let mut params = [
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr() as *mut c_void,
            },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
                data: init as *mut MpvOpenGlInitParams as *mut c_void,
            },
            MpvRenderParam {
                type_: 0,
                data: std::ptr::null_mut(),
            },
        ];
        let mut rctx: RenderCtx = std::ptr::null_mut();
        let rc = unsafe { (self.api.rc_create)(&mut rctx, self.handle, params.as_mut_ptr()) };
        if rc < 0 {
            return Err(format!(
                "mpv_render_context_create(opengl): {}",
                err_string(&self.api, rc)
            ));
        }
        self.rctx = rctx;
        unsafe {
            (self.api.rc_set_update_cb)(
                rctx,
                Some(frame_cb),
                Arc::as_ptr(&self.wake) as *mut c_void,
            );
        }
        Ok(())
    }

    /// Рисует текущий кадр прямо в FBO (никаких копий через RAM).
    pub fn render_opengl(&mut self, fbo: u32, w: u32, h: u32) -> Result<(), String> {
        if self.rctx.is_null() {
            return Err("OpenGL render-контекст не создан".to_string());
        }
        let mut fbo_param = MpvOpenGlFbo {
            fbo: fbo as c_int,
            w: w as c_int,
            h: h as c_int,
            internal_format: GL_RGBA8,
        };
        // MPV_RENDER_PARAM_FLIP_Y (4) не передаём: кадр и в FBO, и в программном
        // буфере приходит «сверху вниз», поэтому шейдер (`gpu.rs`) берёт UV без
        // переворота, а `rgb0_to_color_image` не переставляет строки. Проверено
        // сравнением с кадром, который mpv сам пишет в PNG.
        let mut params = [
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_OPENGL_FBO,
                data: &mut fbo_param as *mut MpvOpenGlFbo as *mut c_void,
            },
            MpvRenderParam {
                type_: 0,
                data: std::ptr::null_mut(),
            },
        ];
        let rc = unsafe { (self.api.rc_render)(self.rctx, params.as_mut_ptr()) };
        if rc < 0 {
            return Err(err_string(&self.api, rc));
        }
        unsafe {
            (self.api.rc_swap)(self.rctx);
        }
        Ok(())
    }

    /// Какое декодирование реально используется: `"no"` — софт,
    /// `d3d11va-copy`/`dxva2-copy` — аппаратное (GPU).
    pub fn hwdec_current(&self) -> Option<String> {
        let mut p: *mut c_char = std::ptr::null_mut();
        let rc = unsafe {
            (self.api.get_property)(
                self.handle,
                b"hwdec-current\0".as_ptr() as *const c_char,
                MPV_FORMAT_STRING,
                &mut p as *mut *mut c_char as *mut c_void,
            )
        };
        if rc < 0 || p.is_null() {
            return None;
        }
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string();
        unsafe { (self.api.free)(p as *mut c_void) };
        Some(s)
    }

    /// mpv остановил воспроизведение, потому что кэш пуст (сеть не успевает).
    pub fn paused_for_cache(&self) -> bool {
        self.get_flag("paused-for-cache").unwrap_or(false)
    }

    /// Скорость прихода данных из сети (байт/с) по данным mpv.
    /// `None` — свойство недоступно (старая сборка mpv) или поток ещё не начался.
    pub fn input_rate(&self) -> Option<f64> {
        const PROP: &str = "demuxer-cache-state/raw-input-rate";
        self.get_int(PROP)
            .map(|v| v as f64)
            .or_else(|| self.get_double(PROP))
            .filter(|v| *v > 0.0)
    }

    pub fn cache_duration(&self) -> Option<f64> {
        self.get_double("demuxer-cache-duration")
    }

    /// Позиция воспроизведения (с): к ней будет привязан показ субтитров —
    /// буфер и догон эфира тогда не сдвигают текст относительно речи.
    /// Понадобится на этапе 0.19.0 (синхронизация готовых фраз).
    #[allow(dead_code)]
    pub fn time_pos(&self) -> Option<f64> {
        self.get_double("time-pos")
    }

    /// Числовое свойство mpv (`MPV_FORMAT_DOUBLE`).
    fn get_double(&self, name: &str) -> Option<f64> {
        let n = CString::new(name).ok()?;
        let mut v: f64 = 0.0;
        let rc = unsafe {
            (self.api.get_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_DOUBLE,
                &mut v as *mut f64 as *mut c_void,
            )
        };
        if rc >= 0 {
            Some(v)
        } else {
            None
        }
    }

    /// Целочисленное свойство mpv (`MPV_FORMAT_INT64`).
    fn get_int(&self, name: &str) -> Option<i64> {
        let n = CString::new(name).ok()?;
        let mut v: i64 = 0;
        let rc = unsafe {
            (self.api.get_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_INT64,
                &mut v as *mut i64 as *mut c_void,
            )
        };
        if rc >= 0 {
            Some(v)
        } else {
            None
        }
    }

    /// Булево свойство mpv (`MPV_FORMAT_FLAG`).
    fn get_flag(&self, name: &str) -> Option<bool> {
        let n = CString::new(name).ok()?;
        let mut flag: c_int = 0;
        let rc = unsafe {
            (self.api.get_property)(
                self.handle,
                n.as_ptr(),
                MPV_FORMAT_FLAG,
                &mut flag as *mut c_int as *mut c_void,
            )
        };
        if rc >= 0 {
            Some(flag != 0)
        } else {
            None
        }
    }

    /// Фактическая частота кадров видео (что mpv реально отдаёт).
    pub fn video_fps(&self) -> Option<f64> {
        self.get_double("estimated-vf-fps")
            .or_else(|| self.get_double("container-fps"))
            .filter(|f| *f > 0.0)
    }

    /// Сколько кадров mpv не успел показать (потери декодирования/вывода).
    pub fn dropped_frames(&self) -> Option<i64> {
        self.get_int("frame-drop-count")
    }

    /// Размер видеокадра в пикселях (как его отдаёт поток).
    pub fn video_size(&self) -> Option<(i64, i64)> {
        let w = self.get_int("video-params/w")?;
        let h = self.get_int("video-params/h")?;
        if w > 0 && h > 0 {
            Some((w, h))
        } else {
            None
        }
    }

    /// Пропорции кадра (ширина / высота) с учётом анаморфных пикселей и
    /// поворота — mpv отдаёт готовое значение `video-params/aspect`.
    pub fn video_aspect(&self) -> Option<f32> {
        self.get_double("video-params/aspect")
            .filter(|a| a.is_finite() && *a > 0.1)
            .map(|a| a as f32)
            .or_else(|| {
                self.video_size()
                    .map(|(w, h)| w as f32 / h as f32)
            })
    }

    /// Рендерит текущий кадр в память (rgb0) и превращает в `ColorImage`.
    pub fn render(&mut self, w: u32, h: u32) -> Result<egui::ColorImage, String> {
        if self.rctx.is_null() {
            return Err("программный render-контекст не создан".to_string());
        }
        let w = w.max(2);
        let h = h.max(2);
        if self.size != (w, h) {
            self.size = (w, h);
            self.buf.resize((w as usize) * 4 * (h as usize), 0);
        }
        let stride = (w as usize) * 4;
        let mut sz: [c_int; 2] = [w as c_int, h as c_int];
        let mut stride_v: usize = stride;
        let fmt = b"rgb0\0";
        let mut params = [
            MpvRenderParam { type_: MPV_RENDER_PARAM_SW_SIZE, data: sz.as_mut_ptr() as *mut c_void },
            MpvRenderParam { type_: MPV_RENDER_PARAM_SW_FORMAT, data: fmt.as_ptr() as *mut c_void },
            MpvRenderParam {
                type_: MPV_RENDER_PARAM_SW_STRIDE,
                data: &mut stride_v as *mut usize as *mut c_void,
            },
            MpvRenderParam { type_: MPV_RENDER_PARAM_SW_POINTER, data: self.buf.as_mut_ptr() as *mut c_void },
            MpvRenderParam { type_: 0, data: std::ptr::null_mut() },
        ];
        let rc = unsafe { (self.api.rc_render)(self.rctx, params.as_mut_ptr()) };
        if rc < 0 {
            return Err(err_string(&self.api, rc));
        }
        // Сообщаем mpv, что кадр «показан» — это нужно для корректного
        // продвижения кадров и таймингов воспроизведения.
        unsafe {
            (self.api.rc_swap)(self.rctx);
        }
        Ok(rgb0_to_color_image(&self.buf, w, h))
    }
}

/// `rgb0`-кадр (4 байта на пиксель) → `ColorImage` egui.
///
/// Программный Render API mpv отдаёт кадр уже «сверху вниз»: строка 0 буфера —
/// **верхняя** строка кадра (проверено сравнением с кадром, который mpv пишет
/// в PNG). Поэтому строки копируются как есть: ни переворота, ни
/// `MPV_RENDER_PARAM_FLIP_Y` не нужно. В GPU-пути всё наоборот — там кадр в FBO
/// лежит снизу вверх, и его переворачивает шейдер (`gpu.rs`).
fn rgb0_to_color_image(buf: &[u8], w: u32, h: u32) -> egui::ColorImage {
    let w = w.max(1) as usize;
    let h = h.max(1) as usize;
    let row = w * 4;
    let rows = (buf.len() / row).min(h);
    let mut pixels: Vec<egui::Color32> = Vec::with_capacity(w * h);
    for y in 0..rows {
        let start = y * row;
        for px in buf[start..start + row].chunks_exact(4) {
            pixels.push(egui::Color32::from_rgb(px[0], px[1], px[2]));
        }
    }
    if pixels.is_empty() {
        // Кадра ещё нет (буфер пуст) — отдаём чёрный пиксель, а не паникуем.
        return egui::ColorImage {
            size: [1, 1],
            pixels: vec![egui::Color32::BLACK],
        };
    }
    let height = pixels.len() / w;
    egui::ColorImage {
        size: [w, height],
        pixels,
    }
}

impl Drop for MpvPlayer {
    fn drop(&mut self) {
        unsafe {
            if !self.rctx.is_null() {
                (self.api.rc_free)(self.rctx);
                self.rctx = std::ptr::null_mut();
            }
            if !self.handle.is_null() {
                (self.api.terminate_destroy)(self.handle);
                self.handle = std::ptr::null_mut();
            }
        }
    }
}

fn err_string(api: &Api, code: c_int) -> String {
    if let Some(f) = api.error_string {
        let p = unsafe { f(code) };
        if !p.is_null() {
            let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().to_string();
            return format!("{} (код {})", s, code);
        }
    }
    format!("код {}", code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Кадр `rgb0` (4 байта на пиксель) должен давать ровно `w*h` непрозрачных
    /// `Color32` в том же порядке строк, в котором его отдаёт mpv (строка 0
    /// буфера — верхняя строка кадра).
    /// Именно это соотношение (байты → пиксели) однажды было нарушено
    /// и приводило к падению плеера на первом кадре.
    #[test]
    fn rgb0_frame_becomes_opaque_color32() {
        // 2×2: первая строка буфера — верх кадра, вторая — низ.
        let frame: Vec<u8> = vec![
            10, 20, 30, 0, 40, 50, 60, 0, // верх
            1, 2, 3, 0, 250, 251, 252, 0, // низ
        ];
        let img = rgb0_to_color_image(&frame, 2, 2);
        assert_eq!(img.size, [2, 2]);
        assert_eq!(img.pixels.len(), 4);
        // Первая строка ColorImage — верхняя строка кадра (альфа непрозрачная).
        assert_eq!(img.pixels[0].to_array(), [10, 20, 30, 255]);
        assert_eq!(img.pixels[1], egui::Color32::from_rgb(40, 50, 60));
        assert_eq!(img.pixels[2], egui::Color32::from_rgb(1, 2, 3));
        assert_eq!(img.pixels[3], egui::Color32::from_rgb(250, 251, 252));
    }

    /// Пустой буфер (кадра ещё нет) не должен приводить к панике.
    #[test]
    fn empty_frame_is_a_single_black_pixel() {
        let img = rgb0_to_color_image(&[], 4, 4);
        assert_eq!(img.size, [1, 1]);
        assert_eq!(img.pixels, vec![egui::Color32::BLACK]);
    }

    /// Ручная проверка настоящего кадра через libmpv (тот путь, что падал:
    /// `load` → `render` → текстура). Требует `libmpv-2.dll` рядом с тестовым
    /// `.exe` (т.е. в `target\release\deps`). Запуск:
    ///
    /// ```text
    /// cargo test --release -- --ignored --nocapture render_real_frame
    /// ```
    ///
    /// По умолчанию используется синтетический поток mpv, свой адрес можно
    /// передать в `TW_TEST_URL` (например, живую HLS-ссылку).
    #[test]
    #[ignore]
    fn render_real_frame() {
        let url = std::env::var("TW_TEST_URL")
            .unwrap_or_else(|_| "av://lavfi:testsrc=size=320x180:rate=30".to_string());
        let mut player = MpvPlayer::new(true).expect("libmpv-2.dll не загрузился");
        player.load(&url).expect("loadfile");
        let mut frames = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while frames < 20 && std::time::Instant::now() < deadline {
            player.poll_events();
            if player.frame_ready() || player.take_dirty() {
                let img = player.render(160, 90).expect("render");
                // Ровно по одному Color32 на пиксель: рассинхрон здесь раньше
                // приводил к чтению за границей буфера и закрытию плеера.
                assert_eq!(img.pixels.len(), 160 * 90);
                assert_eq!(img.size, [160, 90]);
                frames += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            frames > 0,
            "кадров не получено (url={}, hwdec={:?})",
            url,
            player.hwdec_current()
        );
        println!(
            "ok: {} кадров, hwdec-current = {:?}",
            frames,
            player.hwdec_current()
        );
    }
}

/// Диагностика: загрузка DLL и наличие ключевых символов.
pub fn probe() -> String {
    let mut log = String::new();
    match load_library() {
        Ok(module) => {
            log.push_str("dll=loaded\n");
            for s in [
                "mpv_create",
                "mpv_initialize",
                "mpv_render_context_create",
                "mpv_render_context_render",
                "mpv_render_context_set_update_callback",
            ] {
                let p = unsafe { get_sym(module, s) };
                log.push_str(&format!("{}={}\n", s, if p.is_null() { "no" } else { "ok" }));
            }
        }
        Err(e) => log.push_str(&format!("dll_error={}\n", e)),
    }
    log
}