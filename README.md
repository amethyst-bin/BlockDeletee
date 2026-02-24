# BlockDeletee

[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20Windows-blue)](#build--run)
[![UI](https://img.shields.io/badge/UI-TUI%20%2B%20Qt%20(QML)-6f42c1)](#ui-modes)
[![Stars](https://img.shields.io/github/stars/amethyst-bin/BlockDeletee?style=social)](https://github.com/amethyst-bin/BlockDeletee/stargazers)
[![Issues](https://img.shields.io/github/issues/amethyst-bin/BlockDeletee)](https://github.com/amethyst-bin/BlockDeletee/issues)

Голосовое удаление блоков в Minecraft по RCON + Vosk (RU), переписанное на Rust.

Приложение слушает микрофон, распознает речь, сопоставляет фразы/алиасы с блоками и удаляет найденные блоки в чанке игрока через `fill`.

## Features

- Rust backend (быстрее и кроссплатформенно)
- Vosk speech recognition (русская модель)
- Minecraft RCON (без Python-зависимостей)
- `TUI` интерфейс (`ratatui`)
- `Qt/QML` интерфейс (`qmetaobject`)
- Настройки в UI (IP, Port, Username, UI mode)
- Автоперезапуск после изменения настроек, требующих рестарт
- Цветные логи, статусы `MIC/REC/RCON/PLAYER`

## UI Modes

В `config.json`:

```json
{
  "ui": {
    "mode": "tui"
  }
}
```

Поддерживается:

- `tui`
- `qt`

Если поле отсутствует, при старте будет выбор интерфейса.

## Quick Start (Release)

### Windows

Положи рядом:

- `blockdeletee.exe`
- `vosk.dll`
- `config.json` (или `config.example.json` -> переименовать)
- `blocks.json`
- папку `models/`

Запусти:

```bat
blockdeletee.exe
```

### Linux

Положи рядом:

- `blockdeletee`
- `config.json` (или `config.example.json` -> переименовать)
- `blocks.json`
- папку `models/`

И установи `libvosk.so` (или добавь в `LD_LIBRARY_PATH`).

Запуск:

```bash
chmod +x ./blockdeletee
./blockdeletee
```

## Build & Run

### Requirements

- Rust (`cargo`, `rustc`)
- Qt 6 dev packages (для `qt` UI и сборки текущего проекта)
- Vosk runtime library
- Для Linux: ALSA dev (`alsa-lib`)

### Linux (native)

```bash
cargo run -- --config config.json
```

Release:

```bash
cargo build --release
```

### Windows (cross-compile from Linux, GNU)

Нужны:

- `rustup target add x86_64-pc-windows-gnu`
- `mingw-w64-gcc`
- Windows `vosk.dll` + `libvosk.dll.a`

Сборка:

```bash
RUSTFLAGS="-L native=$(pwd)/third_party/vosk-win64" \
cargo build --release --target x86_64-pc-windows-gnu
```

Примечание: `Qt` cross-compile требует Windows Qt SDK/headers. Без него Windows-сборка `qt` режима с Linux не соберется.

## Config

Смотри `config.example.json`.

Основные поля:

- `minecraft.rcon_host`
- `minecraft.rcon_port`
- `minecraft.rcon_password`
- `microphone.player_name`
- `speech.model_path`
- `ui.mode`

## Development Notes

- `TUI` и `Qt` вынесены в отдельные модули:
  - `src/ui_tui.rs`
  - `src/ui_qt.rs`
- backend bootstrap:
  - `src/backend_bootstrap.rs`

## License

Лицензия пока не добавлена. Если планируется публичное использование, добавь `LICENSE`.
