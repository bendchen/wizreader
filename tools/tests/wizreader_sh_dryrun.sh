#!/usr/bin/env bash
# wizreader.sh 的干跑测试（不碰真实产物）：只验「参数装配 + 分支判定」，不跑真实编译。
#
# 原理：把 wizreader.sh 复制到临时目录 ⇒ 其 PROJ_DIR 随脚本位置走 ⇒ 脚本内所有
#       rm -rf / 构建路径都落在临时目录，绝不动真实 target/；再用桩 npm / rustup 记录
#       收到的参数并按场景造产物（真的用 clang + lipo 合成 fat 二进制来喂自验分支）。
# 用法：bash tools/tests/wizreader_sh_dryrun.sh     （全绿则退出码 0）
# 覆盖：build universal 正常路径 / 单架构产物拦截 / 缺 rust 目标自动安装 /
#       build 宿主路径不受影响 / universal+dmg 组合 / 指纹失败不拖垮构建判定
set -uo pipefail

DRY="${DRY:-${TMPDIR:-/tmp}/wizreader_sh_dryrun}"   # 工作目录（可被外部覆盖）
PROJ=$DRY/proj
BIN=$DRY/bin
TAURI=$PROJ/src-tauri
SRC="$(cd "$(dirname "$0")/../.." && pwd)/wizreader.sh"
PASS=0; FAIL=0
ok()   { echo "  ✓ $1"; PASS=$((PASS+1)); }
bad()  { echo "  ✗ $1"; FAIL=$((FAIL+1)); }
check(){ if [ "$2" = "$3" ]; then ok "$1"; else bad "$1（期望 [$3] 实得 [$2]）"; fi; }

rm -rf "$DRY"; mkdir -p "$BIN" "$PROJ" "$PROJ/src" "$TAURI/src"
echo "<html></html>" > "$PROJ/index.html"
echo "{}" > "$TAURI/tauri.conf.json"
echo "// s" > "$PROJ/src/main.ts"
echo "// s" > "$TAURI/src/lib.rs"

cat > "$DRY/t.c" <<'EOF'
int main(void){return 0;}
EOF
clang -arch x86_64 -o "$DRY/t_x86" "$DRY/t.c" 2>/dev/null
clang -arch arm64  -o "$DRY/t_arm" "$DRY/t.c" 2>/dev/null
lipo -create -output "$DRY/fat" "$DRY/t_x86" "$DRY/t_arm" 2>/dev/null
lipo -info "$DRY/fat" >/dev/null 2>&1 || { echo "无法合成测试用 fat 二进制，退出"; exit 1; }

cat > "$BIN/npm" <<EOF
#!/usr/bin/env bash
echo "npm \$*" >> "$DRY/calls.log"
mode="\$(cat "$DRY/mode" 2>/dev/null || echo none)"
UNI="$DRY/proj/src-tauri/target/universal-apple-darwin/release/bundle/macos/wizreader.app/Contents/MacOS"
case "\$mode" in
  fat)  mkdir -p "\$UNI" && cp "$DRY/fat" "\$UNI/wizreader" ;;
  thin) mkdir -p "\$UNI" && cp "$DRY/t_x86" "\$UNI/wizreader" ;;
  *)    : ;;
esac
exit 0
EOF
chmod +x "$BIN/npm"

cat > "$BIN/rustup" <<EOF
#!/usr/bin/env bash
echo "rustup \$*" >> "$DRY/calls.log"
if [ "\$1" = "target" ] && [ "\${2:-}" = "list" ]; then
  [ "\$(cat "$DRY/mode" 2>/dev/null)" = "noTargets" ] && exit 0
  echo "x86_64-apple-darwin"; echo "aarch64-apple-darwin"
fi
exit 0
EOF
chmod +x "$BIN/rustup"

run() { # run <mode> <args...>
  echo "$1" > "$DRY/mode"; shift
  : > "$DRY/calls.log"
  ( cd "$PROJ" && PATH="$BIN:$HOME/.cargo/bin:$PATH" ./wizreader.sh "$@" ) > "$DRY/out.log" 2>&1
  echo "$?" > "$DRY/exit"
}

cp "$SRC" "$PROJ/wizreader.sh"; chmod +x "$PROJ/wizreader.sh"

echo "=== T1 build universal（目标齐备 + 双架构产物）==="
run fat build universal
grep -q -- 'run tauri build -- --target universal-apple-darwin --bundles app' "$DRY/calls.log" && ok "npm 收到 --target universal-apple-darwin --bundles app（含 -- 分隔符）" || bad "npm 参数不对：$(cat "$DRY/calls.log")"
grep -q 'rustup target add' "$DRY/calls.log" && bad "目标已备齐却仍触发安装" || ok "目标齐备时不触发安装"
check "退出码 0" "$(cat "$DRY/exit")" "0"
grep -q '通用包已就绪' "$DRY/out.log" && ok "打印「通用包已就绪」" || bad "缺少就绪提示：$(tail -3 "$DRY/out.log")"
grep -q '架构: x86_64 arm64' "$DRY/out.log" && ok "自验打印双架构" || bad "未打印双架构"
grep -q '已记录源码指纹' "$DRY/out.log" && ok "指纹正常记录" || bad "指纹记录异常"

echo "=== T2 build universal（产物单架构 ⇒ 必须拦住）==="
run thin build universal
check "退出码 1" "$(cat "$DRY/exit")" "1"
grep -q '不是双架构' "$DRY/out.log" && ok "识别出单架构并告警" || bad "未拦截单架构产物：$(tail -3 "$DRY/out.log")"

echo "=== T3 build universal（rustup 缺目标 ⇒ 自动安装）==="
run noTargets build universal
grep -q 'rustup target add aarch64-apple-darwin x86_64-apple-darwin' "$DRY/calls.log" && ok "调用 rustup target add（两目标、顺序正确）" || bad "未安装或参数不对：$(cat "$DRY/calls.log")"

echo "=== T4 build（宿主包：不带 --target、不做通用自验）==="
run none build
grep -q -- 'run tauri build -- --bundles app' "$DRY/calls.log" && ok "宿主构建隐式补 --bundles app" || bad "宿主构建参数不对：$(cat "$DRY/calls.log")"
grep -q -- '--target' "$DRY/calls.log" && bad "宿主构建不该带 --target" || ok "宿主构建不带 --target"
grep -q '通用包' "$DRY/out.log" && bad "宿主构建不该走通用自验" || ok "宿主构建跳过通用自验"

echo "=== T5 build universal dmg（可叠加）==="
run none build universal dmg
grep -q -- '--target universal-apple-darwin --bundles dmg' "$DRY/calls.log" && ok "universal dmg 组合正确" || bad "组合参数不对：$(cat "$DRY/calls.log")"

echo "=== T6 指纹失败不得判定构建失败（源码目录缺失场景）==="
rm -rf "$PROJ/src" "$PROJ/index.html"
run fat build universal
check "退出码仍为 0" "$(cat "$DRY/exit")" "0"
grep -q '指纹记录失败' "$DRY/out.log" && ok "指纹失败给出警告" || bad "未给出指纹失败警告"
grep -q '通用包已就绪' "$DRY/out.log" && ok "产物判定不受指纹影响" || bad "指纹拖累了产物判定"

echo
echo "结果：通过 $PASS 项，失败 $FAIL 项"
[ "$FAIL" -eq 0 ]
