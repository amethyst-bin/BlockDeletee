#!/usr/bin/env bash
set -euo pipefail

APP_DIR="/opt/blockdeletee"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/blockdeletee"
CFG_FILE="${CFG_DIR}/config.json"

mkdir -p "$CFG_DIR"

if [[ ! -f "$CFG_FILE" ]]; then
  cp "$APP_DIR/config.example.json" "$CFG_FILE"
  echo "[BlockDeletee] Создан шаблон конфига: $CFG_FILE" >&2
  echo "[BlockDeletee] Заполни RCON host/password/player_name и запусти снова." >&2
fi

cd "$APP_DIR"
exec ./blockdeletee --config "$CFG_FILE" "$@"
