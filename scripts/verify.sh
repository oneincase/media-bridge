#!/usr/bin/env bash
# 一条命令跑完全部验证：本机测试 + 三平台交叉检查 + 真机自检。
#
# 用法：scripts/verify.sh [真机烟测]
#   不带参数    只跑测试与交叉检查（不影响系统音量、不出声音）
#   带任何参数  额外跑真机烟测（会短暂播放一段 1kHz 测试音来验证采集链路）
set -uo pipefail
cd "$(dirname "$0")/.."
fails=0
step() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }
check() { if [ "$1" -ne 0 ]; then printf '\033[31m✗ %s\033[0m\n' "$2"; fails=$((fails+1)); else printf '\033[32m✓ %s\033[0m\n' "$2"; fi; }

# 跑测试并汇总所有 test binary 的结果（只 tail 会漏掉别的 target）
run_tests() {
  local out
  out=$(cargo test "$@" 2>&1)
  echo "$out" | grep -E "^test result" | sed 's/^/  /'
  echo "$out" | grep -E "^test result" | awk '{p+=$4; f+=$6} END {printf "  合计：passed=%d failed=%d\n", p, f; exit (f>0 ? 1 : 0)}'
}

step "本机单元 + 集成测试（默认 feature）"
run_tests; check "$?" "cargo test"
step "全部 feature 测试"
run_tests --features embedded,online; check "$?" "cargo test --features embedded,online"

# 注意：这里**不能**用 --all-features 交叉检查 —— `online` 依赖的 ureq 会拉进 ring，
# 而 ring 的构建脚本需要目标平台的 C 工具链（在 macOS 上交叉编译 ring 到 Windows/Linux 会失败）。
# 这是构建工具链的限制，不是代码问题：在目标平台上原生构建 --all-features 是正常的。
for target in x86_64-pc-windows-msvc x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
  step "交叉检查 $target（默认 feature + embedded）"
  if ! rustup target list --installed | grep -q "^$target$"; then
    echo "（未安装该 target，跳过：rustup target add $target）"
    continue
  fi
  cargo check --target "$target" --all-targets 2>&1 | tail -1
  cargo check --target "$target" --all-targets --features embedded 2>&1 | tail -1
  check "$?" "cargo check --target $target"
done

if [ "$#" -gt 0 ]; then
  step "真机自检"
  cargo build --release >/dev/null 2>&1 || { check 1 "release 构建"; exit $fails; }
  BIN=./target/release/media-bridge
  $BIN diagnose
  check "$?" "diagnose"
  if [ "$(uname)" = "Darwin" ]; then
    step "采集链路（放 1kHz 测试音 6 秒，然后核对频谱段）"
    TONE=$(mktemp -t mb-tone).wav
    ffmpeg -y -v error -f lavfi -i "sine=frequency=1000:duration=6" -ar 48000 -ac 2 "$TONE"
    (afplay -v 0.3 "$TONE" >/dev/null 2>&1 &)
    sleep 1
    $BIN capture 2 --pcm --out /tmp/mb-verify.wav
    $BIN spectrum --wav /tmp/mb-verify.wav | grep -E "峰值段|帧 0"
    echo "（预期：峰值段 = 26，即 1kHz 在 16kHz 分析下的位置）"
    pkill -f afplay 2>/dev/null
    rm -f "$TONE"
  fi
fi

printf '\n'
if [ "$fails" -eq 0 ]; then printf '\033[32m全部通过\033[0m\n'; else printf '\033[31m%d 项失败\033[0m\n' "$fails"; fi
exit "$fails"
