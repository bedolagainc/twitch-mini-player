//! Живые субтитры: звук стрима → whisper.cpp → opus-mt-en-ru → строка на экране.
//!
//! Поток живёт отдельно от интерфейса: забирает звук из общего кольца захвата
//! (`subs::Shared`), режет его на фразы по паузам, распознаёт и переводит.
//! UI только читает готовые строки, поэтому ни распознавание, ни перевод не
//! подвешивают окно и не влияют на плавность видео.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::asr;
use crate::mt;
use crate::subs::{Shared, SAMPLE_RATE};

/// Кадр анализа — 30 мс (как буфер захвата).
const FRAME: usize = (SAMPLE_RATE as usize * 30) / 1000;
/// Сколько звука перед началом речи сохраняем: иначе срезается первый слог.
const PREROLL: usize = (SAMPLE_RATE as usize * 250) / 1000;
/// Тишина, после которой фраза считается законченной.
///
/// Было 320 мс: пауза в 0.3 с — обычное дело внутри предложения, а каждая
/// лишняя сотая секунды здесь — это задержка перед появлением строки.
const SILENCE_CLOSE: usize = (SAMPLE_RATE as usize * 200) / 1000;
/// Минимальная длина речи: короче — щелчок, кашель или обрывок.
const MIN_VOICED: usize = (SAMPLE_RATE as usize * 140) / 1000;
/// Максимальная длина фразы: дольше whisper начинает «додумывать», а строка
/// опаздывает. Шесть секунд — примерно два предложения.
const MAX_PHRASE: usize = SAMPLE_RATE as usize * 6;
/// Пик громкости во фразе ниже этого — музыка или шум, а не речь.
/// Порог низкий намеренно: плеер слушают и на 20 % громкости, а перед
/// распознаванием тихая фраза поднимается до комфортного уровня.
/// Захват процесса отдаёт звук уже после громкости mpv: на 30 % речь
/// приходит с пиком 0.005–0.01, поэтому порог ниже 0.002 отсекал бы её.
const MIN_PEAK: f32 = 0.002;
/// До какого пика поднимаем тихую фразу перед whisper (он точнее на громком).
const TARGET_PEAK: f32 = 0.7;
/// Максимальное усиление тихой фразы: громче — уже шум вместе с речью.
/// 48× поднимает очень тихий захват (пик 0.015, ~-36 dBFS) до слышимого
/// уровня, при этом ограничение не даёт шуму забить речь.
const MAX_GAIN: f32 = 48.0;
/// Сколько тишины оставляем в конце фразы (для контекста распознавания).
const TAIL_KEEP: usize = (SAMPLE_RATE as usize * 120) / 1000;
/// Сколько секунд звука забираем за раз: если поток отстал, старое отбрасываем.
const MAX_DRAIN_SECS: f32 = 4.0;
/// Пауза, когда новых сэмплов нет.
const IDLE: Duration = Duration::from_millis(40);
/// Сколько последняя строка висит на экране после появления.
const HOLD: Duration = Duration::from_secs(7);
/// Промежуточная строка: как часто распознаём речь, которая ещё не закончилась.
/// Это главный способ убрать ощущение задержки — текст появляется через
/// ~0.4 с после начала фразы, а не после её конца.
const PARTIAL_EVERY: Duration = Duration::from_millis(500);
/// Сколько звука нужно, чтобы показать промежуточную строку.
const PARTIAL_MIN: usize = (SAMPLE_RATE as usize * 400) / 1000;
/// Промежуточная строка распознаётся по последним секундам фразы: длиннее —
/// только дороже, а хвост всё равно перепишет финальное распознавание.
const PARTIAL_MAX_SECS: f32 = 5.0;
/// Сколько висит промежуточная строка, если финальной так и не пришло
/// (например, звук оборвался на полуслове): дальше она уже врёт. Фразу,
/// отброшенную фильтром, гасит `DropPartial` — сразу, без ожидания.
const PARTIAL_HOLD: Duration = Duration::from_millis(2500);
/// Одинаковые подряд строки — это музыкальный луп или «эхо» whisper.
const REPEAT_WINDOW: Duration = Duration::from_secs(12);
/// Насколько модель должна быть уверена, что это не речь, чтобы фразу
/// выбросить. Музыка и звуки игры дают 0.8–0.99, обычная речь — 0.0–0.2.
const NO_SPEECH_MAX: f32 = 0.6;
/// Жёсткий порог: выше него фраза выбрасывается при любой длине.
const NO_SPEECH_HARD: f32 = 0.85;
/// Короткая фраза с таким «не речь» — почти наверняка музыка.
const NO_SPEECH_SHORT_WORDS: usize = 4;
/// Предел плотности речи: люди говорят не быстрее ~3 слов в секунду. Всё,
/// что заметно плотнее, — зацикливание whisper на шуме («no no no no …»),
/// даже если по времени распознавание успело уложиться в лимит.
const MAX_WORDS_PER_SEC: f32 = 3.0;
/// Запас слов на короткие фразы: у секунды звука шкала ещё грубая.
const MAX_WORDS_SLACK: usize = 4;
/// Самое длинное слово, которое может быть настоящим. Длиннее — это
/// зацикливание внутри слова, которого фильтр по словам не видит:
/// «We're gonna make a meet-a-doo-doo-doo-…» на две сотни символов.
const MAX_WORD_CHARS: usize = 32;

/// Событие из потока субтитров.
enum Event {
    /// Что происходит сейчас (загрузка модели, ожидание звука).
    Status(String),
    /// Модель распознавания загружена: имя файла.
    Model(String),
    /// Движки готовы, звук слушается.
    Ready,
    /// Готовая фраза: английский текст, перевод (если включён) и время обработки.
    Line {
        en: String,
        ru: Option<String>,
        latency_ms: u32,
    },
    /// Промежуточная строка: речь ещё продолжается, финальной пока нет.
    Partial { en: String, ru: Option<String> },
    /// Не ошибка, но и не то, что ждали: перевод недоступен и т. п.
    Note(String),
    /// Состояние перевода («загружен», «выключен», «ошибка…»).
    Mt(String),
    /// Фраза отброшена: модель решила, что это не речь (музыка, звуки игры).
    NotSpeech(u32),
    /// Фраза обработана, но распознать в ней текст не удалось.
    Unheard(u32),
    /// Фраза повторяет предыдущую (музыкальный луп, «эхо» whisper).
    Repeated(u32),
    /// Фраза закончилась, но показывать из неё нечего: промежуточный текст
    /// от неё устарел и должен исчезнуть сразу, а не висеть до `PARTIAL_HOLD`.
    DropPartial,
    Error(String),
}

/// Строка субтитров, готовая к показу.
pub struct Line {
    pub en: String,
    pub ru: Option<String>,
    pub at: Instant,
}

impl Line {
    /// Что показывать: перевод, если он есть, иначе оригинал.
    pub fn text(&self) -> &str {
        match &self.ru {
            Some(ru) if !ru.trim().is_empty() => ru,
            _ => &self.en,
        }
    }
}

/// Что нарисовать поверх кадра.
pub struct Overlay {
    /// Текст строки: перевод, иначе оригинал.
    pub text: String,
    /// Строка ещё распознаётся: показать её бледнее, без «прыжка» вёрстки.
    pub partial: bool,
}

/// Живые субтитры: поток обработки и последние строки для оверлея.
pub struct Live {
    stop: Arc<AtomicBool>,
    translate: Arc<AtomicBool>,
    /// Поколение настройки перевода: по нему поток узнаёт, что пользователь
    /// включил перевод заново, и пробует загрузить модель ещё раз.
    mt_gen: Arc<std::sync::atomic::AtomicU32>,
    thread: Option<JoinHandle<()>>,
    rx: Receiver<Event>,
    lines: VecDeque<Line>,
    /// Текущий шаг конвейера («загружаю перевод…», «слушаю звук стрима»).
    pub status: String,
    /// Движки загружены — можно ждать текст.
    pub ready: bool,
    pub error: Option<String>,
    /// Сколько фраз уже распознано.
    pub count: u32,
    /// Сколько фраз отброшено как не-речь (музыка, звуки игры).
    pub skipped: u32,
    /// Сколько фраз обработано, но без текста (тихо, обрывок, шум).
    pub unheard: u32,
    /// Сколько фраз отброшено как повтор предыдущей.
    pub repeated: u32,
    /// Промежуточная строка: текст, который ещё распознаётся, и её время.
    partial: Option<(String, Instant)>,
    /// Время последней фразы: распознавание + перевод.
    pub last_ms: Option<u32>,
    /// Имя файла модели распознавания.
    pub model: String,
    /// Состояние перевода для консоли.
    pub mt: String,
}

impl Live {
    /// Запускает поток: модели грузятся в нём же, UI не ждёт.
    pub fn start(shared: Arc<Shared>, translate: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::new(AtomicBool::new(translate));
        let gen = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let (tx, rx) = mpsc::channel();
        let st = Arc::clone(&stop);
        let fl = Arc::clone(&flag);
        let gn = Arc::clone(&gen);
        let mut error = None;
        // Копия отправителя для сообщения о панике: поток субтитров живёт
        // отдельно, и его падение не должно закрывать плеер молча.
        let tx_crash = tx.clone();
        let thread = match std::thread::Builder::new()
            .name("live-subs".to_string())
            .spawn(move || {
                let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(shared, st, fl, gn, tx)
                }))
                .is_err();
                if crashed {
                    crate::debug_log("live: поток субтитров упал (panic)");
                    let _ = tx_crash.send(Event::Error(
                        "субтитры: внутренняя ошибка — включите CC заново".to_string(),
                    ));
                }
            })
        {
            Ok(t) => Some(t),
            Err(e) => {
                error = Some(format!("не удалось создать поток субтитров: {e}"));
                None
            }
        };
        Self {
            stop,
            translate: flag,
            mt_gen: gen,
            thread,
            rx,
            lines: VecDeque::new(),
            status: "запускаю…".to_string(),
            ready: false,
            error,
            count: 0,
            skipped: 0,
            unheard: 0,
            repeated: 0,
            partial: None,
            last_ms: None,
            model: String::new(),
            mt: if translate {
                "перевод: загружаю…".to_string()
            } else {
                "перевод: выключен".to_string()
            },
        }
    }

    /// Включает/выключает перевод: модель догрузится на лету.
    ///
    /// Повторное включение (после ошибки загрузки) сбрасывает её и пробует
    /// открыть модель заново — иначе неудачный первый запуск блокировал
    /// перевод до перезапуска плеера.
    pub fn set_translate(&self, on: bool) {
        if self.translate.swap(on, Ordering::Relaxed) != on {
            self.mt_gen.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Забирает события потока. `true` — что-то изменилось, нужна перерисовка.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(Event::Status(s)) => {
                    self.status = s;
                    changed = true;
                }
                Ok(Event::Model(m)) => {
                    self.model = m;
                    changed = true;
                }
                Ok(Event::Ready) => {
                    self.ready = true;
                    self.status = "слушаю звук стрима".to_string();
                    changed = true;
                }
                Ok(Event::Line { en, ru, latency_ms }) => {
                    self.count = self.count.saturating_add(1);
                    self.last_ms = Some(latency_ms);
                    // Финальная строка заменила промежуточную.
                    self.partial = None;
                    self.lines.push_back(Line {
                        en,
                        ru,
                        at: Instant::now(),
                    });
                    while self.lines.len() > 3 {
                        self.lines.pop_front();
                    }
                    changed = true;
                }
                Ok(Event::Partial { en, ru }) => {
                    let text = match ru {
                        Some(ru) if !ru.trim().is_empty() => ru,
                        _ => en,
                    };
                    if !text.trim().is_empty() {
                        self.partial = Some((text, Instant::now()));
                        changed = true;
                    }
                }
                Ok(Event::Note(s)) => {
                    self.status = s;
                    changed = true;
                }
                Ok(Event::Mt(s)) => {
                    self.mt = s;
                    changed = true;
                }
                Ok(Event::NotSpeech(n)) => {
                    self.skipped = n;
                    changed = true;
                }
                Ok(Event::Unheard(n)) => {
                    self.unheard = n;
                    changed = true;
                }
                Ok(Event::Repeated(n)) => {
                    self.repeated = n;
                    changed = true;
                }
                Ok(Event::DropPartial) => {
                    if self.partial.take().is_some() {
                        changed = true;
                    }
                }
                Ok(Event::Error(e)) => {
                    self.error = Some(e);
                    self.ready = false;
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.error.is_none() && self.thread.is_some() {
                        self.error = Some("поток субтитров завершился".to_string());
                        self.ready = false;
                        changed = true;
                    }
                    break;
                }
            }
        }
        changed
    }

    /// Что показать поверх видео (`None` — показывать нечего).
    ///
    /// Одна строка, без «предыдущей»: две строки подряд читаются как два
    /// разных говорящих, а в стриме говорит один человек. Пока фраза ещё
    /// распознаётся, показывается промежуточный текст (бледнее).
    pub fn overlay(&self) -> Option<Overlay> {
        let fresh_partial = self
            .partial
            .as_ref()
            .filter(|(_, at)| at.elapsed() <= PARTIAL_HOLD);
        if let Some((text, at)) = fresh_partial {
            let newer = self.lines.back().map(|l| l.at < *at).unwrap_or(true);
            if newer && !text.trim().is_empty() {
                return Some(Overlay {
                    text: text.clone(),
                    partial: true,
                });
            }
        }
        let cur = self.lines.back()?;
        if cur.at.elapsed() > HOLD {
            return None;
        }
        let text = cur.text().to_string();
        if text.trim().is_empty() {
            return None;
        }
        Some(Overlay {
            text,
            partial: false,
        })
    }

    /// Последние строки для консоли: «EN → RU».
    pub fn recent(&self, n: usize) -> Vec<String> {
        self.lines
            .iter()
            .rev()
            .take(n)
            .map(|l| match &l.ru {
                Some(ru) if !ru.trim().is_empty() => format!("{} → {}", l.en, ru),
                _ => l.en.clone(),
            })
            .collect()
    }

    /// Строка состояния для консоли разработчика.
    pub fn status_line(&self) -> String {
        let mut s = format!("{} · фраз: {}", self.status, self.count);
        if let Some(ms) = self.last_ms {
            s.push_str(&format!(" · последняя {ms} мс"));
        }
        if !self.model.is_empty() {
            s.push_str(&format!(" · модель {}", self.model));
        }
        s
    }

    /// Состояние перевода и фильтра речи одной строкой (для консоли).
    pub fn details_line(&self) -> String {
        let mut parts = vec![self.mt.clone()];
        if self.skipped > 0 {
            parts.push(format!("отсеяно не-речи: {}", self.skipped));
        }
        if self.unheard > 0 {
            parts.push(format!("без текста: {}", self.unheard));
        }
        if self.repeated > 0 {
            parts.push(format!("повторов: {}", self.repeated));
        }
        parts.join(" · ")
    }

    /// Останавливает поток и освобождает модели.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Выключение CC не должно ждать, пока whisper додекодирует фразу:
        // поток останавливается, только когда вернётся из `whisper_full`.
        crate::asr::abort_now();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Вся работа потока: загрузка движков → нарезка фраз → распознавание → перевод.
fn run(
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    translate: Arc<AtomicBool>,
    mt_gen: Arc<std::sync::atomic::AtomicU32>,
    tx: mpsc::Sender<Event>,
) {
    let _ = tx.send(Event::Status("загружаю распознавание речи…".to_string()));
    let asr = match asr::Asr::new() {
        Ok(a) => a,
        Err(e) => {
            let _ = tx.send(Event::Error(e));
            return;
        }
    };
    let model = asr
        .model_path()
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let _ = tx.send(Event::Model(model));

    // Прогрев: первый вызов whisper ощутимо дольше остальных (аллокации,
    // заполнение кэша), и без него самая первая фраза «залипала».
    let warmup = vec![0.0f32; SAMPLE_RATE as usize / 2];
    let _ = asr.text_streaming(&warmup);

    let mut translator: Option<mt::Translator> = None;
    let mut mt_error: Option<String> = None;
    // Поколение настройки перевода: смена значения = пользователь переключил
    // галочку, и после ошибки загрузку надо повторить.
    let mut mt_loaded_gen = mt_gen.load(Ordering::Relaxed);

    let load_translator = |translator: &mut Option<mt::Translator>,
                           mt_error: &mut Option<String>,
                           tx: &mpsc::Sender<Event>| {
        let _ = tx.send(Event::Mt("перевод: загружаю…".to_string()));
        let started = Instant::now();
        match mt::Translator::new() {
            Ok(t) => {
                let name = t
                    .model_dir()
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "opus-mt-en-ru".to_string());
                // Каталог по умолчанию называется просто `mt` — в консоли
                // полезнее имя самой модели.
                let name = if name == "mt" {
                    "opus-mt-en-ru".to_string()
                } else {
                    name
                };
                let _ = tx.send(Event::Mt(format!("перевод: готов ({name})")));
                crate::debug_log(&format!(
                    "live: перевод загружен за {} мс",
                    started.elapsed().as_millis()
                ));
                *translator = Some(t);
                *mt_error = None;
            }
            Err(e) => {
                crate::debug_log(&format!("live: перевод не загрузился: {e}"));
                let _ = tx.send(Event::Mt(format!("перевод: ошибка — {e}")));
                *translator = None;
                *mt_error = Some(e);
            }
        }
    };

    if translate.load(Ordering::Relaxed) {
        let _ = tx.send(Event::Status("загружаю перевод…".to_string()));
        load_translator(&mut translator, &mut mt_error, &tx);
    } else {
        let _ = tx.send(Event::Mt("перевод: выключен".to_string()));
    }
    let _ = tx.send(Event::Ready);
    crate::debug_log("live: движки загружены, слушаю звук");

    let mut seg = Segmenter::new();
    let mut not_speech = 0u32;
    let mut unheard = 0u32;
    let mut repeated = 0u32;
    // Предыдущая показанная строка: по ней отсеиваются повторы (музыкальный
    // луп, «эхо» whisper на ритмичной музыке).
    let mut last_emit: Option<(String, Instant)> = None;
    // Промежуточное распознавание: когда последний раз и что показали.
    let mut last_partial = Instant::now();
    let mut last_partial_text = String::new();
    while !stop.load(Ordering::SeqCst) {
        // Настройка перевода могла измениться, пока шло распознавание.
        let want_ru = translate.load(Ordering::Relaxed);
        let gen_now = mt_gen.load(Ordering::Relaxed);
        if gen_now != mt_loaded_gen {
            mt_loaded_gen = gen_now;
            if want_ru {
                load_translator(&mut translator, &mut mt_error, &tx);
            } else {
                // Держать 100 МБ моделей на паузе незачем — освобождаем,
                // при повторном включении загрузка займёт меньше секунды.
                translator = None;
                mt_error = None;
                let _ = tx.send(Event::Mt("перевод: выключен".to_string()));
            }
        }
        if want_ru && translator.is_none() && mt_error.is_none() {
            load_translator(&mut translator, &mut mt_error, &tx);
        }

        let pcm = shared.drain_live(MAX_DRAIN_SECS);
        if pcm.is_empty() {
            std::thread::sleep(IDLE);
            continue;
        }
        for phrase in seg.push(&pcm) {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let started = Instant::now();
            let secs = phrase.samples.len() as f32 / SAMPLE_RATE as f32;
            if crate::debug_enabled() {
                crate::debug_log(&format!("live: фраза {secs:.2} с, пик {:.3}", phrase.peak));
            }
            let audio = normalize(&phrase.samples, phrase.peak);
            let (raw, no_speech) = match asr.text_streaming_checked(&audio) {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(Event::Error(e));
                    continue;
                }
            };
            let en = clean(&raw);
            let en = collapse_repeats(&en);
            let words = en.split_whitespace().count();
            let verdict = judge(&en, no_speech, secs, last_emit.as_ref());
            if !matches!(verdict, Verdict::Show) {
                // Фраза закончилась, а показывать из неё нечего: её
                // промежуточный текст уже устарел.
                let _ = tx.send(Event::DropPartial);
            }
            match verdict {
                Verdict::Show => {}
                Verdict::Empty => {
                    unheard += 1;
                    let _ = tx.send(Event::Unheard(unheard));
                    if crate::debug_enabled() {
                        crate::debug_log(&format!(
                            "live: текста нет (no_speech {no_speech:.2}): {}",
                            preview(&en, 120)
                        ));
                    }
                    continue;
                }
                Verdict::NotSpeech => {
                    not_speech += 1;
                    let _ = tx.send(Event::NotSpeech(not_speech));
                    if crate::debug_enabled() {
                        crate::debug_log(&format!(
                            "live: не речь (no_speech {no_speech:.2}), пропускаю: {}",
                            preview(&en, 120)
                        ));
                    }
                    continue;
                }
                Verdict::Repeat => {
                    repeated += 1;
                    let _ = tx.send(Event::Repeated(repeated));
                    if crate::debug_enabled() {
                        crate::debug_log(&format!("live: повтор, пропускаю: {}", preview(&en, 120)));
                    }
                    continue;
                }
                Verdict::Garbled => {
                    not_speech += 1;
                    let _ = tx.send(Event::NotSpeech(not_speech));
                    if crate::debug_enabled() {
                        crate::debug_log(&format!(
                            "live: зацикливание ({words} слов на {secs:.1} с), пропускаю: {}",
                            preview(&en, 120)
                        ));
                    }
                    continue;
                }
            }

            // Время распознавания отдельно от перевода: по этой паре видно,
            // где именно ушла задержка — модель или переводчик.
            let rec_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let ru = translate_line(&mut translator, want_ru, &en, &tx);
            let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
            if crate::debug_enabled() {
                let ru_log = ru
                    .as_deref()
                    .map(|r| preview(r, 160))
                    .unwrap_or_else(|| "—".to_string());
                crate::debug_log(&format!(
                    "live: {latency_ms} мс (распознавание {rec_ms} мс), {secs:.1} с звука: {} => {ru_log}",
                    preview(&en, 160)
                ));
            }
            last_emit = Some((normalize_text(&en), Instant::now()));
            last_partial_text.clear();
            let _ = tx.send(Event::Line {
                en,
                ru,
                latency_ms,
            });
        }

        // Фраза ещё не закончилась, а текст уже нужен: распознаём накопленное,
        // чтобы строка появлялась во время речи, а не после паузы.
        if seg.speech && last_partial.elapsed() >= PARTIAL_EVERY && !stop.load(Ordering::SeqCst) {
            last_partial = Instant::now();
            if let Some(p) = seg.snapshot(PARTIAL_MAX_SECS) {
                let audio = normalize(&p.samples, p.peak);
                if let Ok((raw, no_speech)) = asr.text_streaming_checked(&audio) {
                    let en = collapse_repeats(&clean(&raw));
                    let words = en.split_whitespace().count();
                    let psecs = p.samples.len() as f32 / SAMPLE_RATE as f32;
                    // Промежуточную строку показываем мягче финальной: она
                    // может оборваться на середине слова, зато не опаздывает.
                    let usable = !en.is_empty()
                        && !is_noise(&en)
                        && no_speech <= NO_SPEECH_HARD
                        && !(no_speech > NO_SPEECH_MAX && words < 2)
                        && !garbled(&en, words, psecs);
                    if usable && en != last_partial_text {
                        last_partial_text = en.clone();
                        let ru = translate_line(&mut translator, want_ru, &en, &tx);
                        if crate::debug_enabled() {
                            let ru_log = ru
                                .as_deref()
                                .map(|r| preview(r, 160))
                                .unwrap_or_else(|| "—".to_string());
                            crate::debug_log(&format!(
                                "live: промежуточная, {psecs:.1} с звука: {} => {ru_log}",
                                preview(&en, 160)
                            ));
                        }
                        let _ = tx.send(Event::Partial { en, ru });
                    }
                }
            }
        }
    }
    crate::debug_log("live: поток субтитров остановлен");
}

/// Готовая фраза: звук для распознавания и её громкость.
struct Phrase {
    samples: Vec<f32>,
    peak: f32,
}

/// Решение по распознанной фразе.
enum Verdict {
    /// Показать зрителю.
    Show,
    /// Текста нет: тишина, обрывок, шум.
    Empty,
    /// Это не речь: музыка, звуки игры.
    NotSpeech,
    /// Тот же текст, что и предыдущая строка.
    Repeat,
    /// Модель зациклилась: слов в разы больше, чем помещается в звук.
    Garbled,
}

/// Зацикливание whisper: в секунду «речи» влезает столько слов, сколько
/// человек физически не скажет.
fn too_dense(words: usize, secs: f32) -> bool {
    words > (secs.max(0.0) * MAX_WORDS_PER_SEC) as usize + MAX_WORDS_SLACK
}

/// Признаки зацикливания модели: нечеловечески плотная речь или слово,
/// которого не бывает (`doo-doo-doo-…` на две сотни символов).
fn garbled(en: &str, words: usize, secs: f32) -> bool {
    too_dense(words, secs) || en.split_whitespace().any(|w| w.chars().count() > MAX_WORD_CHARS)
}

/// Короткий вид строки для лога: зацикливание бывает на сотни символов,
/// и такой текст бесполезно писать в отладочный лог целиком.
fn preview(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Отсеивает музыку, шум и повторы — то, из-за чего субтитры «путаются».
///
/// Порог «не речь» двухуровневый: выше 0.85 модель уверена почти полностью,
/// а 0.6–0.85 бывает и у настоящей речи поверх музыки — такие фразы
/// выбрасываются только короткие, где вокал легко спутать с текстом.
/// Отдельно отсеиваются «одно слово на шесть секунд звука» (обрывок
/// распознавания на музыке, а не реплика) и «слишком много слов на секунду
/// звука» — зацикливание модели («no no no no …»), которое иначе уходит
/// в перевод и висит на экране.
fn judge(en: &str, no_speech: f32, secs: f32, prev: Option<&(String, Instant)>) -> Verdict {
    if en.is_empty() || is_noise(en) {
        return Verdict::Empty;
    }
    let words = en.split_whitespace().count();
    if no_speech > NO_SPEECH_HARD || (no_speech > NO_SPEECH_MAX && words <= NO_SPEECH_SHORT_WORDS)
    {
        return Verdict::NotSpeech;
    }
    if words == 1 && secs > 2.5 {
        return Verdict::Empty;
    }
    if garbled(en, words, secs) {
        return Verdict::Garbled;
    }
    if let Some((last, at)) = prev {
        if at.elapsed() < REPEAT_WINDOW && normalize_text(en) == *last {
            return Verdict::Repeat;
        }
    }
    Verdict::Show
}

/// Переводит фразу, если перевод включён и модель загружена.
fn translate_line(
    translator: &mut Option<mt::Translator>,
    want_ru: bool,
    en: &str,
    tx: &mpsc::Sender<Event>,
) -> Option<String> {
    if !want_ru {
        return None;
    }
    let t = translator.as_mut()?;
    match t.translate(en) {
        Ok(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        Err(e) => {
            let _ = tx.send(Event::Note(format!("перевод: {e}")));
            None
        }
    }
}

/// RMS блока.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

/// Дописывает `src` в `dst`, оставляя не больше `cap` сэмплов.
fn push_limited(dst: &mut Vec<f32>, src: &[f32], cap: usize) {
    dst.extend_from_slice(src);
    if dst.len() > cap {
        let extra = dst.len() - cap;
        dst.drain(..extra);
    }
}

/// Поднимает тихую фразу до комфортной громкости.
///
/// Стрим слушают по-разному, а процессный захват отдаёт звук уже с учётом
/// громкости плеера: на 20 % речь приходит с пиком 0.02–0.05, и whisper на
/// такой громкости заметно ошибается. Усиление ограничено, чтобы не вытащить
/// на первый план шум.
fn normalize(samples: &[f32], peak: f32) -> Vec<f32> {
    // Уже достаточно громко — не трогаем: усиление только добавит клиппинг.
    if peak >= TARGET_PEAK * 0.7 {
        return samples.to_vec();
    }
    let gain = (TARGET_PEAK / peak.max(1e-6)).clamp(1.0, MAX_GAIN);
    if gain <= 1.01 {
        return samples.to_vec();
    }
    samples
        .iter()
        .map(|s| (s * gain).clamp(-1.0, 1.0))
        .collect()
}

/// Режет поток звука на фразы по паузам.
///
/// Порог тишины подстраивается под текущий шум (RMS): в тихом стриме он ниже,
/// в шумном — выше, поэтому речь не теряется и тишина не уходит в whisper.
struct Segmenter {
    /// Накопленный звук текущей фразы.
    cur: Vec<f32>,
    /// Звук до начала речи: начало слова не срезается.
    pre: Vec<f32>,
    /// Неполный кадр анализа — переносится в следующий вызов.
    rest: Vec<f32>,
    speech: bool,
    silence: usize,
    voiced: usize,
    peak: f32,
    /// Оценка уровня шума (RMS).
    noise: f32,
}

impl Segmenter {
    fn new() -> Self {
        Self {
            cur: Vec::new(),
            pre: Vec::new(),
            rest: Vec::new(),
            speech: false,
            silence: 0,
            voiced: 0,
            peak: 0.0,
            // Стартовое значение — типичный шум записи: пока он не подстроится,
            // первые полсекунды не должны уходить в «речь» на пустом месте.
            noise: 0.001,
        }
    }
    /// Добавляет звук, возвращая законченные фразы (может быть несколько).
    fn push(&mut self, pcm: &[f32]) -> Vec<Phrase> {
        let mut out = Vec::new();
        if pcm.is_empty() {
            return out;
        }
        let mut joined: Vec<f32>;
        let data: &[f32] = if self.rest.is_empty() {
            pcm
        } else {
            joined = std::mem::take(&mut self.rest);
            joined.extend_from_slice(pcm);
            &joined
        };
        let mut i = 0;
        while i + FRAME <= data.len() {
            let frame = &data[i..i + FRAME];
            i += FRAME;
            let level = rms(frame);
            let threshold = (self.noise * 2.8).clamp(0.0006, 0.03);
            let voiced = level > threshold;
            if !self.speech {
                push_limited(&mut self.pre, frame, PREROLL);
                if !voiced {
                    // Пока речи нет, оценка шума подтягивается к текущему уровню.
                    self.noise = self.noise * 0.95 + level * 0.05;
                    continue;
                }
                self.speech = true;
                self.cur.clear();
                self.cur.extend_from_slice(&self.pre);
                self.pre.clear();
                self.silence = 0;
                self.voiced = FRAME;
                self.peak = level;
                self.cur.extend_from_slice(frame);
                continue;
            }
            self.cur.extend_from_slice(frame);
            if voiced {
                self.voiced += FRAME;
                self.silence = 0;
                self.peak = self.peak.max(level);
            } else {
                self.silence += FRAME;
                self.noise = self.noise * 0.98 + level * 0.02;
            }
            if self.silence >= SILENCE_CLOSE || self.cur.len() >= MAX_PHRASE {
                if let Some(p) = self.close() {
                    out.push(p);
                }
            }
        }
        self.rest = data[i..].to_vec();
        out
    }

    /// Снимок незаконченной фразы для промежуточного распознавания.
    ///
    /// Берём последние `max_secs` секунд: начало длинной фразы для показа
    /// уже не нужно, а распознавание хвоста дешевле.
    fn snapshot(&self, max_secs: f32) -> Option<Phrase> {
        if !self.speech || self.cur.len() < PARTIAL_MIN {
            return None;
        }
        let max = (max_secs * SAMPLE_RATE as f32) as usize;
        let from = self.cur.len().saturating_sub(max);
        let samples = self.cur[from..].to_vec();
        let peak = samples.iter().fold(0f32, |a, s| a.max(s.abs()));
        Some(Phrase { samples, peak })
    }

    /// Заканчивает фразу: обрезает лишнюю тишину и отбрасывает обрывки.
    fn close(&mut self) -> Option<Phrase> {
        let trim = self.silence.saturating_sub(TAIL_KEEP);
        let keep = self.cur.len().saturating_sub(trim);
        let samples: Vec<f32> = self.cur[..keep].to_vec();
        let peak = self.peak;
        let voiced = self.voiced;
        self.speech = false;
        self.cur.clear();
        self.pre.clear();
        self.silence = 0;
        self.voiced = 0;
        self.peak = 0.0;
        if voiced < MIN_VOICED || peak < MIN_PEAK || samples.is_empty() {
            return None;
        }
        Some(Phrase { samples, peak })
    }
}

/// Схлопывает зацикливание whisper: «I am testing the I am testing the …» → одна фраза.
///
/// Модель иногда «застревает» на повторе (музыка, обрыв фразы) и выдаёт один
/// и тот же кусок десятки раз. Такой текст бесполезен и, что хуже, заставляет
/// переводчик работать секундами. Работаем по словам, в два прохода:
/// 1) периодический хвост из трёх и более одинаковых кусков по 1–6 слов;
/// 2) два подряд одинаковых куска по 2–20 слов (музыкальный луп целиком).
fn collapse_repeats(text: &str) -> String {
    let mut words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < 6 {
        return text.to_string();
    }
    for _ in 0..4 {
        let keys = word_keys(&words);
        // 1. Периодический хвост: оставляем один период.
        let max_period = (words.len() / 3).min(6);
        let mut tail = None;
        for start in 0..words.len() {
            for period in 1..=max_period {
                let end = start + period;
                if end > words.len() {
                    break;
                }
                let mut i = end;
                let mut reps = 1;
                while i + period <= words.len() && keys[i..i + period] == keys[i - period..i] {
                    i += period;
                    reps += 1;
                }
                if reps >= 3 && i == words.len() {
                    tail = Some(end);
                }
            }
        }
        if let Some(end) = tail {
            words.truncate(end);
            continue;
        }
        // 2. Два одинаковых куска подряд: второй выкидываем.
        let mut cut = None;
        let max_block = (words.len() / 2).min(20);
        'blocks: for period in (2..=max_block).rev() {
            for i in 0..=words.len() - 2 * period {
                if keys[i..i + period] == keys[i + period..i + 2 * period] {
                    cut = Some(i + period..i + 2 * period);
                    break 'blocks;
                }
            }
        }
        match cut {
            Some(range) => {
                words.drain(range);
            }
            None => break,
        }
    }
    words.join(" ")
}

/// Слова в нижнем регистре без пунктуации — для сравнения повторов.
fn word_keys(words: &[&str]) -> Vec<String> {
    words
        .iter()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect()
        })
        .collect()
}

/// Убирает служебные пометки whisper: `[BLANK_AUDIO]`, `(music)`, `♪`, `*...*`.
///
/// В живом режиме они особенно мешают: на музыке модель выдаёт `[Music]`
/// вместо текста. Скобки удаляются вместе с содержимым, звёздочки и ноты —
/// просто выкидываются.
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut square = 0usize;
    let mut round = 0usize;
    for ch in text.chars() {
        match ch {
            '[' => {
                square += 1;
                continue;
            }
            ']' => {
                square = square.saturating_sub(1);
                continue;
            }
            '(' if square == 0 => {
                round += 1;
                continue;
            }
            ')' if square == 0 && round > 0 => {
                round = round.saturating_sub(1);
                continue;
            }
            '♪' | '♫' | '*' => continue,
            _ => {}
        }
        if square > 0 || round > 0 {
            continue;
        }
        out.push(ch);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Фразы-галлюцинации whisper на музыке, тишине и шуме.
const HALLUCINATIONS: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "thank you so much for watching",
    "please subscribe",
    "subscribe to my channel",
    "subscribe to the channel",
    "like and subscribe",
    "subtitles by",
    "transcription by",
    "amara org",
];

/// Текст в нижнем регистре без пунктуации — для сравнения с галлюцинациями.
fn normalize_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Текст без букв и цифр или из повторов одного слова — не речь.
fn is_noise(text: &str) -> bool {
    let letters = text.chars().filter(|c| c.is_alphanumeric()).count();
    if letters < 2 {
        return true;
    }
    // Фразы, которые whisper выдаёт на музыке и тишине (обучен на субтитрах
    // YouTube): это не речь стримера, а «эхо» обучающих данных.
    let norm = normalize_text(text);
    if HALLUCINATIONS
        .iter()
        .any(|h| norm == *h || norm.starts_with(&format!("{h} ")))
    {
        return true;
    }
    let mut words = text.split_whitespace();
    let Some(first) = words.next() else {
        return true;
    };
    let first = first
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase();
    if first.len() < 2 {
        return false;
    }
    let total = 1 + words.clone().count();
    let same = 1 + words
        .filter(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase() == first)
        .count();
    total >= 3 && same == total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(secs: f32, amp: f32) -> Vec<f32> {
        let n = (secs * SAMPLE_RATE as f32) as usize;
        (0..n)
            .map(|i| (i as f32 * 300.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * amp)
            .collect()
    }

    fn quiet(secs: f32) -> Vec<f32> {
        vec![0.0; (secs * SAMPLE_RATE as f32) as usize]
    }

    #[test]
    fn segmenter_splits_speech_by_pauses() {
        let mut seg = Segmenter::new();
        let mut signal = quiet(0.3);
        signal.extend(tone(0.8, 0.3));
        signal.extend(quiet(0.6));
        signal.extend(tone(0.8, 0.3));
        signal.extend(quiet(0.6));

        let mut phrases = Vec::new();
        // Кусочки неровные — как приходят пакеты из захвата.
        for chunk in signal.chunks(997) {
            phrases.extend(seg.push(chunk));
        }
        assert_eq!(phrases.len(), 2, "ждали две фразы, получили {}", phrases.len());
        for p in &phrases {
            let secs = p.samples.len() as f32 / SAMPLE_RATE as f32;
            assert!(
                (0.7..1.6).contains(&secs),
                "фраза {secs:.2} с — ожидали около 0.8 с плюс хвост"
            );
            assert!(p.peak > 0.15, "пик {}", p.peak);
        }
    }

    #[test]
    fn segmenter_ignores_silence_and_clicks() {
        let mut seg = Segmenter::new();
        let mut phrases = seg.push(&quiet(3.0));
        // Щелчок длиной в один кадр короче минимальной речи.
        let mut click = vec![0.0f32; FRAME * 3];
        click[FRAME] = 0.5;
        phrases.extend(seg.push(&click));
        phrases.extend(seg.push(&quiet(1.0)));
        assert!(phrases.is_empty(), "фраз быть не должно: {}", phrases.len());
    }

    #[test]
    fn clean_removes_service_markers() {
        assert_eq!(clean("[BLANK_AUDIO]"), "");
        assert_eq!(clean("(music)"), "");
        assert_eq!(clean("♪ la la ♪"), "la la");
        assert_eq!(clean("*laughs* hello  there"), "laughs hello there");
        assert_eq!(clean("  hello   world "), "hello world");
        assert_eq!(clean("I [pause] think so"), "I think so");
    }

    #[test]
    fn noise_is_recognised() {
        assert!(is_noise(""), "пустая строка");
        assert!(is_noise("♪ ♪"), "только ноты");
        assert!(is_noise("you you you"), "повтор одного слова");
        assert!(!is_noise("Thank you"), "короткая, но настоящая речь");
        assert!(!is_noise("Hello, how are you?"), "обычная фраза");
        // Галлюцинации whisper на музыке: текста в кадре нет, а фраза есть.
        assert!(
            is_noise("Thank you for watching!"),
            "типичная подпись с YouTube"
        );
        assert!(is_noise("Please subscribe to my channel."), "призыв подписаться");
        assert!(is_noise("Subtitles by Amara.org"), "подпись субтитров");
        assert!(!is_noise("I will subscribe tomorrow"), "это уже речь");
    }

    #[test]
    fn segments_snapshot_while_speech_goes_on() {
        let mut seg = Segmenter::new();
        assert!(seg.snapshot(5.0).is_none(), "тишина — снимка нет");
        seg.push(&tone(1.2, 0.3));
        let p = seg.snapshot(5.0).expect("речь идёт — снимок есть");
        let secs = p.samples.len() as f32 / SAMPLE_RATE as f32;
        assert!((0.5..1.5).contains(&secs), "снимок {secs:.2} с");
        assert!(p.peak > 0.15, "пик {}", p.peak);
        // Снимок не забирает звук: фраза продолжает копиться.
        let before = p.samples.len();
        seg.push(&tone(0.5, 0.3));
        let after = seg.snapshot(5.0).expect("фраза на месте").samples.len();
        assert!(after > before, "снимок обрезал фразу: {before} → {after}");
    }

    #[test]
    fn repeated_tail_is_collapsed() {
        let looped = "I am testing the I am testing the I am testing the I am testing the";
        assert_eq!(collapse_repeats(looped), "I am testing the");
        // Один повтор в обычной речи не трогаем.
        let normal = "I am testing the game and I am testing the sound";
        assert_eq!(collapse_repeats(normal), normal);
        // Короткие фразы не трогаем вовсе.
        assert_eq!(collapse_repeats("no no no"), "no no no");
        // Повтор в конце длинной фразы тоже схлопывается.
        let tail = "so today we play hello hello hello hello hello";
        assert_eq!(collapse_repeats(tail), "so today we play hello");
        // Музыкальный луп целым куском: второй такой же кусок выкидывается.
        let block = "What is this? What's happening? What is this? What's happening?";
        assert_eq!(collapse_repeats(block), "What is this? What's happening?");
    }

    #[test]
    fn judge_separates_speech_from_music() {
        // Обычная речь — показываем.
        assert!(matches!(
            judge("Hello, how are you?", 0.05, 1.5, None),
            Verdict::Show
        ));
        // Вокал поверх музыки: короткая фраза с высокой «не речью» — мимо.
        assert!(matches!(judge("Oh yeah", 0.7, 1.0, None), Verdict::NotSpeech));
        // Длинная фраза с той же оценкой — это уже речь стримера.
        assert!(matches!(
            judge("I think this game is really good today", 0.7, 2.0, None),
            Verdict::Show
        ));
        assert!(matches!(judge("Hello", 0.95, 0.5, None), Verdict::NotSpeech));
        assert!(matches!(judge("", 0.0, 0.5, None), Verdict::Empty));
        assert!(matches!(
            judge("Thank you for watching", 0.1, 1.0, None),
            Verdict::Empty
        ));
        // Одно слово на шесть секунд звука — обрывок распознавания, не реплика.
        assert!(matches!(judge("Oh", 0.1, 6.0, None), Verdict::Empty));
        assert!(matches!(judge("Yes!", 0.1, 0.6, None), Verdict::Show));
    }

    #[test]
    fn judge_drops_looped_noise() {
        // Зацикливание whisper на шуме: десятки слов на две секунды звука.
        let looped = "I I I I have no I I I have no I have no I have no I have no";
        assert!(matches!(judge(looped, 0.02, 1.9, None), Verdict::Garbled));
        assert!(too_dense(40, 1.9));
        // Один и тот же повтор ловится раньше — как «не текст».
        assert!(matches!(
            judge("no no no no no no no no", 0.02, 1.9, None),
            Verdict::Empty
        ));
        // Зацикливание внутри одного слова: «doo-doo-doo…» на сотню символов —
        // по словам фраза обычная, поэтому нужен отдельный признак.
        let doo = "We're gonna make a meet-a-doo-doo-doo-doo-doo-doo-doo-doo-doo-doo-doo-doo";
        assert!(matches!(judge(doo, 0.3, 6.0, None), Verdict::Garbled));
        assert!(garbled(doo, 5, 6.0));
        // Длинное, но настоящее слово не трогаем.
        assert!(!garbled("Antidisestablishmentarianism is a word", 5, 6.0));
        // Обычная речь той же длины проходит: запас покрывает короткие фразы.
        let speech = "I have no idea what is going on";
        assert!(matches!(judge(speech, 0.05, 1.9, None), Verdict::Show));
        assert!(!too_dense(7, 1.9));
        // Короткая фраза с запасом: у 0.4 с звука шкала ещё грубая.
        assert!(!too_dense(5, 0.4));
        assert!(too_dense(12, 0.4));
    }

    #[test]
    fn judge_drops_repeated_line() {
        let prev = (normalize_text("Hello, how are you?"), Instant::now());
        assert!(matches!(
            judge("hello how are you", 0.1, 1.0, Some(&prev)),
            Verdict::Repeat
        ));
        assert!(matches!(
            judge("What is going on", 0.1, 1.0, Some(&prev)),
            Verdict::Show
        ));
    }

    /// Печатает новые строки оверлея с временем от начала подачи звука.
    /// Помогает мерить задержку: промежуточная строка должна появиться
    /// во время речи, финальная — почти сразу после паузы.
    fn note_overlay(
        live: &Live,
        started: Instant,
        lines: &mut Vec<(String, Option<String>)>,
        partials: &mut usize,
        last_partial: &mut String,
    ) {
        let Some(cur) = live.overlay() else {
            return;
        };
        let at = started.elapsed().as_millis();
        if cur.partial {
            if cur.text != *last_partial {
                *partials += 1;
                *last_partial = cur.text.clone();
                println!("промежуточная через {at} мс: {}", cur.text);
            }
            return;
        }
        if lines.last().map(|(c, _)| c) != Some(&cur.text) {
            println!("строка через {at} мс: {}", cur.text);
            lines.push((cur.text, None));
        }
    }

    /// Живой прогон конвейера без WASAPI: звук подаётся прямо в буфер
    /// субтитров, а `Live` должен выдать строки. Проверяет и перевод —
    /// именно он «не работал» на реальном стриме.
    /// `cargo test --release live_pipeline_translates -- --ignored --nocapture`
    #[test]
    #[ignore = "требует модели whisper и перевода"]
    fn live_pipeline_translates_from_buffer() {
        let Ok(tmp) = std::env::var("TEMP") else {
            return;
        };
        let path = std::path::PathBuf::from(tmp).join("speech_en.wav");
        if !path.exists() {
            println!("нет файла с речью: {}", path.display());
            return;
        }
        let samples = read_wav(&path).expect("WAV читается");
        let shared = crate::subs::Shared::for_test();
        let mut live = Live::start(Arc::clone(&shared), true);
        // Звук кладём порциями, как его отдаёт захват: 100 мс звука за 100 мс
        // времени. Иначе кольцо переполняется, старый звук выбрасывается и
        // замер задержки теряет смысл.
        let started = Instant::now();
        let mut lines: Vec<(String, Option<String>)> = Vec::new();
        let mut partials = 0usize;
        let mut last_partial = String::new();
        for chunk in samples.chunks(SAMPLE_RATE as usize / 10) {
            shared.push_live(chunk);
            live.poll();
            note_overlay(&live, started, &mut lines, &mut partials, &mut last_partial);
            let ms = chunk.len() as u64 * 1000 / SAMPLE_RATE as u64;
            std::thread::sleep(Duration::from_millis(ms));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && lines.len() < 3 {
            live.poll();
            note_overlay(&live, started, &mut lines, &mut partials, &mut last_partial);
            std::thread::sleep(Duration::from_millis(100));
        }
        println!("промежуточных обновлений: {partials}");
        let details = live.details_line();
        let status = live.status_line();
        let recent = live.recent(3);
        println!("{status}\n{details}\n{recent:?}");
        live.stop();
        assert!(!lines.is_empty(), "субтитры не появились: {status}");
        assert!(
            details.contains("перевод: готов"),
            "перевод не загрузился: {details}"
        );
        assert!(
            lines.iter().any(|(l, _)| l.chars().any(|c| ('а'..='я').contains(&c))),
            "перевод не появился на экране: {lines:?}"
        );
    }

    /// Выключение и повторное включение CC: поток субтитров перезапускается,
    /// и движки (whisper + ONNX) загружаются заново. Раньше второй запуск
    /// перевода падал: `ort` разрешает одну среду на процесс, а мы её
    /// освобождали вместе с потоком.
    /// `cargo test --release live_restarts -- --ignored --nocapture`
    #[test]
    #[ignore = "требует модели whisper и перевода"]
    fn live_restarts_engines() {
        let shared = crate::subs::Shared::for_test();
        for round in 1..=2 {
            let mut live = Live::start(Arc::clone(&shared), true);
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline && !live.ready {
                live.poll();
                std::thread::sleep(Duration::from_millis(50));
            }
            println!(
                "круг {round}: готов={} · {} · {}",
                live.ready,
                live.status_line(),
                live.details_line()
            );
            assert!(live.ready, "круг {round}: движки не загрузились");
            assert!(
                live.details_line().contains("перевод: готов"),
                "круг {round}: перевод не загрузился — {}",
                live.details_line()
            );
            assert!(live.error.is_none(), "круг {round}: {:?}", live.error);
            // Ровно то, что делает кнопка CC: остановить поток.
            live.stop();
        }
    }

    #[test]
    fn normalize_lifts_quiet_speech_only() {
        let quiet = vec![0.01f32, -0.02, 0.02];
        let loud = normalize(&quiet, 0.02);
        let peak = loud.iter().fold(0f32, |a, s| a.max(s.abs()));
        assert!((0.6..0.75).contains(&peak), "пик после усиления {peak}");

        let normal = vec![0.3f32, -0.6];
        assert_eq!(normalize(&normal, 0.6), normal, "громкое не трогаем");

        // Ограничение усиления: очень тихий звук не превращается в шум.
        let tiny = vec![0.001f32];
        assert!(normalize(&tiny, 0.001)[0] <= 0.05);
    }

    #[test]
    fn line_prefers_translation() {
        let line = Line {
            en: "How are you?".to_string(),
            ru: Some("Как дела?".to_string()),
            at: Instant::now(),
        };
        assert_eq!(line.text(), "Как дела?");
        let line = Line {
            en: "How are you?".to_string(),
            ru: None,
            at: Instant::now(),
        };
        assert_eq!(line.text(), "How are you?");
    }

    /// Минимальный разбор WAV: 16-бит PCM, моно/стерео → моно `f32`.
    fn read_wav(path: &std::path::Path) -> Result<Vec<f32>, String> {
        let b = std::fs::read(path).map_err(|e| e.to_string())?;
        if b.len() < 44 || &b[0..4] != b"RIFF".as_slice() || &b[8..12] != b"WAVE".as_slice() {
            return Err("это не WAV".to_string());
        }
        let mut pos = 12;
        let mut channels = 1usize;
        while pos + 8 <= b.len() {
            let id = &b[pos..pos + 4];
            let size =
                u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
            let body = pos + 8;
            if id == b"fmt ".as_slice() {
                channels = u16::from_le_bytes([b[body + 2], b[body + 3]]) as usize;
            } else if id == b"data".as_slice() {
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

    /// Полный живой прогон: захват звука процесса → нарезка → распознавание →
    /// перевод. Звук подаётся синтезом речи (`%TEMP%\speech_en.wav`), поэтому
    /// проверяется и WASAPI, и потоки, и весь конвейер.
    /// `cargo test --release live_end_to_end -- --ignored --nocapture`
    #[test]
    #[ignore = "требует Windows 11, звуковое устройство и модели"]
    fn live_end_to_end_hears_synthesized_speech() {
        use windows_sys::Win32::Media::Audio::{
            PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT,
        };
        let Ok(tmp) = std::env::var("TEMP") else {
            return;
        };
        let path = std::path::PathBuf::from(tmp).join("speech_en.wav");
        if !path.exists() {
            println!("нет файла с речью: {}", path.display());
            return;
        }
        let wav = std::fs::read(&path).expect("WAV читается");
        let mut cap = crate::subs::Capture::start().expect("захват звука процесса");
        cap.set_live(true);
        let mut live = Live::start(cap.shared(), true);
        std::thread::sleep(Duration::from_millis(300));
        unsafe {
            PlaySoundW(
                wav.as_ptr() as *const u16,
                0,
                SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
            );
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut lines: Vec<String> = Vec::new();
        while Instant::now() < deadline && lines.len() < 4 {
            live.poll();
            if let Some(cur) = live.overlay() {
                if cur.partial {
                    continue;
                }
                if lines.last() != Some(&cur.text) {
                    println!("строка: {}", cur.text);
                    lines.push(cur.text);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        unsafe { PlaySoundW(std::ptr::null(), 0, 0) };
        let status = live.status_line();
        live.stop();
        cap.stop();
        assert!(!lines.is_empty(), "субтитры не появились: {status}");
        assert!(
            lines.iter().any(|l| {
                let s = l.to_lowercase();
                s.contains("привет") || s.contains("hello") || s.contains("как")
            }),
            "узнаваемый текст не найден: {lines:?}"
        );
    }

    /// Прогон всего конвейера на записанном WAV: проверка и замер задержки.
    /// Запуск: `cargo test --release live_pipeline -- --ignored --nocapture`
    /// Файл: `TW_LIVE_WAV`, иначе `%TEMP%\speech_en.wav`, иначе `capture.wav`.
    #[test]
    #[ignore = "требует модели whisper и перевода"]
    fn live_pipeline_on_capture_wav() {
        let path = if let Ok(p) = std::env::var("TW_LIVE_WAV") {
            std::path::PathBuf::from(p)
        } else if let Ok(tmp) = std::env::var("TEMP") {
            let speech = std::path::PathBuf::from(&tmp).join("speech_en.wav");
            if speech.exists() {
                speech
            } else {
                let Ok(base) = std::env::var("APPDATA") else {
                    return;
                };
                std::path::PathBuf::from(base)
                    .join("twitch_mini_player")
                    .join("capture.wav")
            }
        } else {
            return;
        };
        let samples = read_wav(&path).expect("WAV читается");
        assert!(
            samples.len() > SAMPLE_RATE as usize,
            "в записи больше секунды звука"
        );
        let peak = samples.iter().fold(0f32, |a, s| a.max(s.abs()));
        println!(
            "{} — {:.2} с, RMS {:.4}, пик {:.3}",
            path.display(),
            samples.len() as f32 / SAMPLE_RATE as f32,
            rms(&samples),
            peak
        );
        let asr = asr::Asr::new().expect("движок whisper и модель");
        let mut tr = mt::Translator::new().ok();
        let mut seg = Segmenter::new();
        let mut asr_total = Duration::ZERO;
        let mut mt_total = Duration::ZERO;
        let mut count = 0u32;
        for chunk in samples.chunks(SAMPLE_RATE as usize / 10) {
            for phrase in seg.push(chunk) {
                let audio = normalize(&phrase.samples, phrase.peak);
                let t0 = Instant::now();
                let en = clean(&asr.text_streaming(&audio).unwrap_or_default());
                let asr_dt = t0.elapsed();
                if en.is_empty() || is_noise(&en) {
                    continue;
                }
                let t1 = Instant::now();
                let ru = tr.as_mut().and_then(|t| t.translate(&en).ok());
                let mt_dt = t1.elapsed();
                asr_total += asr_dt;
                mt_total += mt_dt;
                count += 1;
                println!(
                    "{:.1} с звука → распознавание {:.0} мс, перевод {:.0} мс: {en} => {}",
                    phrase.samples.len() as f32 / SAMPLE_RATE as f32,
                    asr_dt.as_secs_f32() * 1000.0,
                    mt_dt.as_secs_f32() * 1000.0,
                    ru.unwrap_or_default()
                );
            }
        }
        println!(
            "фраз: {count}, среднее: распознавание {:.0} мс, перевод {:.0} мс",
            asr_total.as_secs_f32() * 1000.0 / count.max(1) as f32,
            mt_total.as_secs_f32() * 1000.0 / count.max(1) as f32
        );
    }
}

