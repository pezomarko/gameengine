#!/usr/bin/env bash
# Sound gate (docs/SOUND.md 7, PLAN.md 11.8 Phase 13): what is heard, proved on a machine
# without a sound device.
#
#   scripts/check-sound.sh              the tests (patches, mixer, cues); the offline walk rendered
#                                       to a WAV from the frame clock and read for silence and
#                                       steps; the arena with fifteen bots rendered the same way
#                                       and read for swings and hits; what the phase added to the
#                                       native binary
#   scripts/check-sound.sh --browser    also both browser builds in headless Chromium: the same
#                                       fight, the page's sound line (its context running after
#                                       the gate's click, cues started, the patches' hash the
#                                       same as the native build's) and what the phase added to
#                                       the wasm
#   scripts/check-sound.sh --browser --software   the same on Chromium's software GPU (CI)
# Environment: SECS (default 20), SKIP_BUILD=1, SKIP_TESTS=1, CHROME (default chromium),
# KEEP=DIR (keep logs and WAVs there).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
BROWSER=0; SOFTWARE=0
for a in "$@"; do
  case "$a" in
    --browser) BROWSER=1 ;;
    --software) SOFTWARE=1 ;;
    *) echo "check-sound: unknown argument $a"; exit 2 ;;
  esac
done
SECS="${SECS:-20}"
budget() { awk -v sec="[$1]" -v key="$2" '/^\[/{s=$1} s==sec && $1==key {print $3; exit}' budgets.toml; }
status=0
# A check whose value or bound is missing fails: an empty string is not a zero.
number() { [[ "$1" =~ ^-?[0-9]+(\.[0-9]+)?$ ]]; }
check_max() { # value max label
  local v="${1%.*}" m="$2" l="$3"
  if ! number "$1" || ! number "$2"; then echo "FAIL: $l: no number to check ('$1' against '$2')"; status=1
  elif (( v > m )); then echo "FAIL: $l $v exceeds $m"; status=1; else echo "OK: $l $v within $m"; fi
}
check_min() { # value min label
  local v="${1%.*}" m="$2" l="$3"
  if ! number "$1" || ! number "$2"; then echo "FAIL: $l: no number to check ('$1' against '$2')"; status=1
  elif (( v < m )); then echo "FAIL: $l $v is under $m"; status=1; else echo "OK: $l $v at least $m"; fi
}
tests() { # at-least-N label, cargo test arguments: the tests must exist and pass
  local want="$1" label="$2" out n; shift 2
  out="$(cargo test -q "$@" 2>&1)" || { echo "$out" | tail -20; echo "FAIL: $label"; status=1; return; }
  n="$(echo "$out" | sed -n 's/^test result: ok\. \([0-9]*\) passed.*/\1/p' | head -1)"
  if [[ -n "$n" ]] && (( n >= want )); then echo "OK: $label ($n tests)"
  else echo "FAIL: $label: $n tests ran, $want expected"; status=1; fi
}
# The field of a report line: `key=value`.
f() { echo "$1" | sed -n "s/.* $2=\([-0-9.A-Za-z]*\).*/\1/p"; }
# dB under full scale of the loudest 100 ms window of a WAV's span (a positive number,
# so that the budgets stay integers): 120 for digital silence.
below() { # file from to
  local line; line="$(python3 scripts/dev/wav-windows.py "$1" --from "$2" --to "$3")" || { echo ""; return; }
  echo "$line" | sed -n 's/.*max_db=\(-*[0-9.]*\).*/\1/p' | tr -d -
}

# 1. The tests (SOUND.md 7.1, 7.2).
if [[ "${SKIP_TESTS:-}" != 1 ]]; then
  tests 5 "the patches: within bounds, seamless loops, pinned hashes, memory" -p gm-client sound::synth::
  tests 8 "the mixer: ears, pitch, stealing without a click, fades, soft clip, no allocation" -p gm-client sound::mixer::
  tests 9 "the cues: every rule of SOUND.md 3 on scripted streams" -p gm-client sound::cues::
  tests 1 "the ears: gain by distance, pan by direction" -p gm-client sound::tests::
fi

# 2. The native client, rendering into a file from its frame clock (SOUND.md 7.3). The
#    client runs its real loop (scripts, a connection), so it needs a display: one of its
#    own (Xvfb says which number it took), on the software GPU, so that nothing of this
#    reaches a person's desktop and the gate runs where there is none.
command -v Xvfb >/dev/null || { echo "check-sound: needs Xvfb (the client runs windowed on a display of its own)"; exit 1; }
# (The client on its own first: built together with the servers its features unify with
# theirs and the binary differs by kilobytes from the one the size baseline measures.)
[[ "${SKIP_BUILD:-}" == 1 ]] || { cargo build --release -p gm-client --locked -q && cargo build --release -p gm-server -p gm-bot --locked -q; }
tmp="${KEEP:-$(mktemp -d)}"; mkdir -p "$tmp"; tmp="$(cd "$tmp" && pwd)"
rm -f "$tmp"/*.log "$tmp"/*.wav "$tmp"/*.der "$tmp"/*.json "$tmp/display"
pids=()
cleanup() {
  kill "${pids[@]:-}" 2>/dev/null || true
  [[ -n "${KEEP:-}" ]] || rm -rf "$tmp"
}
trap cleanup EXIT
Xvfb -displayfd 3 -screen 0 1280x720x24 -nolisten tcp 3> "$tmp/display" > "$tmp/xvfb.log" 2>&1 &
xvfb=$!; pids+=("$xvfb")
for _ in $(seq 1 100); do [[ -s "$tmp/display" ]] && break; kill -0 $xvfb 2>/dev/null || break; sleep 0.1; done
[[ -s "$tmp/display" ]] || { tail -5 "$tmp/xvfb.log"; echo "FAIL: Xvfb did not start"; exit 1; }
client=(env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET "DISPLAY=:$(head -1 "$tmp/display")" target/release/gm-client --software --report)

# 2a. The walk: a second standing, five running, a second standing.
if ! timeout 120 "${client[@]}" --offline --map assets/maps/built/town.bsp --script walk --seconds 7 \
     --sound-dump "$tmp/walk.wav" > "$tmp/walk.log" 2>&1; then
  tail -10 "$tmp/walk.log"; echo "FAIL: the offline walk did not run"; status=1
fi
walk="$(/usr/bin/grep -a '^sound:' "$tmp/walk.log" | tail -1 || true)"
echo "walk: $walk"
if [[ -z "$walk" || ! -s "$tmp/walk.wav" ]]; then
  echo "FAIL: the walk left no sound report or no WAV"; status=1
else
  check_min "$(f "$walk" steps)" "$(budget sound min_walk_steps)" "steps heard in a five-second run"
  check_max "$(f "$walk" dropped)" 0 "cues dropped in the walk"
  check_max "$(f "$walk" block_us_mean)" "$(budget sound max_block_us)" "microseconds per 512-frame block (release, the dump path)"
  # The spawn's landing is in the first third of a second; then the body stands.
  check_min "$(below "$tmp/walk.wav" 0.4 1.0)" "$(budget sound min_quiet_db)" "dB under full scale while standing (0.4-1.0 s)"
  check_min "$(below "$tmp/walk.wav" 6.3 7.0)" "$(budget sound min_quiet_db)" "dB under full scale while standing again (6.3-7.0 s)"
  check_max "$(below "$tmp/walk.wav" 1.0 6.0)" "$(budget sound max_steps_db)" "dB under full scale of the loudest window while running"
fi
native_hash="$(f "$walk" patches)"

# 2b. The fight: the arena with fifteen bots, the client by script.
port=$((20000 + RANDOM % 20000))
target/release/gm-server --map assets/maps/built/arena.bsp --listen 127.0.0.1:$port --cert-out "$tmp/cert.der" \
  --report-secs 5 > "$tmp/zone.log" 2>&1 &
pids+=("$!")
for _ in $(seq 1 50); do [[ -s "$tmp/cert.der" ]] && break; sleep 0.1; done
target/release/gm-bot --connect 127.0.0.1:$port --cert "$tmp/cert.der" --map assets/maps/built/arena.bsp \
  --bots 15 --secs $((SECS + 12)) --behaviour duelist --builds ironclad,blade,frostweaver,shade --teams 1,2 > "$tmp/bots.log" 2>&1 &
pids+=("$!")
sleep 2
if ! timeout $((SECS + 60)) "${client[@]}" --connect 127.0.0.1:$port --cert "$tmp/cert.der" --map assets/maps/built/arena.bsp \
     --name sound --build frostweaver --team 1 --script fight --third-person --seconds "$SECS" \
     --sound-dump "$tmp/fight.wav" > "$tmp/fight.log" 2>&1; then
  tail -10 "$tmp/fight.log"; echo "FAIL: the fight did not run"; status=1
fi
fight="$(/usr/bin/grep -a '^sound:' "$tmp/fight.log" | tail -1 || true)"
echo "fight: $fight"
if [[ -z "$fight" || ! -s "$tmp/fight.wav" ]]; then
  echo "FAIL: the fight left no sound report or no WAV"; status=1
else
  check_min "$(f "$fight" swings)" 1 "swings heard in the fight"
  check_min "$(( $(f "$fight" staggers) + $(f "$fight" hits) + $(f "$fight" hurts) ))" 1 "blows heard landing in the fight (staggers + hits + hurts)"
  check_min "$(f "$fight" launches)" 1 "launches heard in the fight (the own bolts at least)"
  check_max "$(f "$fight" dropped)" 0 "cues dropped in the fight"
  check_max "$(f "$fight" block_us_mean)" "$(budget sound max_block_us)" "microseconds per 512-frame block in the fight"
  check_max "$(below "$tmp/fight.wav" 0 "$SECS")" "$(budget sound max_steps_db)" "dB under full scale of the loudest window of the fight"
fi

# 2c. The native binary's growth since the phase, for the record (SOUND.md 8): later phases
#    grew it past what the sound alone added, so the total is the size gate's to hold
#    (scripts/check-binary-size.sh), not this one's.
echo "note: native client $(stat -c %s target/release/gm-client) bytes, $(( $(stat -c %s target/release/gm-client) - $(budget sound native_bytes_before) )) over the Phase 12 baseline (the sound added $(budget sound native_added_bytes) of them)"

[[ "$BROWSER" == 1 ]] || exit $status

# 3. Both browser builds (SOUND.md 7.4): the fight with a click three seconds in, which both
#    takes the pointer and allows the sound.
command -v node >/dev/null || { echo "check-sound: --browser needs node"; exit 1; }
[[ "${SKIP_BUILD:-}" == 1 && -f target/web/gm-client-webgpu_bg.wasm ]] || scripts/build-web.sh > target/web-build.log 2>&1 \
  || { tail -30 target/web-build.log; echo "FAIL: the web build"; exit 1; }
echo "note: WebGPU wasm $(stat -c %s target/web/gm-client-webgpu_bg.wasm) bytes, $(( $(stat -c %s target/web/gm-client-webgpu_bg.wasm) - $(budget sound webgpu_wasm_bytes_before) )) over Phase 12 (the sound added $(budget sound wasm_added_bytes) of them; the web gate holds the total)"
echo "note: WebGPU wasm $(stat -c %s target/web/gm-client-webgpu_bg.wasm) bytes of $(budget web max_webgpu_wasm_bytes); WebGL2 $(stat -c %s target/web/gm-client-webgl_bg.wasm)"
http=$((20000 + RANDOM % 20000))
mkdir -p "$tmp/web"; rm -f "$tmp/web"/*
ln -s "$ROOT"/target/web/* "$tmp/web/"; rm -f "$tmp/web/config.json"
echo '{"hub": null, "hub_cert_sha256": null, "dev": true}' > "$tmp/web/config.json"
(cd "$tmp/web" && exec python3 -m http.server "$http" --bind 127.0.0.1 >/dev/null 2>&1) &
pids+=("$!")
run() { # build name, query flag
  local build="$1" extra="$2" port=$((20000 + RANDOM % 20000)) server bots
  target/release/gm-server --map assets/maps/built/arena.bsp --listen 127.0.0.1:$port --cert-out "$tmp/cert-$build.der" \
    --web-listen 127.0.0.1:$((port + 1)) --web-info-out "$tmp/web-$build.json" --report-secs 5 > "$tmp/zone-$build.log" 2>&1 &
  server=$!
  sleep 1.5
  target/release/gm-bot --connect 127.0.0.1:$port --cert "$tmp/cert-$build.der" --map assets/maps/built/arena.bsp \
    --bots 15 --secs $((SECS + 12)) --behaviour duelist --builds ironclad,blade,frostweaver,shade --teams 1,2 > "$tmp/bots-$build.log" 2>&1 &
  bots=$!
  sleep 2
  local url hash
  url="$(sed -n 's/.*"url": "\([^"]*\)".*/\1/p' "$tmp/web-$build.json")"
  hash="$(sed -n 's/.*"cert_sha256": "\([^"]*\)".*/\1/p' "$tmp/web-$build.json")"
  local page="http://127.0.0.1:$http/?connect=$url&cert=$hash&map=arena&name=browser&build=frostweaver&team=1&script=fight&report=1&third-person=1&seconds=$SECS$extra"
  local soft=(); [[ "$SOFTWARE" == 1 ]] && soft=(--software)
  timeout $((SECS + 120)) node scripts/web-run.mjs --url "$page" --seconds "$SECS" --click-canvas 3 \
    ${CHROME:+--chrome "$CHROME"} "${soft[@]}" > "$tmp/browser-$build.log" 2>&1 || true
  kill $bots 2>/dev/null || true
  kill -INT $server 2>/dev/null || true; wait $server 2>/dev/null || true
  local done_line
  done_line="$(/usr/bin/grep -a '^GM-DONE' "$tmp/browser-$build.log" | tail -1 || true)"
  if [[ -z "$done_line" ]]; then
    tail -15 "$tmp/browser-$build.log"; echo "FAIL: the $build build did not finish its run"; status=1; return
  fi
  echo "$build: ${done_line##*sound: }"
  if /usr/bin/grep -aq '^\[ERROR\]\|^EXCEPTION' "$tmp/browser-$build.log"; then
    echo "FAIL: the $build build logged an error: $(/usr/bin/grep -a -m1 '^\[ERROR\]\|^EXCEPTION' "$tmp/browser-$build.log" | cut -c1-200)"; status=1
  else
    echo "OK: the $build build logged no error"
  fi
  [[ "$(f "$done_line" context)" == running ]] && echo "OK: $build: the audio context runs after the click" \
    || { echo "FAIL: $build: the audio context is '$(f "$done_line" context)' after the click"; status=1; }
  check_min "$(f "$done_line" started)" 1 "$build: cues the browser started"
  check_min "$(f "$done_line" swings)" 1 "$build: swings heard"
  check_max "$(f "$done_line" stolen)" "$(( $(f "$done_line" started) / 2 + 1 ))" "$build: cues that had to steal a chain (of $(f "$done_line" started) started)"
  if [[ -n "$native_hash" && "$(f "$done_line" patches)" == "$native_hash" ]]; then
    echo "OK: $build: the patches render to the same bytes as the native build's ($native_hash)"
  else
    echo "FAIL: $build: the patches' hash $(f "$done_line" patches) differs from the native build's $native_hash"; status=1
  fi
}
run webgpu ""
run webgl "&gl=1"
[[ -n "${KEEP:-}" ]] && echo "logs and WAVs kept in $tmp"
exit $status
