#!/usr/bin/env bash
# WizReader 启动脚本
# 用法:
#   ./wizreader.sh dev     开发模式（热重载，Vite + tauri dev）
#   ./wizreader.sh app     启动已构建的 release 应用
#   ./wizreader.sh build   构建 release 应用（.app）
#   ./wizreader.sh cli ... 直接触发 wiz-cli（build-index / search / peek）
# 不带参数时等价于 app。
# 注：wiz-cli 为独立 workspace 成员（src-tauri/cli/），不参与 tauri 打包

set -euo pipefail

PROJ_DIR="$(cd "$(dirname "$0")" && pwd)"
TAURI_DIR="$PROJ_DIR/src-tauri"
APP_BUNDLE="$TAURI_DIR/target/release/bundle/macos/wizreader.app"
APP_BIN="$TAURI_DIR/target/release/wizreader"
CLI_BIN="$TAURI_DIR/target/release/wiz-cli"
# 默认数据源目录（留空则自动探测 ~/.wiznote/<账号>/data，也可手动指定）
DEFAULT_DATA_DIR=""
WIZ_HOME="$HOME/.wizreader"

# 自动探测为知数据源（macOS 结构：~/.wiznote/<账号>/data，含 index.db 与 notes/）
if [ -z "$DEFAULT_DATA_DIR" ]; then
  for d in "$HOME"/.wiznote/*/data; do
    if [ -f "$d/index.db" ] && [ -d "$d/notes" ]; then
      DEFAULT_DATA_DIR="$d"
      break
    fi
  done
fi

# 首次运行：生成 settings.json，避免每次都要进设置页手选目录
ensure_settings() {
  if [ ! -f "$WIZ_HOME/settings.json" ]; then
    mkdir -p "$WIZ_HOME"
    if [ -n "$DEFAULT_DATA_DIR" ]; then
      local data_json="\"$DEFAULT_DATA_DIR\""
    else
      local data_json="null"
    fi
    cat > "$WIZ_HOME/settings.json" <<EOF
{
  "data_dir": $data_json,
  "font_size": 16,
  "theme": "system",
  "read_width": 860,
  "allow_remote": false
}
EOF
    if [ -n "$DEFAULT_DATA_DIR" ]; then
      echo "[wizreader] 已初始化配置: $WIZ_HOME/settings.json (data_dir=$DEFAULT_DATA_DIR)"
    else
      echo "[wizreader] 已初始化配置: $WIZ_HOME/settings.json（未探测到数据源，请在应用内点击“设置数据源”）"
    fi
    echo "[wizreader] 若索引缺失，请先执行: $0 cli build-index <data_dir>"
  fi
}

cmd="${1:-app}"
shift || true

case "$cmd" in
  dev)
    ensure_settings
    cd "$PROJ_DIR" && npm run tauri dev "$@"
    ;;
  app)
    ensure_settings
    if [ -d "$APP_BUNDLE" ]; then
      open "$APP_BUNDLE" "$@"
    elif [ -x "$APP_BIN" ]; then
      exec "$APP_BIN" "$@"
    else
      echo "[wizreader] 尚未构建 release 应用，请先执行: $0 build" >&2
      echo "[wizreader] 或改用开发模式: $0 dev" >&2
      exit 1
    fi
    ;;
  build)
    cd "$PROJ_DIR" && npm run tauri build "$@"
    ;;
  cli)
    if [ ! -x "$CLI_BIN" ]; then
      echo "[wizreader] 首次使用，正在编译 wiz-cli（release）..." >&2
      (cd "$TAURI_DIR" && cargo build --release -p wiz-cli)
    fi
    exec "$CLI_BIN" "$@"
    ;;
  *)
    echo "用法: $0 {dev|app|build|cli <args>}"
    exit 1
    ;;
esac
