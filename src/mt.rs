//! Перевод en→ru: MarianMT `opus-mt-en-ru` (int8) на ONNX Runtime.
//!
//! Файлы модели лежат в `%APPDATA%\twitch_mini_player\models\mt`:
//! `tokenizer.json`, `encoder_model_quantized.onnx`,
//! `decoder_model_quantized.onnx`, опционально `generation_config.json`.
//! `onnxruntime.dll` ищется рядом с `.exe`, в `tools\onnx` и в
//! `%APPDATA%\twitch_mini_player\tools\onnx` — его ставит установщик.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

pub const TOKENIZER_FILE: &str = "tokenizer.json";
pub const ENCODER_FILE: &str = "encoder_model_quantized.onnx";
pub const DECODER_FILE: &str = "decoder_model_quantized.onnx";
pub const ONNX_DLL: &str = "onnxruntime.dll";

/// Запасное ограничение длины ответа, если нет `generation_config.json`.
const MAX_NEW_TOKENS: usize = 96;

/// Каталог модели перевода по умолчанию (профиль пользователя).
pub fn models_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("twitch_mini_player").join("models").join("mt")
}

/// Каталоги, где ищем модель: профиль пользователя, затем рядом с `.exe`
/// (portable-версия и установщик кладут модели в `models\mt`).
pub fn model_candidates() -> Vec<PathBuf> {
    let mut dirs = vec![models_dir()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("models").join("mt"));
            if let Some(root) = dir.parent().and_then(|p| p.parent()) {
                dirs.push(root.join("models").join("mt"));
            }
        }
    }
    dirs
}

/// Первый каталог, где есть токенизатор; если такого нет — профиль
/// пользователя, чтобы сообщение об ошибке указывало туда, куда пишет
/// `scripts\fetch-mt.ps1`.
pub fn resolve_models_dir() -> PathBuf {
    model_candidates()
        .into_iter()
        .find(|d| d.join(TOKENIZER_FILE).exists())
        .unwrap_or_else(models_dir)
}

/// Где искать `onnxruntime.dll`.
fn dll_candidates() -> Vec<PathBuf> {
    let mut c = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            c.push(dir.join(ONNX_DLL));
            c.push(dir.join("tools").join("onnx").join(ONNX_DLL));
            for up in [2usize, 3] {
                let mut root = dir.to_path_buf();
                for _ in 0..up {
                    root = match root.parent() {
                        Some(p) => p.to_path_buf(),
                        None => break,
                    };
                    c.push(root.join("tools").join("onnx").join(ONNX_DLL));
                }
            }
        }
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        c.push(
            PathBuf::from(appdata)
                .join("twitch_mini_player")
                .join("tools")
                .join("onnx")
                .join(ONNX_DLL),
        );
    }
    if let Ok(cwd) = std::env::current_dir() {
        c.push(cwd.join("tools").join("onnx").join(ONNX_DLL));
    }
    c
}

/// Путь к найденному `onnxruntime.dll`.
pub fn find_onnx_dll() -> Option<PathBuf> {
    let mut list = dll_candidates();
    list.push(PathBuf::from(ONNX_DLL));
    list.into_iter().find(|p| p.exists())
}

/// Готов ли перевод: есть рантайм и все файлы модели.
pub fn is_available() -> bool {
    let dir = models_dir();
    find_onnx_dll().is_some()
        && dir.join(TOKENIZER_FILE).exists()
        && dir.join(ENCODER_FILE).exists()
        && dir.join(DECODER_FILE).exists()
}

/// Параметры генерации из `generation_config.json` (если он есть).
struct GenConfig {
    start_token: i64,
    eos_token: i64,
    max_new: usize,
}

impl GenConfig {
    fn load(dir: &Path) -> Self {
        let mut cfg = Self {
            // MarianMT начинает декодирование с `<pad>`.
            start_token: 58100,
            eos_token: 0,
            max_new: MAX_NEW_TOKENS,
        };
        let path = dir.join("generation_config.json");
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(v) = json.get("decoder_start_token_id").and_then(|v| v.as_i64()) {
                    cfg.start_token = v;
                }
                if let Some(v) = json.get("eos_token_id").and_then(|v| v.as_i64()) {
                    cfg.eos_token = v;
                }
                if let Some(v) = json.get("max_length").and_then(|v| v.as_u64()) {
                    cfg.max_new = (v as usize).clamp(16, 512);
                }
            }
        }
        cfg
    }
}

/// Xenova-версия `tokenizer.json` содержит `"precompiled_charsmap": null`,
/// который `tokenizers` не принимает. Подменяем `Precompiled` на `NFKC`
/// (для английского текста разница несущественна).
fn patch_tokenizer_json(raw: &str) -> Option<String> {
    fn walk(v: &mut serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(map) => {
                let is_precompiled = map
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| t == "Precompiled")
                    .unwrap_or(false);
                if is_precompiled {
                    map.clear();
                    map.insert(
                        "type".to_string(),
                        serde_json::Value::String("NFKC".to_string()),
                    );
                    return true;
                }
                map.values_mut().any(walk)
            }
            serde_json::Value::Array(items) => items.iter_mut().any(walk),
            _ => false,
        }
    }
    let mut v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if !walk(&mut v) {
        return None;
    }
    serde_json::to_string(&v).ok()
}

/// Движок перевода: токенизатор + две ONNX-сессии (энкодер и декодер).
pub struct Translator {
    tokenizer: Tokenizer,
    encoder: Session,
    decoder: Session,
    cfg: GenConfig,
    dir: PathBuf,
    /// Среда ONNX Runtime: держим, пока живы сессии.
    _env: ort::environment::Environment,
}

impl Translator {
    /// Открывает модель из `%APPDATA%\twitch_mini_player\models\mt`
    /// (или из `models\mt` рядом с `.exe` — portable-версия).
    pub fn new() -> Result<Self, String> {
        Self::with_dir(&resolve_models_dir())
    }

    /// Открывает модель из указанного каталога.
    pub fn with_dir(dir: &Path) -> Result<Self, String> {
        for f in [TOKENIZER_FILE, ENCODER_FILE, DECODER_FILE] {
            let p = dir.join(f);
            if !p.exists() {
                return Err(format!(
                    "перевод недоступен: нет файла {} — запустите scripts\\fetch-mt.ps1",
                    p.display()
                ));
            }
        }
        let dll = find_onnx_dll().ok_or_else(|| {
            "перевод недоступен: onnxruntime.dll не найден рядом с .exe (папка tools\\onnx)"
                .to_string()
        })?;
        let env = ort::init_from(&dll)
            .map_err(|e| format!("onnxruntime.dll ({}): {e}", dll.display()))?
            .build()
            .map_err(|e| format!("onnxruntime: {e}"))?;
        // `tokenizers` паникует на `precompiled_charsmap: null` (Xenova-версия),
        // поэтому json правим всегда, ещё до разбора.
        let raw = std::fs::read_to_string(dir.join(TOKENIZER_FILE))
            .map_err(|e| format!("tokenizer.json: {e}"))?;
        let json = patch_tokenizer_json(&raw).unwrap_or(raw);
        let tokenizer =
            Tokenizer::from_str(&json).map_err(|e| format!("tokenizer.json: {e}"))?;
        let encoder = Session::builder(&env)
            .map_err(|e| format!("энкодер: {e}"))?
            .commit_from_file(dir.join(ENCODER_FILE))
            .map_err(|e| format!("энкодер: {e}"))?;
        let decoder = Session::builder(&env)
            .map_err(|e| format!("декодер: {e}"))?
            .commit_from_file(dir.join(DECODER_FILE))
            .map_err(|e| format!("декодер: {e}"))?;
        Ok(Self {
            tokenizer,
            encoder,
            decoder,
            cfg: GenConfig::load(dir),
            dir: dir.to_path_buf(),
            _env: env,
        })
    }

    pub fn model_dir(&self) -> &Path {
        &self.dir
    }

    /// Переводит английский текст на русский (жадно, без past-кэша).
    pub fn translate(&mut self, text: &str) -> Result<String, String> {
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| format!("токенизация: {e}"))?;
        let mut ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
        if ids.is_empty() {
            return Ok(String::new());
        }
        // MarianMT ждёт `</s>` в конце входа.
        if ids.last().copied() != Some(self.cfg.eos_token) {
            ids.push(self.cfg.eos_token);
        }
        let n = ids.len();
        let shape = vec![1i64, n as i64];
        let ids_t = Tensor::from_array((shape.clone(), ids.into_boxed_slice()))
            .map_err(|e| format!("вход энкодера: {e}"))?;
        let mask_t = Tensor::from_array((shape, vec![1i64; n].into_boxed_slice()))
            .map_err(|e| format!("маска энкодера: {e}"))?;
        let enc_out = self
            .encoder
            .run(ort::inputs!["input_ids" => ids_t, "attention_mask" => mask_t])
            .map_err(|e| format!("энкодер: {e}"))?;
        let (h_shape, h_data) = {
            let (s, d) = enc_out["last_hidden_state"]
                .try_extract_tensor::<f32>()
                .map_err(|e| format!("выход энкодера: {e}"))?;
            (
                s.iter().map(|v| *v as i64).collect::<Vec<i64>>(),
                d.to_vec(),
            )
        };
        drop(enc_out);

        let mut out_ids = vec![self.cfg.start_token];
        for _ in 0..self.cfg.max_new {
            let d_ids = Tensor::from_array((
                vec![1i64, out_ids.len() as i64],
                out_ids.clone().into_boxed_slice(),
            ))
            .map_err(|e| format!("вход декодера: {e}"))?;
            let d_mask = Tensor::from_array((
                vec![1i64, n as i64],
                vec![1i64; n].into_boxed_slice(),
            ))
            .map_err(|e| format!("маска декодера: {e}"))?;
            let h = Tensor::from_array((h_shape.clone(), h_data.clone().into_boxed_slice()))
                .map_err(|e| format!("скрытое состояние: {e}"))?;
            let out = self
                .decoder
                .run(ort::inputs![
                    "input_ids" => d_ids,
                    "encoder_attention_mask" => d_mask,
                    "encoder_hidden_states" => h
                ])
                .map_err(|e| format!("декодер: {e}"))?;
            let next = {
                let (ls, logits) = out["logits"]
                    .try_extract_tensor::<f32>()
                    .map_err(|e| format!("логиты: {e}"))?;
                let vocab = *ls
                    .last()
                    .ok_or_else(|| "пустая форма логитов".to_string())?
                    as usize;
                let steps = (logits.len() / vocab).max(1);
                let last = &logits[(steps - 1) * vocab..steps * vocab];
                let mut best = 0usize;
                let mut bestv = f32::NEG_INFINITY;
                for (i, &v) in last.iter().enumerate() {
                    if v > bestv {
                        bestv = v;
                        best = i;
                    }
                }
                best as i64
            };
            if next == self.cfg.eos_token {
                break;
            }
            out_ids.push(next);
        }
        let body: Vec<u32> = out_ids[1..].iter().map(|&i| i as u32).collect();
        self.tokenizer
            .decode(&body, true)
            .map_err(|e| format!("декодирование: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dir_is_under_models_mt() {
        let d = models_dir();
        assert!(
            d.ends_with(Path::new("twitch_mini_player").join("models").join("mt")),
            "{}",
            d.display()
        );
    }

    #[test]
    fn candidates_start_with_user_profile_and_include_exe_dir() {
        let dirs = model_candidates();
        assert_eq!(dirs[0], models_dir());
        // Второй вариант — рядом с .exe (portable-версия и установщик).
        let exe = std::env::current_exe().expect("текущий exe");
        let beside = exe.parent().expect("папка exe").join("models").join("mt");
        assert!(dirs.contains(&beside), "{dirs:?}");
        // Поиск отдаёт либо профиль пользователя, либо найденный каталог.
        let resolved = resolve_models_dir();
        assert!(dirs.contains(&resolved), "{}", resolved.display());
    }

    #[test]
    fn patches_precompiled_normalizer() {
        let raw = r#"{"normalizer":{"type":"Precompiled","precompiled_charsmap":null}}"#;
        let fixed = patch_tokenizer_json(raw).expect("нормализатор должен замениться");
        assert!(fixed.contains("NFKC"), "{fixed}");
        assert!(!fixed.contains("precompiled_charsmap"), "{fixed}");
    }

    #[test]
    fn missing_model_is_reported() {
        let dir = std::env::temp_dir().join("tmp-empty-mt-model");
        let _ = std::fs::create_dir_all(&dir);
        let err = match Translator::with_dir(&dir) {
            Ok(_) => panic!("пустой каталог не должен открываться"),
            Err(e) => e,
        };
        assert!(err.contains("нет файла"), "{err}");
    }

    /// Живой прогон: `cargo test --release mt -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_translates() {
        let mut tr = match Translator::new() {
            Ok(t) => t,
            Err(e) => {
                println!("перевод недоступен: {e}");
                return;
            }
        };
        for src in [
            "Hello, how are you?",
            "I am going to play some games today.",
        ] {
            match tr.translate(src) {
                Ok(ru) => println!("{src} => {ru}"),
                Err(e) => panic!("перевод: {e}"),
            }
        }
    }
}

