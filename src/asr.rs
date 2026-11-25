//! Распознавание речи через `whisper.cpp` (`whisper.dll`).
//!
//! `subs.rs` уже отдаёт 16 кГц моно — ровно то, что ждёт whisper, поэтому
//! буфер уходит в `whisper_full` без пересчёта частоты. DLL и модель в
//! репозитории не хранятся: их кладёт установщик либо `fetch-whisper.ps1`.
//! Загрузка динамическая (как в `libmpv.rs`), чтобы плеер запускался и без
//! движка — тогда в консоли просто появится понятная ошибка.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::LibraryLoader::{
    FreeLibrary, GetModuleHandleW, GetProcAddress, LoadLibraryExW, LoadLibraryW,
};

/// Искать зависимости загружаемой DLL в её собственном каталоге
/// (`ggml.dll`, `ggml-base.dll`, `ggml-cpu-*.dll` лежат рядом с `whisper.dll`).
const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

/// `struct whisper_ahead` из `whisper.h` (нужен только для layout параметров).
#[repr(C)]
#[derive(Clone, Copy)]
struct WhisperAhead {
    n_text_layer: c_int,
    n_head: c_int,
}

/// `struct whisper_aheads`.
#[repr(C)]
#[derive(Clone, Copy)]
struct WhisperAheads {
    n_heads: usize,
    heads: *const WhisperAhead,
}

/// `struct whisper_context_params` (whisper.cpp b5907).
///
/// Своя копия нужна, чтобы выключить GPU: официальная Windows-сборка
/// `whisper-bin-x64.zip` — CPU-only, и с `use_gpu = true` ggml падает на
/// `GGML_ASSERT(device)` вместо тихого отката на CPU.
#[repr(C)]
#[derive(Clone, Copy)]
struct CtxParams {
    use_gpu: bool,
    flash_attn: bool,
    gpu_device: c_int,
    dtw_token_timestamps: bool,
    dtw_aheads_preset: c_int,
    dtw_n_top: c_int,
    dtw_aheads: WhisperAheads,
    dtw_mem_size: usize,
}

/// Частота, которую ждёт whisper (совпадает с `subs::SAMPLE_RATE`).
pub const SAMPLE_RATE: u32 = 16_000;

/// Модель по умолчанию: q5_1 — компромисс «точность / ~60 МБ».
pub const DEFAULT_MODEL: &str = "ggml-base.en-q5_1.bin";
/// Запасная модель, если base ещё не скачана.
pub const FALLBACK_MODEL: &str = "ggml-tiny.en-q5_1.bin";

/// `whisper_sampling_strategy::WHISPER_SAMPLING_GREEDY`.
const SAMPLING_GREEDY: c_int = 0;

/// Сколько времени отводится на распознавание фразы до принудительной
/// остановки. Шесть секунд звука — это ~1.2 с работы модели, поэтому 1.5 с
/// плюс 0.35 с на секунду звука покрывают медленные машины с запасом.
const DECODE_BASE_MS: u64 = 1500;
const DECODE_PER_SEC_MS: f32 = 350.0;
/// Жёсткий потолок декодирования: дольше — это уже зацикливание модели.
const DECODE_CAP_MS: u64 = 6000;
/// Предел токенов на фразу: речь не быстрее ~3 слов в секунду (≈4 токена),
/// поэтому 8 токенов на секунду звука отсекают циклы «no no no no…».
const TOKENS_PER_SEC: f32 = 8.0;
/// Запас токенов на короткие фразы и знаки препинания.
const TOKENS_SLACK: c_int = 16;

/// Язык распознавания в виде C-строки: модель `.en` всё равно знает только его,
/// но без явного `language` whisper тратит время на определение языка.
const LANG_EN: &[u8] = b"en\0";

/// Префикс `struct whisper_full_params` из `whisper.h` (b5907).
///
/// Объявлены поля только до `abort_callback`: смещения первых полей заданы
/// заголовком и не меняются, а всё, что дальше (колбэки энкодера и грамматика),
/// плеер не трогает. Так можно поправить параметры в объекте, который вернул
/// сам `whisper.dll`, не копируя структуру целиком и не завися от её размера.
#[repr(C)]
struct FullParams {
    strategy: c_int,
    n_threads: c_int,
    n_max_text_ctx: c_int,
    offset_ms: c_int,
    duration_ms: c_int,
    translate: bool,
    no_context: bool,
    no_timestamps: bool,
    single_segment: bool,
    print_special: bool,
    print_progress: bool,
    print_realtime: bool,
    print_timestamps: bool,
    token_timestamps: bool,
    thold_pt: f32,
    thold_ptsum: f32,
    max_len: c_int,
    split_on_word: bool,
    max_tokens: c_int,
    debug_mode: bool,
    audio_ctx: c_int,
    tdrz_enable: bool,
    suppress_regex: *const c_char,
    initial_prompt: *const c_char,
    carry_initial_prompt: bool,
    prompt_tokens: *const i32,
    prompt_n_tokens: c_int,
    language: *const c_char,
    detect_language: bool,
    suppress_blank: bool,
    suppress_nst: bool,
    temperature: f32,
    max_initial_ts: f32,
    length_penalty: f32,
    temperature_inc: f32,
    entropy_thold: f32,
    logprob_thold: f32,
    no_speech_thold: f32,
    greedy_best_of: c_int,
    beam_size: c_int,
    beam_patience: f32,
    /// `abort_callback`: whisper зовёт его во время работы энкодера и декодера,
    /// `true` — прервать вычисления (тогда `whisper_full` вернёт -6/-8/-9).
    abort_callback: Option<unsafe extern "C" fn(*mut c_void) -> bool>,
    abort_callback_user_data: *mut c_void,
}

/// О несовпадении раскладки сообщаем один раз, а не на каждой фразе.
static TUNE_REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Момент (мс от первого вызова `now_ms`), после которого текущее
/// декодирование надо прервать. Атомарные, а не thread-local: ggml проверяет
/// `abort_callback` внутри вычислительного графа, и поток может оказаться
/// не тем, что запустил распознавание.
static DEADLINE_MS: AtomicU64 = AtomicU64::new(u64::MAX);
/// Сработал ли `abort_callback` в текущем вызове `whisper_full`.
static ABORTED: AtomicBool = AtomicBool::new(false);

/// Миллисекунды от первого обращения — монотонная шкала для дедлайна.
fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// `abort_callback` whisper: останавливает энкодер и декодер по времени.
///
/// Без него зацикливание модели на шуме («no no no no …») длится десятки
/// секунд: жадное декодирование ограничено только `n_max_text_ctx` (16 тыс.
/// токенов), а живой поток в это время стоит и субтитры пропадают.
unsafe extern "C" fn abort_when_due(_: *mut c_void) -> bool {
    if now_ms() < DEADLINE_MS.load(Ordering::Relaxed) {
        return false;
    }
    ABORTED.store(true, Ordering::Relaxed);
    true
}

/// Немедленно прерывает текущее распознавание.
///
/// Нужен при выключении CC: `Live::stop` дожидается потока, а живой проход
/// whisper иначе может идти до `DECODE_CAP_MS` и окно «зависает».
pub fn abort_now() {
    DEADLINE_MS.store(now_ms(), Ordering::Relaxed);
}

/// Сколько времени отводим на декодирование фразы такой длины.
fn decode_limit(secs: f32) -> Duration {
    let ms = DECODE_BASE_MS + (secs.max(0.0) * DECODE_PER_SEC_MS) as u64;
    Duration::from_millis(ms.min(DECODE_CAP_MS))
}

/// Настраивает параметры распознавания.
///
/// `streaming` — живой режим: каждая фраза распознаётся независимо (без
/// контекста предыдущей), одним сегментом и без таймкодов. Это дешевле и не
/// даёт модели «продолжать» прошлую фразу на тишине. `n_samples` — длина
/// буфера: по ней уменьшается аудио-контекст, иначе whisper кодирует полные
/// 30 секунд даже для двухсекундной фразы (почти секунда процессора впустую).
///
/// Возвращает `false`, если структура не похожа на `whisper_full_params`
/// (тогда работаем на умолчаниях DLL: лучше медленнее, чем с битой памятью).
unsafe fn tune_params(params: *mut c_void, streaming: bool, n_samples: usize) -> bool {
    if params.is_null() {
        return false;
    }
    let p = &mut *(params as *mut FullParams);
    // `strategy` и `n_threads` — первые два поля. Если там не то, что ждём,
    // DLL собрана из другой версии whisper.cpp: дальше писать нельзя.
    if p.strategy != SAMPLING_GREEDY || p.n_threads < 1 || p.n_threads > 512 {
        if !TUNE_REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            crate::debug_log(&format!(
                "asr: неожидаемые параметры whisper (strategy={}, threads={}), работаю на умолчаниях",
                p.strategy, p.n_threads
            ));
        }
        return false;
    }
    // 4 потока — умолчание whisper.cpp; на 8 ядрах больше даёт выигрыш ~20 %.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    p.n_threads = cores.clamp(1, 8) as c_int;
    p.print_progress = false;
    p.print_realtime = false;
    p.print_timestamps = false;
    p.print_special = false;
    if streaming {
        p.no_context = true;
        p.no_timestamps = true;
        p.single_segment = true;
        p.language = LANG_EN.as_ptr() as *const c_char;
        p.detect_language = false;
        p.translate = false;
        p.suppress_blank = true;
        p.suppress_nst = true;
        // Чуть строже умолчания (0.6): в стриме много музыки и звуков игры,
        // и на них модель охотнее выдаёт «тишину» вместо выдуманного текста.
        p.no_speech_thold = 0.5;
        // Без fallback-проходов по температуре: они нужны для «красивого»
        // текста, а в живом режиме важна предсказуемая задержка.
        p.temperature = 0.0;
        p.temperature_inc = 0.0;
        // Жадное декодирование оставляем как есть: `greedy.best_of = 1`
        // проверялся и дал повторы («I am testing the I am testing the …»)
        // на живом звуке — качество важнее десятков миллисекунд.
        // Энкодер получает только реальную длину фразы (+запас), а не 30 с:
        // 30 с = 1500 позиций, 3 с ≈ 150. Это главный выигрыш по времени.
        //
        // Запас 50 % и минимум 256 позиций — не «на всякий случай»: меньший
        // аудио-контекст (проверял 128–160) заставляет модель зацикливаться
        // («I am testing the I am testing the …») и терять конец фразы.
        // 256 позиций = 5.1 с звука: короткие фразы упираются в этот минимум,
        // поэтому дальше сокращать нечего — быстрее делает только нарезка
        // фраз короче (см. `SILENCE_CLOSE` в live.rs) и промежуточные строки.
        let secs = n_samples as f32 / SAMPLE_RATE as f32;
        let frames = (secs / 0.02).ceil() as c_int;
        let ctx = (frames + frames / 2 + 32).max(256);
        // Энкодер свёрточный с шагом 2: нечётная длина недопустима.
        p.audio_ctx = (((ctx + 1) / 2) * 2).min(1500);
        // Пределы на случай зацикливания. Жадное декодирование без них
        // ограничено только `n_max_text_ctx`: на двухсекундном шуме модель
        // выдавала тысячи токенов «no no no …» и работала 38 секунд, а
        // живой поток всё это время стоял.
        p.max_tokens = ((secs * TOKENS_PER_SEC) as c_int + TOKENS_SLACK).max(24);
        p.abort_callback = Some(abort_when_due);
        p.abort_callback_user_data = std::ptr::null_mut();
    }
    true
}

type FnInitFromFile = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FnFreeCtx = unsafe extern "C" fn(*mut c_void);
type FnDefaultParams = unsafe extern "C" fn(c_int) -> *mut c_void;
type FnFull = unsafe extern "C" fn(*mut c_void, *mut c_void, *const f32, c_int) -> c_int;
type FnCount = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnText = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;
type FnTime = unsafe extern "C" fn(*mut c_void, c_int) -> i64;
type FnInitWithParams = unsafe extern "C" fn(*const c_char, CtxParams) -> *mut c_void;
type FnDefaultCtxParams = unsafe extern "C" fn() -> CtxParams;
/// `whisper_full_get_segment_no_speech_prob`: вероятность «это не речь».
type FnSegmentNoSpeech = unsafe extern "C" fn(*mut c_void, c_int) -> f32;

/// Указатели на функции `whisper.dll`.
struct Api {
    init: FnInitFromFile,
    init_with_params: Option<FnInitWithParams>,
    default_ctx_params: Option<FnDefaultCtxParams>,
    free_ctx: FnFreeCtx,
    default_params: FnDefaultParams,
    full: FnFull,
    n_segments: FnCount,
    segment_text: FnText,
    segment_t0: FnTime,
    segment_t1: FnTime,
    /// Не во всех сборках whisper.cpp: если символа нет, фильтр речи просто
    /// не работает (живые субтитры продолжают идти).
    segment_no_speech: Option<FnSegmentNoSpeech>,
    free_params: Option<unsafe extern "C" fn(*mut c_void)>,
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

/// Каталог с моделями whisper (`%APPDATA%\twitch_mini_player\models`).
pub fn models_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let mut p = PathBuf::from(base);
    p.push("twitch_mini_player");
    p.push("models");
    p
}

/// Кандидаты, где может лежать `whisper.dll`.
fn dll_candidates() -> Vec<PathBuf> {
    let mut c: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            c.push(dir.join("whisper.dll"));
            c.push(dir.join("tools").join("whisper").join("whisper.dll"));
            // target\release\twitch_mini_player.exe → <проект>\tools\whisper
            if let Some(root) = dir.parent().and_then(|p| p.parent()) {
                c.push(root.join("tools").join("whisper").join("whisper.dll"));
            }
            // target\release\deps\twitch_mini_player-*.exe (тесты) → <проект>\tools\whisper
            if let Some(root) = dir
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent())
            {
                c.push(root.join("tools").join("whisper").join("whisper.dll"));
            }
        }
    }
    let mut app = models_dir();
    app.pop(); // models → twitch_mini_player
    c.push(app.join("tools").join("whisper").join("whisper.dll"));
    if let Ok(cwd) = std::env::current_dir() {
        c.push(cwd.join("tools").join("whisper").join("whisper.dll"));
        c.push(cwd.join("whisper.dll"));
    }
    c
}

/// Первый существующий путь к `whisper.dll`.
pub fn find_dll() -> Option<PathBuf> {
    dll_candidates().into_iter().find(|p| p.exists())
}

/// Подключает CPU-бэкенды ggml из каталога движка.
///
/// `whisper.dll` из `whisper-bin-x64.zip` не подгружает `ggml-cpu-*.dll` сам
/// (в его окружении `devices = 0`), из-за чего падает `GGML_ASSERT(device)`.
/// Явный вызов `ggml_backend_load_all_from_path` решает проблему.
unsafe fn load_backends(dir: &Path) {
    let h = GetModuleHandleW(wide("ggml.dll").as_ptr());
    if h == 0 {
        return;
    }
    let f = get_sym(h as isize, "ggml_backend_load_all_from_path");
    if f.is_null() {
        return;
    }
    let load: unsafe extern "C" fn(*const c_char) = std::mem::transmute(f);
    if let Ok(c) = CString::new(dir.to_string_lossy().as_bytes()) {
        load(c.as_ptr());
    }
}

fn load_library() -> Result<isize, String> {
    let mut errs: Vec<String> = Vec::new();
    for c in dll_candidates() {
        if !c.exists() {
            continue;
        }
        let w = wide(&c.to_string_lossy());
        let h = unsafe { LoadLibraryExW(w.as_ptr(), 0, LOAD_WITH_ALTERED_SEARCH_PATH) };
        if h != 0 {
            return Ok(h);
        }
        errs.push(format!(
            "{}: {}",
            c.display(),
            std::io::Error::last_os_error()
        ));
    }
    let w = wide("whisper.dll");
    let h = unsafe { LoadLibraryW(w.as_ptr()) };
    if h != 0 {
        return Ok(h);
    }
    if errs.is_empty() {
        errs.push(format!(
            "whisper.dll: {}",
            std::io::Error::last_os_error()
        ));
    }
    Err(format!(
        "движок whisper.dll не загрузился ({}). Положите его рядом с .exe \
         (папка tools\\whisper) или запустите scripts\\fetch-whisper.ps1",
        errs.join("; ")
    ))
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
    let p_init = sym!("whisper_init_from_file");
    let p_init_wp = get_sym(module, "whisper_init_from_file_with_params");
    let p_def_ctx = get_sym(module, "whisper_context_default_params");
    let p_free = sym!("whisper_free");
    let p_params = sym!("whisper_full_default_params_by_ref");
    let p_full = sym!("whisper_full");
    let p_nseg = sym!("whisper_full_n_segments");
    let p_text = sym!("whisper_full_get_segment_text");
    let p_t0 = sym!("whisper_full_get_segment_t0");
    let p_t1 = sym!("whisper_full_get_segment_t1");
    let p_no_speech = get_sym(module, "whisper_full_get_segment_no_speech_prob");
    let p_free_params = get_sym(module, "whisper_free_params");

    if !missing.is_empty() {
        return Err(format!(
            "в whisper.dll нет нужных символов: {}",
            missing.join(", ")
        ));
    }

    Ok(Api {
        init: std::mem::transmute(p_init),
        init_with_params: if p_init_wp.is_null() {
            None
        } else {
            Some(std::mem::transmute(p_init_wp))
        },
        default_ctx_params: if p_def_ctx.is_null() {
            None
        } else {
            Some(std::mem::transmute(p_def_ctx))
        },
        free_ctx: std::mem::transmute(p_free),
        default_params: std::mem::transmute(p_params),
        full: std::mem::transmute(p_full),
        n_segments: std::mem::transmute(p_nseg),
        segment_text: std::mem::transmute(p_text),
        segment_t0: std::mem::transmute(p_t0),
        segment_t1: std::mem::transmute(p_t1),
        segment_no_speech: if p_no_speech.is_null() {
            None
        } else {
            Some(std::mem::transmute(p_no_speech))
        },
        free_params: if p_free_params.is_null() {
            None
        } else {
            Some(std::mem::transmute(p_free_params))
        },
    })
}

/// Каталоги, где ищем файл модели.
fn model_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![models_dir()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("models"));
            if let Some(root) = dir.parent().and_then(|p| p.parent()) {
                dirs.push(root.join("models"));
            }
        }
    }
    dirs
}

/// Ищет модель: сначала `base.en`, затем `tiny.en`.
pub fn find_model() -> Option<PathBuf> {
    let dirs = model_dirs();
    for name in [DEFAULT_MODEL, FALLBACK_MODEL] {
        for d in &dirs {
            let p = d.join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    None
}

/// Один распознанный фрагмент: таймкоды в сотых долях секунды (как у whisper).
/// Таймкоды и `no_speech` нужны диагностике и тестам; живой оверлей берёт текст.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Segment {
    pub t0_cs: i64,
    pub t1_cs: i64,
    pub text: String,
    /// `no_speech_prob` сегмента: насколько модель считает, что это не речь
    /// (музыка, звуки игры, шум). В живом режиме по нему отсеиваются
    /// «субтитры» из игрового звука.
    pub no_speech: f32,
}

impl Segment {
    #[allow(dead_code)]
    pub fn start_secs(&self) -> f32 {
        self.t0_cs as f32 / 100.0
    }

    #[allow(dead_code)]
    pub fn end_secs(&self) -> f32 {
        self.t1_cs as f32 / 100.0
    }
}

/// Загруженный движок и модель. Дорогой объект: создаётся один раз на поток.
pub struct Asr {
    api: Api,
    module: isize,
    ctx: *mut c_void,
    model: PathBuf,
}

impl Asr {
    /// Загружает движок и модель по умолчанию (`base.en`, затем `tiny.en`).
    pub fn new() -> Result<Self, String> {
        let model = find_model().ok_or_else(|| {
            format!(
                "модель whisper не найдена: положите {} в {} (или в папку models рядом с .exe)",
                DEFAULT_MODEL,
                models_dir().display()
            )
        })?;
        Self::with_model(&model)
    }

    /// Загружает конкретный файл модели.
    pub fn with_model(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Err(format!("модель whisper не найдена: {}", path.display()));
        }
        let module = load_library()?;
        // Без этого whisper падает: ggml не видит CPU-бэкенды сам.
        if let Some(dir) = find_dll().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            unsafe { load_backends(&dir) };
        }
        let api = match unsafe { load_api(module) } {
            Ok(a) => a,
            Err(e) => {
                unsafe { FreeLibrary(module) };
                return Err(e);
            }
        };
        let cpath = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| "в пути к модели есть нулевой байт".to_string())?;
        let ctx = match (api.init_with_params, api.default_ctx_params) {
            (Some(init_wp), Some(defaults)) => {
                // CPU-only: официальная сборка whisper.cpp падает на
                // GGML_ASSERT(device), если оставить use_gpu = true.
                let mut p = unsafe { defaults() };
                p.use_gpu = false;
                p.flash_attn = false;
                p.dtw_token_timestamps = false;
                unsafe { init_wp(cpath.as_ptr(), p) }
            }
            _ => unsafe { (api.init)(cpath.as_ptr()) },
        };
        if ctx.is_null() {
            unsafe { FreeLibrary(module) };
            return Err(format!(
                "whisper не смог открыть модель: {}",
                path.display()
            ));
        }
        Ok(Self {
            api,
            module,
            ctx,
            model: path.to_path_buf(),
        })
    }

    pub fn model_path(&self) -> &Path {
        &self.model
    }

    /// Распознаёт буфер 16 кГц моно. Пустой ответ = тишина или не речь.
    #[allow(dead_code)]
    pub fn transcribe(&self, samples: &[f32]) -> Result<Vec<Segment>, String> {
        self.transcribe_opts(samples, false)
    }

    /// Распознаёт фразу в живом режиме: без контекста, одним сегментом,
    /// без таймкодов. Так дешевле и нет склейки с предыдущей фразой.
    pub fn transcribe_streaming(&self, samples: &[f32]) -> Result<Vec<Segment>, String> {
        self.transcribe_opts(samples, true)
    }

    fn transcribe_opts(&self, samples: &[f32], streaming: bool) -> Result<Vec<Segment>, String> {
        if samples.is_empty() {
            return Ok(Vec::new());
        }
        let n = i32::try_from(samples.len())
            .map_err(|_| "буфер длиннее i32 сэмплов".to_string())?;
        let params = unsafe { (self.api.default_params)(SAMPLING_GREEDY) };
        if params.is_null() {
            return Err("whisper_full_default_params_by_ref вернул null".to_string());
        }
        // Параметры правим до вызова: после `whisper_full` структуру забирает
        // `whisper_free_params`.
        unsafe {
            tune_params(params, streaming, samples.len());
        }
        // Срок действует только в живом режиме: `abort_callback` ставится там
        // же, в `tune_params`.
        let limit = streaming.then(|| decode_limit(samples.len() as f32 / SAMPLE_RATE as f32));
        if let Some(limit) = limit {
            ABORTED.store(false, Ordering::Relaxed);
            DEADLINE_MS.store(
                now_ms().saturating_add(limit.as_millis() as u64),
                Ordering::Relaxed,
            );
        }
        let rc = unsafe { (self.api.full)(self.ctx, params, samples.as_ptr(), n) };
        let aborted = ABORTED.swap(false, Ordering::Relaxed);
        DEADLINE_MS.store(u64::MAX, Ordering::Relaxed);
        if let Some(free) = self.api.free_params {
            unsafe { free(params) };
        }
        if aborted {
            // Обрывок прерванного прохода не показываем: это цикл, а не речь.
            if crate::debug_enabled() {
                let ms = limit.map(|l| l.as_millis()).unwrap_or(0);
                crate::debug_log(&format!(
                    "asr: распознавание прервано по времени ({ms} мс на {:.1} с звука) — модель зациклилась",
                    samples.len() as f32 / SAMPLE_RATE as f32
                ));
            }
            return Ok(Vec::new());
        }
        if rc != 0 {
            return Err(format!("whisper_full завершился с кодом {rc}"));
        }
        let count = unsafe { (self.api.n_segments)(self.ctx) }.max(0);
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let ptr = unsafe { (self.api.segment_text)(self.ctx, i) };
            let text = if ptr.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .trim()
                    .to_string()
            };
            if text.is_empty() {
                continue;
            }
            out.push(Segment {
                t0_cs: unsafe { (self.api.segment_t0)(self.ctx, i) },
                t1_cs: unsafe { (self.api.segment_t1)(self.ctx, i) },
                no_speech: match self.api.segment_no_speech {
                    Some(f) => unsafe { f(self.ctx, i) }.clamp(0.0, 1.0),
                    // Символа нет — считаем, что фильтр речи выключен.
                    None => 0.0,
                },
                text,
            });
        }
        Ok(out)
    }

    /// Текст без таймкодов — для проверки и логов.
    #[allow(dead_code)]
    pub fn text(&self, samples: &[f32]) -> Result<String, String> {
        let segs = self.transcribe(samples)?;
        Ok(segs
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" "))
    }

    /// Текст живой фразы (без контекста, без таймкодов).
    pub fn text_streaming(&self, samples: &[f32]) -> Result<String, String> {
        Ok(self.text_streaming_checked(samples)?.0)
    }

    /// Текст живой фразы и «вероятность, что это не речь» (максимум по
    /// сегментам). По второму значению живой режим отбрасывает музыку и звуки
    /// игры: whisper помечает их высоким `no_speech_prob`, даже если выдал
    /// правдоподобную фразу.
    pub fn text_streaming_checked(&self, samples: &[f32]) -> Result<(String, f32), String> {
        let segs = self.transcribe_streaming(samples)?;
        let no_speech = segs.iter().map(|s| s.no_speech).fold(0.0f32, f32::max);
        let text = segs
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        Ok((text, no_speech))
    }
}

impl Drop for Asr {
    fn drop(&mut self) {
        unsafe {
            (self.api.free_ctx)(self.ctx);
            FreeLibrary(self.module);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_dir_is_under_appdata() {
        assert!(models_dir().ends_with("models"), "{:?}", models_dir());
    }

    #[test]
    fn missing_model_is_reported() {
        let err = match Asr::with_model(Path::new("no-such-model.bin")) {
            Ok(_) => panic!("несуществующая модель не должна загружаться"),
            Err(e) => e,
        };
        assert!(err.contains("не найдена"), "{err}");
    }

    /// Диагностика бэкендов ggml: почему `devices = 0`.
    /// Запуск: `cargo test --release diag_backends -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn diag_backends() {
        let module = load_library().expect("whisper.dll загружается");
        unsafe {
            let h = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(
                wide("ggml.dll").as_ptr(),
            );
            println!("ggml.dll module = {h}");
            if h != 0 {
                for name in [
                    "ggml_backend_load_all",
                    "ggml_backend_load_all_from_path",
                    "ggml_backend_dev_count",
                ] {
                    println!("  {name}: {}", !get_sym(h as isize, name).is_null());
                }
            }
            let dll = find_dll().expect("путь к whisper.dll");
            let dir = dll.parent().expect("каталог движка").to_path_buf();
            for f in ["ggml-cpu-x64.dll", "ggml-base.dll", "ggml.dll"] {
                let p = dir.join(f);
                let w = wide(&p.to_string_lossy());
                let h2 = LoadLibraryExW(w.as_ptr(), 0, LOAD_WITH_ALTERED_SEARCH_PATH);
                println!(
                    "load {} -> {h2} err={}",
                    p.display(),
                    std::io::Error::last_os_error()
                );
            }
            println!("whisper module = {module}");
        }
    }

    /// Живой прогон: распознаёт `%APPDATA%\twitch_mini_player\capture.wav`.
    /// Запуск: `cargo test --release -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_transcribes_capture_wav() {
        let model = find_model().expect("модель скачана (scripts\\fetch-whisper.ps1)");
        let mut wav = models_dir();
        wav.pop();
        wav.push("capture.wav");
        assert!(wav.exists(), "нет файла записи: {}", wav.display());
        let samples = read_wav_mono16(&wav).expect("WAV читается");
        assert!(
            samples.len() > SAMPLE_RATE as usize,
            "в записи больше секунды звука"
        );
        let asr = Asr::with_model(&model).expect("движок и модель загружаются");
        let segs = asr.transcribe(&samples).expect("распознавание проходит");
        for s in &segs {
            println!("[{:.2}–{:.2}] {}", s.start_secs(), s.end_secs(), s.text);
        }
        assert!(!segs.is_empty(), "речь в записи распознана");
    }

    /// Минимальный разбор WAV: 16-бит PCM, моно/стерео → моно `f32`.
    fn read_wav_mono16(path: &Path) -> Result<Vec<f32>, String> {
        let b = std::fs::read(path).map_err(|e| e.to_string())?;
        if b.len() < 44 || &b[0..4] != b"RIFF".as_slice() || &b[8..12] != b"WAVE".as_slice() {
            return Err("это не WAV".to_string());
        }
        let mut pos = 12;
        let mut channels = 1usize;
        let mut rate = 0u32;
        while pos + 8 <= b.len() {
            let id = &b[pos..pos + 4];
            let size = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
            let body = pos + 8;
            if id == b"fmt ".as_slice() {
                channels = u16::from_le_bytes([b[body + 2], b[body + 3]]) as usize;
                rate = u32::from_le_bytes([b[body + 4], b[body + 5], b[body + 6], b[body + 7]]);
            } else if id == b"data".as_slice() {
                if rate != SAMPLE_RATE {
                    return Err(format!("ожидали {SAMPLE_RATE} Гц, в файле {rate}"));
                }
                let ch = channels.max(1);
                let end = (body + size).min(b.len());
                let mut out = Vec::with_capacity((end - body) / (2 * ch));
                for frame in b[body..end].chunks_exact(2 * ch) {
                    let mut acc = 0f32;
                    for c in 0..ch {
                        let o = c * 2;
                        acc += i16::from_le_bytes([frame[o], frame[o + 1]]) as f32 / 32768.0;
                    }
                    out.push(acc / ch as f32);
                }
                return Ok(out);
            }
            pos = body + size + (size & 1);
        }
        Err("в WAV нет чанка data".to_string())
    }
}
