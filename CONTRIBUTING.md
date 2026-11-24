# Как предложить правку

Спасибо, что хотите помочь. Проект небольшой, поэтому процесс простой:
**баги и идеи — в issues, код — в pull request**. Отдельного «уровня доверия» нет:
любой может форкнуть репозиторий, собрать и прислать PR.

- Репозиторий: <https://github.com/bedolagainc/twitch-mini-player>
- Issues: <https://github.com/bedolagainc/twitch-mini-player/issues>
- Releases: <https://github.com/bedolagainc/twitch-mini-player/releases>

## Быстрый старт

1. Нажмите **Fork** и клонируйте свой форк:

   ```powershell
   git clone https://github.com/<ваш-логин>/twitch-mini-player.git
   cd twitch-mini-player
   ```

2. Создайте ветку от `main` — по смыслу правки:

   ```
   fix/level-bar-freeze
   feat/quality-hotkeys
   docs/readme-typos
   ```

3. Внесите изменения, соберите и прогоните тесты (см. ниже).
4. Обновите `CHANGELOG.md` и версию (см. «Правила проекта»).
5. Коммит, push, PR в `main`. Один PR — одна тема: так проще ревьюить.

## Сборка и проверка

Требуется Windows (проект использует WASAPI и Win32) и установленный Rust
(<https://rustup.rs/>). `streamlink` и `libmpv-2.dll` нужны для **запуска**,
но не для сборки и не для тестов.

```powershell
cargo build --release --locked
cargo test --release --locked
```

Ожидаемо: `27 passed; 0 failed; 5 ignored`. Пропущенные тесты — живые
(`render_real_frame` — нужен `libmpv-2.dll`; `capture_live` — реальный звук;
`live_transcribes` и `live_translates` — модели распознавания и перевода).
Их можно запустить руками:

```powershell
cargo test --release -- --ignored --nocapture
```

Проверка вручную (без Twitch) — через демо-поток:

```powershell
$env:TW_DEMO_URL = 'av://lavfi:testsrc=size=1280x720:rate=30'
.\target\release\twitch_mini_player.exe
```

## Правила проекта

- **Каждое изменение сопровождается версией.** В проекте это не бюрократия, а
  способ понять по `.exe`, что внутри:
  - `patch` (`0.18.1`) — исправление без нового поведения;
  - `minor` (`0.19.0`) — новая функция или UX;
  - `major` (`1.0.0`) — ломающее изменение.
- **Появились поля в настройках** → поднимите `SETTINGS_VERSION` и опишите
  миграцию: пользовательские поля (канал, громкость, окно) должны выжить.
- **`CHANGELOG.md`** — обязательная запись в `[Unreleased]` по разделам
  «Добавлено / Изменено / Удалено / Исправлено».
- **Версия и `CHANGELOG` — в одном PR**, иначе CI соберёт `.exe` с одной
  версией, а в описании релиза будет другая.

## Стиль кода

- Комментарии, doc-комментарии и все строки интерфейса — **по-русски**
  (проект русскоязычный, так проще читать диффы).
- Windows-специфичный код — под `#[cfg(windows)]`; `windows-sys` уже подключён,
  новые зависимости обсуждаемы: каждая — плюс к размеру `.exe`.
- Ошибки — обычными `Result<_, String>` в стиле проекта, без `unwrap()` там, где
  пользователь может увидеть панику (паники пишутся в
  `%TEMP%\twitch_mini_player_crash.txt`).
- Не блокируйте UI: сеть, `streamlink` и захват звука живут в фоновых потоках,
  общение — через `mpsc`-каналы.

## Что проверит CI

GitHub Actions (`.github/workflows/build.yml`) на каждый push и PR:

1. `cargo build --release --locked`
2. `cargo test --release --locked`
3. `scripts/fetch-vendor.ps1` — libmpv, streamlink, whisper.cpp, модели
4. упаковка портативного `twitch_mini_player-<версия>-windows-x64.zip`
5. сборка установщика `twitch_mini_player-<версия>-setup.exe` (Inno Setup 6)

Оба файла — артефакты сборки. При пуше тега `v*` они прикрепляются к релизу —
так появляются сборки в [Releases](https://github.com/bedolagainc/twitch-mini-player/releases).

## Если PR принят

Правка попадает в `main`, версия поднимается, тег `v<версия>` создаёт релиз
со сборкой. Автор правки указывается в `CHANGELOG.md`, если он этого хочет.
