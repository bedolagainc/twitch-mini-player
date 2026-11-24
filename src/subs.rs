//! Захват звука **только своего процесса** для субтитров (этап 0.18.0).
//!
//! Зачем именно так. Обычный loopback-захват (`AUDCLNT_STREAMFLAGS_LOOPBACK`
//! на устройстве вывода) отдаёт весь звук компьютера: музыка из браузера,
//! игра, уведомления, голосовой чат. В субтитры это превращалось бы в мусор.
//! Начиная с Windows 11 (build 20348) есть *process loopback*: виртуальное
//! устройство `VAD\Process_Loopback`, которому при активации передаётся PID —
//! и оно отдаёт звук только этого процесса и его дочерних. Наш плеер играет
//! поток через mpv в своём же процессе, поэтому в субтитрах оказывается ровно
//! звук стрима.
//!
//! Что делаем со звуком: микшируем в моно и пересчитываем в 16 кГц — формат,
//! который ждёт whisper.cpp. Складываем в ограниченное кольцо (30 с) и считаем
//! уровень для индикатора. Распознавания в 0.18.0 ещё нет: в консоли видно
//! уровень и частоту, а кнопка «Записать 10 с» кладёт WAV рядом с настройками,
//! чтобы убедиться на слух: захватывается только стример.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows_sys::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, BLOB, COINIT_MULTITHREADED, VT_BLOB};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

/// Частота, с которой дальше будет работать распознавание речи (whisper.cpp).
pub const SAMPLE_RATE: u32 = 16_000;
/// Сколько секунд звука держим в кольце: хватает на любую фразу и на 10 с записи.
const RING_SECS: usize = 30;
/// Сколько ждём активации виртуального устройства (обычно 20–80 мс).
const ACTIVATE_TIMEOUT_MS: u32 = 4_000;
/// Период буфера захвата: 20 мс — компромисс между задержкой и числом пробуждений.
const BUFFER_PERIOD_100NS: i64 = 200_000;

/// Формат, в котором устройство отдаёт звук. Мы его задаём сами (16-бит PCM),
/// поэтому здесь только число каналов и частота.
#[derive(Debug, Clone, Copy)]
pub struct MixFormat {
    pub channels: u16,
    pub rate: u32,
}

const WAVE_FORMAT_PCM: u16 = 1;

// --- GUID'ы интерфейсов (значения из audioclient.h, ABI не меняется) ---

const IID_IAUDIO_CLIENT: GUID = GUID {
    data1: 0x1CB9_AD4C,
    data2: 0xDBFA,
    data3: 0x4C32,
    data4: [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
};

const IID_IAUDIO_CAPTURE_CLIENT: GUID = GUID {
    data1: 0xC8AD_BD64,
    data2: 0xE71E,
    data3: 0x48A0,
    data4: [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17],
};

const IID_IAUDIO_COMPLETION_HANDLER: GUID = GUID {
    data1: 0x41D9_49AB,
    data2: 0x9862,
    data3: 0x444A,
    data4: [0x80, 0xF6, 0xC2, 0x61, 0x33, 0x4D, 0xA5, 0xEB],
};

const IID_IUNKNOWN: GUID = GUID {
    data1: 0,
    data2: 0,
    data3: 0,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};

/// `IAgileObject` (пустой интерфейс-маркер). Без него Windows отвечает
/// `E_ILLEGAL_METHOD_CALL` (0x8000000E): обработчик вызывается из MTA и должен
/// быть «agile», иначе его пришлось бы маршалить между апартаментами.
const IID_IAGILE_OBJECT: GUID = GUID {
    data1: 0x94EA_2B94,
    data2: 0xE9CC,
    data3: 0x49E0,
    data4: [0xC0, 0xFF, 0xEE, 0x64, 0xCA, 0x8F, 0x5B, 0x90],
};

/// Сравнение GUID по полям: `windows-sys` 0.48 не даёт `PartialEq` для `GUID`.
fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1
        && a.data2 == b.data2
        && a.data3 == b.data3
        && a.data4 == b.data4
}

/// `hr` в человекочитаемый текст: HRESULT без внешних крейтов.
fn hr_text(hr: i32) -> String {
    format!("0x{:08X}", hr as u32)
}

// --- COM-интерфейсы как vtables ---
//
// `windows-sys` (в отличие от `windows`) не генерирует методы для COM: только
// сырые указатели. Поэтому объявляем vtable сами — порядок методов фиксирован
// заголовками Windows SDK и от версии к версии не меняется.

#[repr(C)]
struct IUnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct AudioClientVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    initialize: unsafe extern "system" fn(
        *mut c_void,
        i32,
        u32,
        i64,
        i64,
        *const WAVEFORMATEX,
        *const GUID,
    ) -> i32,
    get_buffer_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    get_stream_latency: unsafe extern "system" fn(*mut c_void, *mut i64) -> i32,
    get_current_padding: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
    is_format_supported: unsafe extern "system" fn(
        *mut c_void,
        i32,
        *const WAVEFORMATEX,
        *mut *mut WAVEFORMATEX,
    ) -> i32,
    get_mix_format: unsafe extern "system" fn(*mut c_void, *mut *mut WAVEFORMATEX) -> i32,
    get_device_period: unsafe extern "system" fn(*mut c_void, *mut i64, *mut i64) -> i32,
    start: unsafe extern "system" fn(*mut c_void) -> i32,
    stop: unsafe extern "system" fn(*mut c_void) -> i32,
    reset: unsafe extern "system" fn(*mut c_void) -> i32,
    set_event_handle: unsafe extern "system" fn(*mut c_void, HANDLE) -> i32,
    get_service: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct AudioCaptureClientVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_buffer: unsafe extern "system" fn(
        *mut c_void,
        *mut *mut u8,
        *mut u32,
        *mut u32,
        *mut u64,
        *mut u64,
    ) -> i32,
    release_buffer: unsafe extern "system" fn(*mut c_void, u32) -> i32,
    get_next_packet_size: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32,
}

#[repr(C)]
struct AsyncOperationVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_activate_result: unsafe extern "system" fn(*mut c_void, *mut i32, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct CompletionHandlerVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    activate_completed: unsafe extern "system" fn(*mut c_void, *mut c_void),
}

/// vtable по указателю на COM-объект (первое поле объекта — указатель на vtable).
unsafe fn vtbl<T>(obj: *mut c_void) -> *const T {
    *(obj as *const *const T)
}

/// `Release()` любого COM-объекта.
unsafe fn com_release(obj: *mut c_void) {
    if obj.is_null() {
        return;
    }
    let v = vtbl::<IUnknownVtbl>(obj);
    ((*v).release)(obj);
}

const S_OK: i32 = 0;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
const E_POINTER: i32 = 0x8000_4003u32 as i32;
/// `WAIT_OBJECT_0`: событие дождались.
const WAIT_OBJECT_0: u32 = 0;

// --- Обработчик завершения активации ---

/// COM-объект `IActivateAudioInterfaceCompletionHandler`. Windows вызывает
/// `ActivateCompleted` из своего потока; нам достаточно «дёрнуть» событие,
/// на котором ждёт поток захвата.
#[repr(C)]
struct Handler {
    vtbl: *const CompletionHandlerVtbl,
    refs: AtomicU32,
    event: HANDLE,
}

unsafe extern "system" fn handler_query_interface(
    this: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() || riid.is_null() {
        return E_POINTER;
    }
    let iid = &*riid;
    if guid_eq(iid, &IID_IUNKNOWN)
        || guid_eq(iid, &IID_IAUDIO_COMPLETION_HANDLER)
        || guid_eq(iid, &IID_IAGILE_OBJECT)
    {
        *out = this;
        (*(this as *const Handler))
            .refs
            .fetch_add(1, Ordering::SeqCst);
        S_OK
    } else {
        *out = std::ptr::null_mut();
        E_NOINTERFACE
    }
}

unsafe extern "system" fn handler_add_ref(this: *mut c_void) -> u32 {
    let h = &*(this as *const Handler);
    h.refs.fetch_add(1, Ordering::SeqCst) + 1
}

unsafe extern "system" fn handler_release(this: *mut c_void) -> u32 {
    let h = &*(this as *const Handler);
    let left = h.refs.fetch_sub(1, Ordering::SeqCst) - 1;
    if left == 0 {
        drop(Box::from_raw(this as *mut Handler));
    }
    left
}

unsafe extern "system" fn handler_activate_completed(this: *mut c_void, _op: *mut c_void) {
    let h = &*(this as *const Handler);
    SetEvent(h.event);
}

static HANDLER_VTBL: CompletionHandlerVtbl = CompletionHandlerVtbl {
    query_interface: handler_query_interface,
    add_ref: handler_add_ref,
    release: handler_release,
    activate_completed: handler_activate_completed,
};

/// Создаёт обработчик и событие, на котором будем ждать активацию.
unsafe fn create_handler() -> Result<(*mut Handler, HANDLE), String> {
    let event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
    if event == 0 {
        return Err("CreateEventW: не удалось создать событие".to_string());
    }
    let handler = Box::new(Handler {
        vtbl: &HANDLER_VTBL,
        refs: AtomicU32::new(1),
        event,
    });
    Ok((Box::into_raw(handler), event))
}

// --- Активация виртуального устройства «звук процесса» ---

/// Создаёт `IAudioClient` для виртуального устройства `VAD\Process_Loopback`
/// с PID нашего процесса: звук других программ в него не попадает.
unsafe fn activate_process_loopback(pid: u32) -> Result<*mut c_void, String> {
    let (handler, event) = create_handler()?;

    // Параметры активации: включаем процесс и всё его дерево (наши же окна).
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };

    // PROPVARIANT с BLOB-ом на эти параметры — так требует API.
    let mut pv: PROPVARIANT = std::mem::zeroed();
    let inner = &mut pv.Anonymous.Anonymous;
    inner.vt = VT_BLOB;
    inner.Anonymous.blob = BLOB {
        cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        pBlobData: &mut params as *mut _ as *mut u8,
    };

    let mut op: *mut c_void = std::ptr::null_mut();
    let hr = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IID_IAUDIO_CLIENT,
        &pv,
        handler as IActivateAudioInterfaceCompletionHandler,
        &mut op as *mut *mut c_void as *mut IActivateAudioInterfaceAsyncOperation,
    );
    if hr < 0 {
        com_release(handler as *mut c_void);
        CloseHandle(event);
        return Err(format!(
            "ActivateAudioInterfaceAsync: {} (нужна Windows 11 build 20348+)",
            hr_text(hr)
        ));
    }

    let wait = WaitForSingleObject(event, ACTIVATE_TIMEOUT_MS);
    CloseHandle(event);
    if wait != WAIT_OBJECT_0 {
        com_release(op as *mut c_void);
        com_release(handler as *mut c_void);
        return Err("устройство захвата не активировалось (таймаут)".to_string());
    }

    let mut activate_hr: i32 = 0;
    let mut client: *mut c_void = std::ptr::null_mut();
    let v = vtbl::<AsyncOperationVtbl>(op);
    let hr = ((*v).get_activate_result)(op, &mut activate_hr, &mut client);
    com_release(op as *mut c_void);
    com_release(handler as *mut c_void);
    if hr < 0 {
        return Err(format!("GetActivateResult: {}", hr_text(hr)));
    }
    if activate_hr < 0 {
        return Err(format!("активация захвата: {}", hr_text(activate_hr)));
    }
    if client.is_null() {
        return Err("активация захвата вернула NULL".to_string());
    }
    Ok(client)
}

/// Формат, в котором просим звук у виртуального устройства.
///
/// `GetMixFormat` у process-looback устройства возвращает `E_NOTIMPL`:
/// микшера у него нет, формат задаёт вызывающий. Поэтому перебираем обычные
/// варианты (так же поступает пример Microsoft ApplicationLoopback) — берём
/// первый, который примет `Initialize`. Для распознавания речи 16-бит PCM
/// более чем достаточно, а моно/стерео приводим к моно сами.
fn candidate_formats() -> [MixFormat; 4] {
    [
        MixFormat {
            channels: 2,
            rate: 48_000,
        },
        MixFormat {
            channels: 2,
            rate: 44_100,
        },
        MixFormat {
            channels: 1,
            rate: 48_000,
        },
        MixFormat {
            channels: 1,
            rate: 16_000,
        },
    ]
}

/// `WAVEFORMATEX` под 16-битный PCM с нужным числом каналов и частотой.
fn wave_format(fmt: MixFormat) -> WAVEFORMATEX {
    let block_align = fmt.channels * 2;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: fmt.channels,
        nSamplesPerSec: fmt.rate,
        nAvgBytesPerSec: fmt.rate * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Готовит клиент к захвату: общий режим, loopback + события, буфер 20 мс.
unsafe fn init_capture(
    client: *mut c_void,
    fmt: MixFormat,
    data_event: HANDLE,
) -> Result<*mut c_void, String> {
    let v = vtbl::<AudioClientVtbl>(client);
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let format = wave_format(fmt);
    let hr = ((*v).initialize)(
        client,
        AUDCLNT_SHAREMODE_SHARED,
        flags,
        BUFFER_PERIOD_100NS,
        0,
        &format,
        std::ptr::null(),
    );
    if hr < 0 {
        return Err(format!(
            "Initialize({} Гц, {} кан): {}",
            fmt.rate,
            fmt.channels,
            hr_text(hr)
        ));
    }
    let hr = ((*v).set_event_handle)(client, data_event);
    if hr < 0 {
        return Err(format!("SetEventHandle: {}", hr_text(hr)));
    }
    let mut capture: *mut c_void = std::ptr::null_mut();
    let hr = ((*v).get_service)(client, &IID_IAUDIO_CAPTURE_CLIENT, &mut capture);
    if hr < 0 || capture.is_null() {
        return Err(format!("GetService(IAudioCaptureClient): {}", hr_text(hr)));
    }
    let hr = ((*v).start)(client);
    if hr < 0 {
        com_release(capture);
        return Err(format!("IAudioClient::Start: {}", hr_text(hr)));
    }
    Ok(capture)
}

/// Микширует блок PCM устройства в моно float (`out` очищается).
///
/// `bytes` — указатель из `IAudioCaptureClient::GetBuffer`, валиден только
/// до `ReleaseBuffer`, поэтому копируем значения сразу.
unsafe fn convert_mono(bytes: *const u8, frames: usize, channels: u16, out: &mut Vec<f32>) {
    let ch = channels.max(1) as usize;
    let count = frames * ch;
    out.clear();
    out.reserve(frames);
    let src = std::slice::from_raw_parts(bytes as *const i16, count);
    for frame in src.chunks_exact(ch) {
        let sum: i32 = frame.iter().map(|&s| s as i32).sum();
        out.push(sum as f32 / (ch as f32 * 32768.0));
    }
}

/// Линейный пересчёт частоты (48 кГц → 16 кГц и любые другие пары).
///
/// Для речи этого достаточно: whisper всё равно считает мел-спектрограмму,
/// а полноценный sinc-ресемплер стоил бы лишних зависимостей и ЦП. Состояние
/// (`pos`, `last`) сохраняется между блоками — на стыках не слышно щелчков.
struct Resampler {
    src_rate: u32,
    dst_rate: u32,
    pos: f64,
    last: f32,
    /// Был ли уже хотя бы один блок: на самом первом `last` ещё не задан.
    primed: bool,
}

impl Resampler {
    fn new(src_rate: u32) -> Self {
        Self {
            src_rate: src_rate.max(1),
            dst_rate: SAMPLE_RATE,
            pos: 0.0,
            last: 0.0,
            primed: false,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        if input.is_empty() {
            return;
        }
        if self.src_rate == self.dst_rate {
            out.extend_from_slice(input);
            self.last = input[input.len() - 1];
            self.primed = true;
            return;
        }
        let step = self.src_rate as f64 / self.dst_rate as f64;
        let len = input.len();
        // Виртуальный буфер: индекс 0 — последний сэмпл прошлого блока,
        // индекс k — input[k - 1].
        out.reserve((len as f64 / step) as usize + 2);
        while self.pos < len as f64 {
            let i = self.pos.floor() as usize;
            let frac = (self.pos - i as f64) as f32;
            let s0 = if i == 0 {
                if self.primed {
                    self.last
                } else {
                    input[0]
                }
            } else {
                input[i - 1]
            };
            let s1 = input[i.min(len - 1)];
            out.push(s0 + (s1 - s0) * frac);
            self.pos += step;
        }
        self.pos -= len as f64;
        self.last = input[len - 1];
        self.primed = true;
    }
}

/// Сколько сэмплов 16 кГц держим в кольце.
const RING_CAP: usize = RING_SECS * SAMPLE_RATE as usize;

/// Кольцевой буфер: старые сэмплы вытесняются, новые всегда на месте.
struct Ring {
    data: std::collections::VecDeque<f32>,
    cap: usize,
    dropped: u64,
}

impl Ring {
    fn new(cap: usize) -> Self {
        Self {
            data: std::collections::VecDeque::with_capacity(cap.min(1024)),
            cap,
            dropped: 0,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        for &s in samples {
            if self.data.len() >= self.cap {
                self.data.pop_front();
                self.dropped += 1;
            }
            self.data.push_back(s);
        }
    }

    fn drain_all(&mut self) -> Vec<f32> {
        self.data.drain(..).collect()
    }

    /// Последние `n` сэмплов, не вытесняя их (для распознавания).
    fn tail(&self, n: usize) -> Vec<f32> {
        let n = n.min(self.data.len());
        self.data
            .iter()
            .skip(self.data.len() - n)
            .copied()
            .collect()
    }
}

/// Состояние, общее для UI и потока захвата.
struct Shared {
    ring: Mutex<Ring>,
    /// Уровень последнего блока (RMS) как биты `f32`.
    level: AtomicU32,
    /// Всего сэмплов 16 кГц получено за сессию.
    total: AtomicU64,
    /// Сколько сэмплов вытеснено из кольца (UI не успевает забирать).
    dropped: AtomicU64,
    format: Mutex<Option<MixFormat>>,
    /// Текст ошибки или примечания из потока захвата.
    note: Mutex<String>,
    running: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            ring: Mutex::new(Ring::new(RING_CAP)),
            level: AtomicU32::new(0),
            total: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            format: Mutex::new(None),
            note: Mutex::new(String::new()),
            running: AtomicBool::new(false),
        }
    }

    fn set_note(&self, text: String) {
        if let Ok(mut n) = self.note.lock() {
            *n = text;
        }
    }
}

/// Захват звука процесса: поток живёт до `stop()`/`Drop`.
pub struct Capture {
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Capture {
    /// Запускает захват и ждёт готовности (активация устройства — обычно
    /// несколько десятков миллисекунд).
    pub fn start() -> Result<Self, String> {
        let shared = Arc::new(Shared::new());
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let sh = Arc::clone(&shared);
        let st = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("subs-capture".to_string())
            .spawn(move || capture_thread(sh, st, tx))
            .map_err(|e| format!("не удалось создать поток захвата: {e}"))?;

        let wait = Duration::from_millis(ACTIVATE_TIMEOUT_MS as u64 + 1000);
        match rx.recv_timeout(wait) {
            Ok(Ok(())) => Ok(Self {
                shared,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                Err("захват не запустился: виртуальное устройство не ответило".to_string())
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                // Поток упал до готовности — забираем текст причины, если он есть.
                let note = shared.note.lock().ok().map(|n| n.clone()).unwrap_or_default();
                if note.is_empty() {
                    Err("поток захвата завершился при запуске".to_string())
                } else {
                    Err(note)
                }
            }
        }
    }

    /// Останавливает поток и освобождает WASAPI-объекты.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }

    /// Уровень последнего блока (RMS, 0…1).
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    /// Забирает накопленные сэмплы 16 кГц (в порядке поступления).
    pub fn drain(&self) -> Vec<f32> {
        match self.shared.ring.lock() {
            Ok(mut r) => r.drain_all(),
            Err(e) => e.into_inner().drain_all(),
        }
    }

    /// Последние `n` сэмплов 16 кГц, не забирая их из кольца.
    pub fn last(&self, n: usize) -> Vec<f32> {
        match self.shared.ring.lock() {
            Ok(r) => r.tail(n),
            Err(e) => e.into_inner().tail(n),
        }
    }

    /// Ошибка/примечание из потока захвата (забирается один раз).
    pub fn take_note(&self) -> Option<String> {
        let mut n = self.shared.note.lock().ok()?;
        if n.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut *n))
        }
    }

    /// Формат устройства и счётчики — для строки статистики в консоли.
    pub fn stats(&self) -> (Option<MixFormat>, f64, u64) {
        let fmt = self.shared.format.lock().ok().and_then(|f| *f);
        let secs = self.shared.total.load(Ordering::Relaxed) as f64 / SAMPLE_RATE as f64;
        let dropped = self.shared.dropped.load(Ordering::Relaxed);
        (fmt, secs, dropped)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Точка входа потока захвата: COM инициализируется и освобождается здесь же.
fn capture_thread(shared: Arc<Shared>, stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>) {
    unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED) };
    let result = unsafe { run_capture(&shared, &stop, &ready) };
    unsafe { CoUninitialize() };
    if let Err(e) = result {
        crate::debug_log(&format!("subs: захват остановлен: {e}"));
        shared.set_note(e);
    }
    shared.running.store(false, Ordering::SeqCst);
}

/// RMS блока — для индикатора уровня.
fn rms_of(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Вся жизнь захвата: активация → чтение пакетов → освобождение ресурсов.
unsafe fn run_capture(
    shared: &Arc<Shared>,
    stop: &Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let client = activate_process_loopback(std::process::id())?;
    let data_event = CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
    if data_event == 0 {
        com_release(client);
        return Err("CreateEventW: не удалось создать событие данных".to_string());
    }
    // Формат задаём сами (см. `candidate_formats`) и пробуем по очереди.
    let mut mix = None;
    let mut capture: *mut c_void = std::ptr::null_mut();
    let mut last_error = String::new();
    for candidate in candidate_formats() {
        match init_capture(client, candidate, data_event) {
            Ok(c) => {
                mix = Some(candidate);
                capture = c;
                break;
            }
            Err(e) => last_error = e,
        }
    }
    let Some(mix) = mix else {
        CloseHandle(data_event);
        com_release(client);
        return Err(format!("не удалось подобрать формат захвата: {last_error}"));
    };
    let fmt = mix;
    crate::debug_log(&format!(
        "subs: захват звука процесса запущен: {} Гц, {} кан",
        fmt.rate, fmt.channels
    ));
    if let Ok(mut slot) = shared.format.lock() {
        *slot = Some(fmt);
    }
    shared.running.store(true, Ordering::SeqCst);
    let _ = ready.send(Ok(()));

    let vt = vtbl::<AudioCaptureClientVtbl>(capture);
    let mut mono: Vec<f32> = Vec::new();
    let mut res: Vec<f32> = Vec::new();
    let mut rs = Resampler::new(fmt.rate);
    let mut error: Option<String> = None;

    while !stop.load(Ordering::SeqCst) {
        // Событие приходит примерно каждые 20 мс; таймаут — чтобы замечать stop.
        if WaitForSingleObject(data_event, 200) != WAIT_OBJECT_0 {
            continue;
        }
        loop {
            let mut packet: u32 = 0;
            let hr = ((*vt).get_next_packet_size)(capture, &mut packet);
            if hr < 0 {
                error = Some(format!("GetNextPacketSize: {}", hr_text(hr)));
                break;
            }
            if packet == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            let hr = ((*vt).get_buffer)(
                capture,
                &mut data,
                &mut frames,
                &mut flags,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if hr < 0 {
                error = Some(format!("GetBuffer: {}", hr_text(hr)));
                break;
            }
            if frames > 0 && !data.is_null() {
                if flags & AUDCLNT_BUFFERFLAGS_SILENT as u32 != 0 {
                    mono.clear();
                    mono.resize(frames as usize, 0.0);
                } else {
                    convert_mono(data, frames as usize, fmt.channels, &mut mono);
                }
                shared
                    .level
                    .store(rms_of(&mono).to_bits(), Ordering::Relaxed);
                rs.process(&mono, &mut res);
                let dropped = match shared.ring.lock() {
                    Ok(mut ring) => {
                        ring.push(&res);
                        ring.dropped
                    }
                    Err(e) => {
                        let mut ring = e.into_inner();
                        ring.push(&res);
                        ring.dropped
                    }
                };
                shared.total.fetch_add(res.len() as u64, Ordering::Relaxed);
                shared.dropped.store(dropped, Ordering::Relaxed);
            }
            ((*vt).release_buffer)(capture, frames);
        }
        if error.is_some() {
            break;
        }
    }

    // Порядок важен: сначала останавливаем поток, потом отпускаем интерфейсы.
    let v = vtbl::<AudioClientVtbl>(client);
    ((*v).stop)(client);
    com_release(capture);
    com_release(client);
    CloseHandle(data_event);
    error.map_or(Ok(()), Err)
}

/// Сколько секунд пишем в тестовый WAV.
pub const RECORD_SECS: f32 = 10.0;

/// Куда складывать тестовый WAV: рядом с настройками плеера.
pub fn default_wav_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
    let mut p = PathBuf::from(base);
    p.push("twitch_mini_player");
    p.push("capture.wav");
    p
}

/// Пишет 16-битный PCM-моно WAV: его можно открыть любым плеером и услышать,
/// что именно попало в захват (только ли стример).
pub fn write_wav(path: &std::path::Path, samples: &[f32], rate: u32) -> std::io::Result<()> {
    std::fs::write(path, wav_bytes(samples, rate))
}

/// WAV-файл в памяти: тот же формат, что и `write_wav` (16-бит PCM моно).
pub fn wav_bytes(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // размер блока fmt
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // моно
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes()); // байт/с
    out.extend_from_slice(&2u16.to_le_bytes()); // выравнивание блока
    out.extend_from_slice(&16u16.to_le_bytes()); // бит на сэмпл
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Состояние субтитров в интерфейсе. В 0.18.0 — только захват звука:
/// индикатор уровня, статистика и тестовая запись WAV.
pub struct Subs {
    /// Захват включён пользователем (сохраняется в настройках).
    pub enabled: bool,
    capture: Option<Capture>,
    recording: Option<Vec<f32>>,
    pub last_error: Option<String>,
    pub last_note: Option<String>,
}

impl Subs {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            capture: None,
            recording: None,
            last_error: None,
            last_note: None,
        }
    }

    /// Включает/выключает захват; повторный вызов с тем же значением безвреден.
    pub fn set_enabled(&mut self, on: bool) {
        if self.enabled == on {
            return;
        }
        self.enabled = on;
        if on {
            self.last_error = None;
        } else {
            self.recording = None;
        }
    }

    /// Стартует/останавливает захват и ведёт запись WAV. Вызывается каждый кадр.
    pub fn tick(&mut self) {
        if !self.enabled {
            // `Capture::drop` останавливает поток и закрывает WASAPI-объекты.
            self.capture = None;
            self.recording = None;
            return;
        }
        if self.capture.is_none() {
            match Capture::start() {
                Ok(c) => {
                    self.last_error = None;
                    self.capture = Some(c);
                }
                Err(e) => {
                    self.last_error = Some(e);
                    self.enabled = false;
                }
            }
            return;
        }
        // Поток мог остановиться сам (сменилось устройство вывода, ошибка WASAPI).
        if let Some(note) = self.capture.as_ref().and_then(|c| c.take_note()) {
            self.last_error = Some(note);
            self.enabled = false;
            self.capture = None;
            self.recording = None;
            return;
        }
        if self.recording.is_some() {
            let chunk = self.capture.as_ref().map(|c| c.drain()).unwrap_or_default();
            let need = (RECORD_SECS * SAMPLE_RATE as f32) as usize;
            let mut finished: Option<Vec<f32>> = None;
            if let Some(rec) = self.recording.as_mut() {
                rec.extend_from_slice(&chunk);
                if rec.len() >= need {
                    let mut done = std::mem::take(rec);
                    done.truncate(need);
                    finished = Some(done);
                }
            }
            if let Some(samples) = finished {
                self.recording = None;
                let path = default_wav_path();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match write_wav(&path, &samples, SAMPLE_RATE) {
                    Ok(()) => {
                        self.last_note =
                            Some(format!("Записано {RECORD_SECS:.0} с: {}", path.display()))
                    }
                    Err(e) => self.last_error = Some(format!("не удалось записать WAV: {e}")),
                }
            }
        }
    }

    /// Начинает тестовую запись: накопленное до нажатия в файл не попадает.
    pub fn start_recording(&mut self) {
        if self.capture.is_none() {
            self.last_error = Some("сначала включите захват звука".to_string());
            return;
        }
        if let Some(c) = &self.capture {
            let _ = c.drain();
        }
        self.recording = Some(Vec::with_capacity(
            (RECORD_SECS * SAMPLE_RATE as f32) as usize,
        ));
        self.last_note = None;
    }

    /// Сколько секунд уже записано (`None` — запись не идёт).
    pub fn recording_secs(&self) -> Option<f32> {
        self.recording
            .as_ref()
            .map(|r| r.len() as f32 / SAMPLE_RATE as f32)
    }

    /// Уровень звука (RMS) для индикатора.
    pub fn level(&self) -> f32 {
        self.capture.as_ref().map(|c| c.level()).unwrap_or(0.0)
    }

    /// Последние `secs` секунд захваченного звука (16 кГц моно).
    pub fn last_samples(&self, secs: f32) -> Vec<f32> {
        let n = (secs.max(0.0) * SAMPLE_RATE as f32) as usize;
        self.capture
            .as_ref()
            .map(|c| c.last(n))
            .unwrap_or_default()
    }

    pub fn is_running(&self) -> bool {
        self.capture.as_ref().map(|c| c.is_running()).unwrap_or(false)
    }

    /// Строка «48.0 кГц, 2 кан → 16 кГц моно · 12.3 с · вытеснено 0».
    pub fn stats_line(&self) -> String {
        let Some(cap) = &self.capture else {
            return "захват выключен".to_string();
        };
        let (fmt, secs, dropped) = cap.stats();
        let src = match fmt {
            Some(f) => format!("{:.1} кГц, {} кан", f.rate as f32 / 1000.0, f.channels),
            None => "—".to_string(),
        };
        format!("{src} → 16 кГц моно · {secs:.1} с · вытеснено {dropped}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_converts_48k_to_16k() {
        let mut rs = Resampler::new(48_000);
        let input: Vec<f32> = (0..480).map(|i| i as f32 / 480.0).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert!(
            (out.len() as i32 - 160).abs() <= 2,
            "получили {} сэмплов вместо ~160",
            out.len()
        );
    }

    #[test]
    fn resampler_keeps_16k_as_is() {
        let mut rs = Resampler::new(SAMPLE_RATE);
        let input = vec![0.5f32; 320];
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        assert_eq!(out.len(), 320);
        assert!(out.iter().all(|s| (*s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn resampler_stays_continuous_between_blocks() {
        let mut rs = Resampler::new(48_000);
        let mut out1 = Vec::new();
        let mut out2 = Vec::new();
        rs.process(&[0.2f32; 48], &mut out1);
        rs.process(&[0.2f32; 48], &mut out2);
        let total = out1.len() + out2.len();
        assert!((total as i32 - 32).abs() <= 3, "всего {total} сэмплов");
        assert!(out1
            .iter()
            .chain(out2.iter())
            .all(|s| (*s - 0.2).abs() < 1e-5));
    }

    #[test]
    fn ring_evicts_oldest_samples() {
        let mut ring = Ring::new(4);
        ring.push(&[1.0, 2.0, 3.0]);
        ring.push(&[4.0, 5.0, 6.0]);
        assert_eq!(ring.dropped, 2);
        assert_eq!(ring.drain_all(), vec![3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn wav_has_riff_header_and_pcm_data() {
        let mut path = std::env::temp_dir();
        path.push("twitch_mini_player_test.wav");
        let samples = [0.0f32, 0.5, -0.5];
        write_wav(&path, &samples, SAMPLE_RATE).expect("WAV пишется");
        let bytes = std::fs::read(&path).expect("WAV читается");
        let _ = std::fs::remove_file(&path);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(&bytes[36..40], b"data");
        let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        assert_eq!(rate, SAMPLE_RATE);
        assert_eq!(bytes.len(), 44 + samples.len() * 2);
    }

    #[test]
    fn rms_is_one_for_full_scale_and_zero_for_silence() {
        assert_eq!(rms_of(&[0.0; 128]), 0.0);
        assert!((rms_of(&[1.0; 128]) - 1.0).abs() < 1e-6);
    }

    /// Проверка настоящего захвата (нужны звук и Windows 11 build 20348+).
    /// Запускается вручную:
    /// `cargo test --release capture_activation -- --ignored --nocapture`
    #[test]
    #[ignore = "требует звуковое устройство и Windows 11"]
    fn capture_activation_works() {
        use windows_sys::Win32::Media::Audio::{
            PlaySoundW, SND_ASYNC, SND_LOOP, SND_MEMORY, SND_NODEFAULT,
        };
        // Без звука у процесса нет аудиосессии — gромкий тон гарантирует,
        // что захватывать есть что (и что уровень действительно виден).
        let tone: Vec<f32> = (0..SAMPLE_RATE as usize)
            .map(|i| {
                (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.25
            })
            .collect();
        let wav = wav_bytes(&tone, SAMPLE_RATE);
        unsafe {
            PlaySoundW(
                wav.as_ptr() as *const u16,
                0,
                SND_MEMORY | SND_ASYNC | SND_LOOP | SND_NODEFAULT,
            );
        }
        std::thread::sleep(Duration::from_millis(300));

        let mut cap = match Capture::start() {
            Ok(c) => c,
            Err(e) => {
                // Диагностика в UTF-8: консоль PowerShell портит кириллицу.
                let mut p = std::env::temp_dir();
                p.push("twitch_capture_error.txt");
                let _ = std::fs::write(p, &e);
                unsafe { PlaySoundW(std::ptr::null(), 0, 0) };
                panic!("захват не запустился: {e}");
            }
        };
        std::thread::sleep(Duration::from_millis(1200));
        let (fmt, secs, dropped) = cap.stats();
        let level = cap.level();
        let samples = cap.drain();
        cap.stop();
        unsafe { PlaySoundW(std::ptr::null(), 0, 0) };

        let report = format!(
            "формат: {fmt:?}, секунд: {secs:.2}, вытеснено: {dropped}, \
             сэмплов: {}, уровень: {level:.4}",
            samples.len()
        );
        let mut p = std::env::temp_dir();
        p.push("twitch_capture_probe.txt");
        let _ = std::fs::write(p, &report);

        assert!(fmt.is_some(), "формат устройства получен");
        assert!(secs > 0.5, "звук приходит: {secs:.2} с");
        assert!(level > 0.01, "тон слышно в захвате: {level:.4}");
    }
}
