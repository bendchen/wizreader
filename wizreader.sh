#!/usr/bin/env bash
# WizReader 启动脚本
# 用法:
#   ./wizreader.sh dev     开发模式（热重载，Vite + tauri dev）
#   ./wizreader.sh app     启动已构建的 release 应用
#   ./wizreader.sh build   构建 release 应用（默认只打宿主架构 .app，跳过 DMG）
#                          构建前会删除所有旧包：失败就没有包，绝不回退旧版本
#   ./wizreader.sh build universal  构建**通用包** .app（x86_64 + arm64，tauri 双次编译后 lipo 合并）
#                                   自动校验/安装两个 Rust 目标；构建完自验架构并打印结果
#   ./wizreader.sh build universal dmg  通用包 + DMG（需「终端控制 Finder」自动化权限）
#   ./wizreader.sh build dmg  单独编译 DMG 发行镜像（需「终端控制 Finder」自动化权限）
#   ./wizreader.sh cli ... 直接触发 wiz-cli（build-index / search / peek）
# 不带参数时等价于 app。
# 注：wiz-cli 为独立 workspace 成员（src-tauri/cli/），不参与 tauri 打包

set -euo pipefail

PROJ_DIR="$(cd "$(dirname "$0")" && pwd)"
TAURI_DIR="$PROJ_DIR/src-tauri"
# 应用产物位置：优先 Universal 包（tauri build --target universal-apple-darwin），
# 回退宿主架构包（tauri build）
APP_BUNDLE_UNIVERSAL="$TAURI_DIR/target/universal-apple-darwin/release/bundle/macos/wizreader.app"
APP_BUNDLE_HOST="$TAURI_DIR/target/release/bundle/macos/wizreader.app"
APP_BIN="$TAURI_DIR/target/universal-apple-darwin/release/wizreader"
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

# 通用包（universal）前置：两个 Rust 目标都必须就位。
# Intel 机器上 arm64 侧是交叉编译、Apple Silicon 上 x86_64 侧是交叉编译 —— 两种都要有 std。
# 已装则零开销（只做一次列表比对）；缺了先尝试自动安装（需要网络），失败给出可复制的命令。
UNIVERSAL_TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
ensure_universal_targets() {
  if ! command -v rustup >/dev/null 2>&1; then
    echo "[wizreader] 未找到 rustup，无法校验通用包的编译目标。" >&2
    echo "[wizreader] 请先安装 rustup，再执行: rustup target add $UNIVERSAL_TARGETS" >&2
    exit 1
  fi
  local installed t missing=""
  installed="$(rustup target list --installed 2>/dev/null || true)"
  for t in $UNIVERSAL_TARGETS; do
    printf '%s\n' "$installed" | grep -qx "$t" || missing="$missing $t"
  done
  if [ -n "$missing" ]; then
    echo "[wizreader] 缺少编译目标:$missing —— 正在安装（需要网络）..."
    # shellcheck disable=SC2086  # 故意按空格拆分
    rustup target add $missing || {
      echo "[wizreader] 目标安装失败，请手动执行: rustup target add $UNIVERSAL_TARGETS" >&2
      exit 1
    }
  fi
}

cmd="${1:-app}"
shift || true

# 取两个候选包里最新构建的那一个。
# 不能固定优先 Universal：重建宿主包后若仍打开旧的 Universal 包，会看到过期界面。
bundle_bin() { printf '%s/Contents/MacOS/wizreader' "$1"; }
newest_bundle() {
  local b ts newest="" newest_ts=0
  for b in "$APP_BUNDLE_UNIVERSAL" "$APP_BUNDLE_HOST"; do
    [ -x "$(bundle_bin "$b")" ] || continue
    ts=$(stat -f %m "$(bundle_bin "$b")")
    if [ "$ts" -gt "$newest_ts" ]; then newest_ts=$ts; newest="$b"; fi
  done
  [ -n "$newest" ] && printf '%s\n' "$newest"
}

# 源码内容指纹。不能用 mtime 判新旧：编辑器回写会把时间戳推过构建时刻（实测误报），
# 而 tauri 构建内嵌的是按内容哈希的资源，所以指纹只取内容。
STAMP_FILE="$TAURI_DIR/target/.wizreader-src-digest"
src_digest() {
  find "$PROJ_DIR/src" "$TAURI_DIR/src" "$PROJ_DIR/index.html" "$TAURI_DIR/tauri.conf.json" \
    -type f ! -name .DS_Store -print 2>/dev/null | sort | xargs shasum -a 1 2>/dev/null | shasum -a 1 | cut -c1-12
}

case "$cmd" in
  dev)
    ensure_settings
    cd "$PROJ_DIR" && npm run tauri dev "$@"
    ;;
  app)
    ensure_settings
    APP_BUNDLE="$(newest_bundle || true)"
    if [ -n "$APP_BUNDLE" ]; then
      # 陈旧提醒：源码指纹与上次构建记录不一致（没指纹文件则不提示，避免 npm 直构建时报错噪声）
      if [ -f "$STAMP_FILE" ]; then
        recorded="$(cat "$STAMP_FILE")"
        current="$(src_digest)"
        if [ "$recorded" != "$current" ]; then
          echo "[wizreader] 注意：源码已改动（$recorded → ${current}）但在用包仍是旧构建，界面可能缺少新功能。" >&2
          echo "[wizreader] 重建后再启动: $0 build   （开发模式: $0 dev）" >&2
        fi
      fi
      open "$APP_BUNDLE" "$@"
    elif [ -x "$APP_BIN" ]; then
      exec "$APP_BIN" "$@"
    else
      echo "[wizreader] 尚未构建 release 应用，请先执行: $0 build" >&2
      echo "[wizreader] 或构建通用包（x86_64 + arm64）: $0 build universal" >&2
      echo "[wizreader] 或改用开发模式: $0 dev" >&2
      exit 1
    fi
    ;;
  build)
    # ./wizreader.sh build             默认只打**宿主架构** .app（日常开发足够）
    # ./wizreader.sh build universal   通用包 .app（x86_64 + arm64，--bundles app）
    # ./wizreader.sh build universal dmg  通用包 + DMG
    # ./wizreader.sh build dmg         单独编译 DMG 发行镜像（--bundles dmg）
    # 说明：DMG 步骤（bundle_dmg.sh）需要「终端控制 Finder」自动化权限，
    # 被拒绝后 TCC 会记住并导致 DMG 打包必失败——
    # 需先在 系统设置 → 隐私与安全性 → 自动化 允许终端控制 Finder
    # （若列表无该条目：tccutil reset AppleEvents 重置后重跑，弹窗点「好」）。
    #
    # ⚠️ 同理必须显式带 --bundles：脚本只在**零参数**时补 `--bundles app`，
    # 一旦传了 --target（参数数不为 0）就不再补，会走 conf 的 `targets: "all"`（含 DMG）。
    BUILD_UNIVERSAL=""
    if [ "${1:-}" = "universal" ]; then
      shift
      ensure_universal_targets
      BUILD_UNIVERSAL=1
      if [ "${1:-}" = "dmg" ]; then
        shift
        set -- --target universal-apple-darwin --bundles dmg "$@"
      else
        set -- --target universal-apple-darwin --bundles app "$@"
      fi
    elif [ "${1:-}" = "dmg" ]; then
      shift
      set -- --bundles dmg "$@"
    elif [ "$#" -eq 0 ]; then
      set -- --bundles app
    fi
    # 构建前清理所有旧包（宿主 .app / Universal 旧包 / 旧 DMG / 失败残留 rw.*.dmg）：
    # 保证「编译成功才有程序，失败就没有程序」，避免 app 启动时静默回退到过期旧包。
    # （Universal 包如需保留请自行注释下行；需要时重跑 `$0 build universal` 重新构建）
    echo "[wizreader] 清理旧构建包..."
    rm -rf "$APP_BUNDLE_HOST" "$APP_BUNDLE_UNIVERSAL" \
           "$TAURI_DIR/target/release/bundle/dmg" \
           "$TAURI_DIR/target/universal-apple-darwin/release/bundle"
    find "$TAURI_DIR/target/release/bundle/macos" -name 'rw.*.dmg' -delete 2>/dev/null || true
    # npm run 转发参数必须带 -- 分隔符，否则 --bundles 会被 npm 自己吃掉
    cd "$PROJ_DIR" && npm run tauri build -- "$@"
    # 记录本次构建对应的源码指纹，供 app 启动时比对。
    # 指纹失败不得判定整个构建失败 —— 包已经产出了（find 对不存在路径返回非零，
    # 叠加 pipefail 会让脚本在这里静默退出，看起来像"构建失败"但其实产物是好的）。
    if mkdir -p "$(dirname "$STAMP_FILE")" && src_digest > "$STAMP_FILE" && [ -s "$STAMP_FILE" ]; then
      echo "[wizreader] 已记录源码指纹 $(cat "$STAMP_FILE")"
    else
      echo "[wizreader] 警告：源码指纹记录失败（不影响已产出的程序包）" >&2
    fi
    # 通用包自验：tauri 若因目标缺失静默退回单架构，必须在这里拦住（否则以为打出了通用包）
    if [ -n "$BUILD_UNIVERSAL" ]; then
      UNI_BIN="$APP_BUNDLE_UNIVERSAL/Contents/MacOS/wizreader"
      if [ ! -x "$UNI_BIN" ]; then
        echo "[wizreader] 警告：未找到通用包产物 $UNI_BIN" >&2
        exit 1
      fi
      UNI_ARCHES="$(lipo -info "$UNI_BIN" 2>/dev/null | sed -n 's/.*are: //p')"
      case "$UNI_ARCHES" in
        *x86_64*arm64*|*arm64*x86_64*)
          echo "[wizreader] 通用包已就绪: $APP_BUNDLE_UNIVERSAL"
          echo "[wizreader] 架构: $UNI_ARCHES"
          ;;
        *)
          echo "[wizreader] 警告：产物不是双架构（lipo: ${UNI_ARCHES:-未知}）" >&2
          echo "[wizreader] 请检查是否缺少 rustup 目标: $UNIVERSAL_TARGETS" >&2
          exit 1
          ;;
      esac
    fi
    ;;
  cli)
    if [ ! -x "$CLI_BIN" ]; then
      echo "[wizreader] 首次使用，正在编译 wiz-cli（release）..." >&2
      (cd "$TAURI_DIR" && cargo build --release -p wiz-cli)
    fi
    exec "$CLI_BIN" "$@"
    ;;
  *)
    echo "用法: $0 {dev|app|build [universal] [dmg]|cli <args>}"
    echo "  build             默认只打宿主架构 .app（x86_64 或 arm64 单架构）"
    echo "  build universal   通用包 .app（x86_64 + arm64），可再加 dmg"
    echo "  build dmg         单独编译 DMG 发行镜像"
    exit 1
    ;;
esac
