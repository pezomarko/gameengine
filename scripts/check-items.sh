#!/usr/bin/env bash
# Items gate (docs/ITEMS.md 7, PLAN.md 11.8 Phase 11): what a character wears, what it does
# in the zone's hits, and a stall bought from by somebody who is not a person.
#
#   scripts/check-items.sh             the term in the damage pipeline and in a simulated
#                                      fight, the item content and its words, the screens
#                                      (every page whole at every window size), the players'
#                                      messages; with GM_TEST_DATABASE_URL also the hub (worn
#                                      items refused by everything that moves or destroys,
#                                      the storm with wearing in it, buying only through a
#                                      zone) and a zone with clients driven by hand (a stall
#                                      bought from at the counter only, the fight lock, the
#                                      zone's hits before and after)
#   scripts/check-items.sh --desktop   also the desktop client on a display of its own (Xvfb,
#                                      the software GPU): a bot keeps a stall, an operator
#                                      hands it swords and kits, gives a new character coin and
#                                      stands it at the counter; by UI script the character
#                                      looks at the stall, buys a sword and a stack of kits,
#                                      wears the sword, drags the kits onto the item bar's
#                                      second cell (LOOK.md 3.2), finds the sword worn through
#                                      the menu too, and presses F, now an empty cell (MODES.md
#                                      11.3: the HUD says why nothing was used). Then the same
#                                      keys from a real keyboard (xdotool), F and 8 among them.
#   scripts/check-items.sh --browser   the same purchase in both browser builds in headless
#                                      Chromium (--software: on Chromium's software GPU); the
#                                      WebGL2 buyer is a frostweaver (the RPG mode), who buys
#                                      the kits alone and presses F at full health
# --desktop and --browser need GM_TEST_DATABASE_URL (a Postgres this run wipes).
# Environment: SKIP_BUILD=1, SKIP_TESTS=1, CHROME (default chromium), KEEP=DIR (keep logs
# and screenshots there).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
DESKTOP=0; BROWSER=0; SOFTWARE="${SOFTWARE:-0}"
for a in "$@"; do
  case "$a" in
    --desktop) DESKTOP=1 ;;
    --browser) BROWSER=1 ;;
    --software) SOFTWARE=1 ;;
    *) echo "check-items: unknown argument $a"; exit 2 ;;
  esac
done
budget() { awk -v sec="[$1]" -v key="$2" '/^\[/{s=$1} s==sec && $1==key {print $3; exit}' budgets.toml; }
status=0
ok()   { echo "OK: $*"; }
fail() { echo "FAIL: $*"; status=1; }
number() { [[ "$1" =~ ^[0-9]+(\.[0-9]+)?$ ]]; }
atmost() { # value max label
  if ! number "$1" || ! number "$2"; then fail "$3: no number to check ('$1' against '$2')"
  elif (( ${1%.*} > $2 )); then fail "$3 $1 exceeds $2"; else ok "$3 $1 within $2"; fi
}
atleast() {
  if ! number "$1" || ! number "$2"; then fail "$3: no number to check ('$1' against '$2')"
  elif (( ${1%.*} < $2 )); then fail "$3 $1 below $2"; else ok "$3 $1 at least $2"; fi
}
equal() { if [[ "$1" == "$2" ]]; then ok "$3: $1"; else fail "$3: '$1', not '$2'"; fi; }
tests() { # minimum, label, cargo test arguments: the tests must exist and pass
  local min="$1" label="$2" out passed; shift 2
  out="$(cargo test -q "$@" 2>&1)" || { echo "$out" | tail -25; fail "$label"; return; }
  passed="$(echo "$out" | sed -n 's/^test result: ok\. \([0-9]*\) passed.*/\1/p' | sort -n | tail -1)"
  atleast "${passed:-0}" "$min" "$label: tests passed"
  # What a test measured and said in a line of its own.
  echo "$out" | /usr/bin/grep -ao "items: .*\|storm: .*" || true
}

# 1. Without a display.
if [[ "${SKIP_TESTS:-}" != 1 ]]; then
  tests 3 "the gear term: every type by its own edge, a place for half, pulses for none; a swing, its windup, a bolt in flight, an area, a riposte, a burn, a respawn, the fight lock's edges" -p gm-core gear
  tests 4 "the item content: what each layer is for, what an item is called, the cap" -p gm-content items::
  tests 6 "the inventory, the storage, the price and the stall, against a scripted hub and zone" -p gm-client bag::
  tests 2 "the wire: the handshake's bytes across versions, the reach of a stall" -p gm-net control::tests::
  tests 2 "the zone's gates: a refusal of its own costs no second, a flood is dropped" -p gm-server --lib request
  tests 1 "the players' messages are the hub's own requests" -p gm-hub-proto player::
  if [[ -n "${GM_TEST_DATABASE_URL:-}" ]]; then
    tests 15 "the economy's transactions (the storm puts things on and offers them at once)" -p gm-hub --test economy -- --nocapture
    tests 1 "the economy over the wire: a stall seen from anywhere, bought from through its zone" -p gm-hub --test economy_protocol
    tests 1 "what is worn at the hub: through the zone only, refused by every mover, carried by a claim, limited by account" -p gm-hub --test items
    tests 1 "a zone and clients by hand: the counter, the fight lock, the hits before and after" -p gm-server --test items -- --nocapture
  else
    echo "note: GM_TEST_DATABASE_URL is not set: the hub's and the zone's side of items was not run"
  fi
fi
[[ "$DESKTOP" == 1 || "$BROWSER" == 1 ]] || exit $status

# 2. A hub, a town, and a keeper with a stall in it.
: "${GM_TEST_DATABASE_URL:?--desktop and --browser need GM_TEST_DATABASE_URL (a Postgres this run wipes)}"
[[ "${SKIP_BUILD:-}" == 1 ]] || cargo build --release -p gm-hub -p gm-server -p gm-bot -p gm-client --locked -q
tmp="${KEEP:-$(mktemp -d)}"; mkdir -p "$tmp"; tmp="$(cd "$tmp" && pwd)"; mkdir -p "$tmp/elsewhere"
# (A directory that is kept may hold an earlier run's files: none of them is this run's.)
rm -f "${tmp:?}/hub.der" "${tmp:?}/hub-web.json" "${tmp:?}"/*.log "${tmp:?}/display"
pids=()
cleanup() {
  kill "${pids[@]:-}" 2>/dev/null || true
  [[ -n "${KEEP:-}" ]] || rm -rf "${tmp:?}"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
plain() { sed 's/\x1b\[[0-9;]*m//g' "$1"; }
show() { echo "--- the end of $(basename "$1"):"; plain "$1" 2>/dev/null | tail -"${2:-12}" || true; }
started() { # pid, name, log
  if ! kill -0 "$1" 2>/dev/null; then show "$3"; echo "FAIL: $2 did not start"; exit 1; fi
}
# The time (epoch seconds, to the millisecond) at which a log first shows a line, while
# the program that writes it lives.
stamp() { # log, text, pid
  for _ in $(seq 1 6000); do
    if /usr/bin/grep -aqF "$2" "$1" 2>/dev/null; then date +%s.%3N; return 0; fi
    kill -0 "$3" 2>/dev/null || return 1
    sleep 0.05
  done
  return 1
}
since() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.1f", b - a }'; }
hp=$((20000 + RANDOM % 20000))
hub=(target/release/gm-hub --database-url "$GM_TEST_DATABASE_URL")
"${hub[@]}" --wipe --listen 127.0.0.1:$hp --cert-out "$tmp/hub.der" \
  --key "$tmp/hub.key" --zone-secret "$tmp" --models-dir "$tmp/models" --auth-per-minute 100000 --start-zone town \
  --web-listen 127.0.0.1:$((hp + 1)) --web-info-out "$tmp/hub-web.json" > "$tmp/hub.log" 2>&1 &
hub_pid=$!; pids+=("$hub_pid")
for _ in $(seq 1 150); do [[ -s "$tmp/hub.der" && -s "$tmp/hub-web.json" ]] && break; kill -0 $hub_pid 2>/dev/null || break; sleep 0.1; done
[[ -s "$tmp/hub.der" && -s "$tmp/hub-web.json" ]] || { show "$tmp/hub.log"; echo "FAIL: the hub did not come up"; exit 1; }
sleep 0.5
link=(--hub 127.0.0.1:$hp --hub-cert "$tmp/hub.der")
target/release/gm-server --map assets/maps/built/town.bsp --listen 127.0.0.1:$((hp + 2)) --cert-out "$tmp/town.der" \
  "${link[@]}" --zone-id town --zone-secret "$tmp" --hz 20 --report-secs 5 \
  --web-listen 127.0.0.1:$((hp + 3)) > "$tmp/town.log" 2>&1 &
town_pid=$!; pids+=("$town_pid")
running=0
for _ in $(seq 1 150); do
  if [[ -s "$tmp/town.der" ]] && /usr/bin/grep -aq "zone running" "$tmp/town.log"; then running=1; break; fi
  kill -0 $town_pid 2>/dev/null || break
  sleep 0.1
done
started $hub_pid "the hub" "$tmp/hub.log"
started $town_pid "the town" "$tmp/town.log"
[[ "$running" == 1 ]] || { show "$tmp/town.log"; echo "FAIL: the town did not come up"; exit 1; }
# The keeper walks to the first tile of the market, opens its stall there, says where it
# stands, and puts up for sale whatever comes to its hands, at one gold and twenty silver.
# A buyer is given two listings' worth and thirty silver over.
PRICE=120; PURSE=270
target/release/gm-bot "${link[@]}" --user keeper@bots.test --password keeper-password --register --character Keeper \
  --zone town --bots 1 --stalls 1 --sell-at $PRICE --secs 900 --behaviour stroll --maps-dir assets/maps/built \
  > "$tmp/keeper.log" 2>&1 &
keeper_pid=$!; pids+=("$keeper_pid")
stamp "$tmp/keeper.log" "stall stands" $keeper_pid > /dev/null || { show "$tmp/keeper.log"; echo "FAIL: the keeper opened no stall"; exit 1; }
stands="$(plain "$tmp/keeper.log" | /usr/bin/grep -a "stall stands" | tail -1)"
field() { echo "$stands" | sed -n "s/.* $1=\([-0-9.]*\).*/\1/p"; }
# In front of the counter and facing it: 72 units from the middle of the tile, the way the
# keeper looks, a hair above the ground.
spot="$(awk -v x="$(field x)" -v y="$(field y)" -v z="$(field z)" -v yaw="$(field yaw)" 'BEGIN {
  r = yaw * 3.14159265358979 / 180; printf "%.1f,%.1f,%.1f %.1f", x + cos(r) * 72, y + sin(r) * 72, z + 25, (yaw + 180) % 360 }')"
ok "the keeper's stall stands at $(field x) $(field y) $(field z); a buyer is put at $spot"
# An operator hands the keeper three swords of iron and oak (one for each buyer below):
# through the ledger, and only what the content knows.
for _ in 1 2 3; do
  "${hub[@]}" --grant-item Keeper sword core/iron,frame/oak > "$tmp/grant.log" 2>&1 || { show "$tmp/grant.log"; fail "a sword for the keeper"; }
done
if "${hub[@]}" --grant-item Keeper cuirass core/iron,frame/oak,catalyst/ember > "$tmp/grant.log" 2>&1; then
  fail "a cuirass was granted with a catalyst it has no room for"
elif plain "$tmp/grant.log" | /usr/bin/grep -aq "a cuirass takes no catalyst"; then
  ok "the operator's hand obeys the content: $(plain "$tmp/grant.log" | tail -1 | sed 's/^Error: //')"
else
  show "$tmp/grant.log"; fail "the grant of a cuirass with a catalyst failed for another reason"
fi
listings() { # n: wait until the keeper has listed n things
  local n=0
  for _ in $(seq 1 150); do
    n="$(plain "$tmp/keeper.log" | /usr/bin/grep -ac " listed " || true)"
    [[ "$n" -ge "$1" ]] && break
    sleep 0.1
  done
  echo "$n"
}
atleast "$(listings 3)" 3 "swords the keeper put up for sale"
# And three stacks of three kits (MODES.md 11.1): a stack never splits and two stacks of a
# kind merge in a holder, so each is listed (moved to the stall) before the next is granted.
for i in 1 2 3; do
  "${hub[@]}" --grant-item Keeper kit 3 > "$tmp/grant.log" 2>&1 || { show "$tmp/grant.log"; fail "kits for the keeper"; }
  atleast "$(listings $((3 + i)))" $((3 + i)) "listings with stack $i of kits"
done
# What the operator does for a buyer made by a client: coin, and a place at the counter.
provide() { # character
  local out="" placed=0
  for _ in $(seq 1 50); do
    # The zone puts a character away a moment after its client left; until then the hub
    # refuses to move it (the command's own exit status says which).
    # shellcheck disable=SC2086
    if out="$("${hub[@]}" --place "$1" town ${spot% *} "${spot#* }" 2>&1)"; then placed=1; break; fi
    sleep 0.2
  done
  [[ "$placed" == 1 ]] || { echo "$out" | tail -3; fail "$1 was not placed at the counter"; return 1; }
  "${hub[@]}" --grant-coin "$1" $PURSE > "$tmp/grant.log" 2>&1 || { show "$tmp/grant.log"; fail "coin for $1"; return 1; }
}
# What a buyer does once it stands in the game: looks, buys a sword and the kits, wears the
# sword, drags the kits onto the item bar's second cell (LOOK.md 3.2: the hub keeps it),
# and finds it worn through the menu as well; then presses F, the first cell, now empty,
# which uses nothing and says so (MODES.md 11.3). One line a step: each waits for what it
# needs.
KIT_ROW="kit ×3  heals 300, used with F"
purchase() {
  cat <<EOS
wait screen game
expect "Keeper's stall  E look"
say at the stall
key E
wait screen stall
expect "Keeper's stall"
expect "2 g 70 s"
expect "a weapon, 40 of 250"
expect "you wear nothing in its place"
click "sword  slash +2.0%"
click Buy
expect "bought: it is in the inventory"
expect "1 g 50 s"
click "$KIT_ROW"
click Buy
expect "bought: it is in the inventory"
expect "30 s"
key Escape
wait screen game
key I
wait screen inventory
expect "30 s"
expect "$KIT_ROW"
drag "$KIT_ROW" "bar 8"
expect "the bar is set"
click "sword  slash +2.0%"
click Wear
expect "sword  slash +2.0%  worn"
say worn
key Escape
wait screen game
key Escape
wait screen menu
click Inventory
wait screen inventory
expect "sword  slash +2.0%  worn"
click Storage
wait screen storage
expect "nothing is stored"
click Back
wait screen inventory
key Escape
wait screen game
key F
say pressed F
sleep 1
EOS
}
# A buyer of the RPG mode (MODES.md 5), whose build wears no sword: the kits alone, and F.
purchase_kits() {
  cat <<EOS
wait screen game
expect "Keeper's stall  E look"
say at the stall
key E
wait screen stall
expect "Keeper's stall"
expect "2 g 70 s"
click "$KIT_ROW"
click Buy
expect "bought: it is in the inventory"
expect "1 g 50 s"
key Escape
wait screen game
key I
wait screen inventory
expect "$KIT_ROW"
key Escape
wait screen game
key F
say pressed F
sleep 1
EOS
}
# A press of an item key that used nothing (MODES.md 11.3): the client says why; the kits
# are kept.
refused() { # log, label, key, why
  if /usr/bin/grep -aq "item $3: $4" "$1"; then ok "$2: $3 used nothing ($4) and the HUD said so"
  else fail "$2: no word of $3 refused ($4)"; fi
}
# What the client itself called an error: a browser shows it in its console and plays on
# (the WebGL build drew no town for two phases, and said so there).
quiet() { # log, label
  local first; first="$(/usr/bin/grep -a -m1 '^\[ERROR\]\|^EXCEPTION' "$1" || true)"
  if [[ -z "$first" ]]; then ok "$2: the client logged no error"; else fail "$2: the client logged an error: ${first:0:200}"; fi
}
# What the hub and the zone hold after `swords` swords and `kits` stacks of kits were bought.
books() { # swords, kits
  local line; line="$("${hub[@]}" --audit 2>/dev/null | /usr/bin/grep -a "^audit:" || true)"
  [[ -n "$line" ]] || { fail "the hub's audit said nothing"; return; }
  echo "$line"
  equal "$(echo "$line" | sed -n 's/.* unsound=\([0-9-]*\).*/\1/p')" 0 "balances that disagree with the ledger, worn items astray"
  equal "$(echo "$line" | sed -n 's/.* worn=\([0-9]*\).*/\1/p')" "$1" "items worn"
  equal "$(echo "$line" | sed -n 's/.* stall_sale=\([0-9]*\).*/\1/p')" "$((($1 + $2) * PRICE))" "coin the keeper was paid"
  local told; told="$(plain "$tmp/town.log" | /usr/bin/grep -a " gear " | /usr/bin/grep -ac 'dealt=\[40, 0, 0, 0, 0, 0, 0, 0\]' || true)"
  atleast "$told" "$1" "times the zone applied a sword's edge on its own kind (40 per mille of slash)"
  equal "$(plain "$tmp/town.log" | /usr/bin/grep -ac 'item used' || true)" 0 "items used (every press was at full health or of an empty cell)"
}

desktop() {
  command -v Xvfb >/dev/null || { fail "--desktop needs Xvfb"; return; }
  command -v xdotool >/dev/null || { fail "--desktop needs xdotool"; return; }
  Xvfb -displayfd 3 -screen 0 1280x720x24 -nolisten tcp 3> "$tmp/display" > "$tmp/xvfb.log" 2>&1 &
  local xvfb=$!; pids+=("$xvfb")
  for _ in $(seq 1 100); do [[ -s "$tmp/display" ]] && break; kill -0 $xvfb 2>/dev/null || break; sleep 0.1; done
  [[ -s "$tmp/display" ]] || { show "$tmp/xvfb.log"; fail "Xvfb did not start"; return; }
  local disp; disp="$(head -1 "$tmp/display")"
  local on=(env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET "DISPLAY=:$disp")
  local client=("$ROOT/target/release/gm-client" --software --settings "$tmp/settings.toml")
  printf 'hub = "127.0.0.1:%s"\nhub_cert = "%s"\n' "$hp" "$tmp/hub.der" > "$tmp/settings.toml"
  local login='wait screen login
expect "email: buyer@gm.test"
field password
type a long password
key Enter
wait screen characters
click Play'

  # 3. A new account and a character, by script; then the operator's hand.
  cat > "$tmp/new.ui" <<'EOS'
wait screen login
click "New account"
field email
type buyer@gm.test
field password
type a long password
field "password again"
type a long password
click "Create account"
wait screen new character
field name
type Buyer
click blade
click Create
wait screen characters
expect "Buyer  blade  new"
quit
EOS
  if (cd "$tmp/elsewhere" && "${on[@]}" timeout 120 "${client[@]}" --ui-script "$tmp/new.ui") > "$tmp/new.log" 2>&1; then
    ok "a new account and a character called Buyer, by script"
  else
    show "$tmp/new.log"; fail "the buyer was not made"; return
  fi
  quiet "$tmp/new.log" "the desktop client, making the buyer"
  provide Buyer || return

  # 4. The purchase, by script: the stall, Buy, the inventory, Wear, the menu's way in.
  { echo "$login"; purchase; echo quit; } > "$tmp/buy.ui"
  local pid t_stall t_worn
  (cd "$tmp/elsewhere" && exec "${on[@]}" timeout 180 "${client[@]}" --ui-script "$tmp/buy.ui") > "$tmp/buy.log" 2>&1 &
  pid=$!; pids+=("$pid")
  t_stall="$(stamp "$tmp/buy.log" "ui-script: at the stall" $pid)" || t_stall=""
  t_worn="$(stamp "$tmp/buy.log" "ui-script: worn" $pid)" || t_worn=""
  if wait $pid && /usr/bin/grep -aq '^ui-script: ok' "$tmp/buy.log"; then
    ok "by script: the stall at the counter, a sword and three kits bought at the prices shown, the purse 2 g 40 s lighter, the sword worn, the same through the menu, the storage empty"
  else
    show "$tmp/buy.log"; fail "the purchase by script did not reach its end"; return
  fi
  quiet "$tmp/buy.log" "the desktop client, at the stall and in the inventory"
  refused "$tmp/buy.log" "the desktop client, by script" F "nothing to use"
  if [[ -n "$t_stall" && -n "$t_worn" ]]; then
    atmost "$(since "$t_stall" "$t_worn")" "$(budget items max_secs_stall_to_worn)" "seconds from E at the stall to the sword being worn (software GPU, by script)"
  else
    fail "the purchase was not timed"
  fi
  books 1 1

  # 5. The keys from a real keyboard: what the window system delivers, not what a script
  #    hands in. The script only says when the screen is ready.
  { echo "$login"; cat <<EOS
wait screen game
expect "Keeper's stall  E look"
say press e
wait screen stall
expect "Keeper's stall"
say press escape
wait screen game
say press i
wait screen inventory
expect "sword  slash +2.0%  worn"
expect "$KIT_ROW"
say press escape again
wait screen game
say press f
sleep 1
say press 8
sleep 1
quit
EOS
  } > "$tmp/keys.ui"
  (cd "$tmp/elsewhere" && exec "${on[@]}" timeout 120 "${client[@]}" --ui-script "$tmp/keys.ui") > "$tmp/keys.log" 2>&1 &
  pid=$!; pids+=("$pid")
  local x=("${on[@]}" xdotool)
  told() { stamp "$tmp/keys.log" "ui-script: $1" $pid > /dev/null; }
  keys() {
    told "press e" || return 1
    # No window manager is here to give the window the keyboard.
    "${x[@]}" windowfocus "$("${x[@]}" search --name '^gamengine' | head -1)"
    sleep 0.2
    "${x[@]}" key e
    told "press escape" || return 1
    "${x[@]}" key Escape
    told "press i" || return 1
    "${x[@]}" key i
    told "press escape again" || return 1
    "${x[@]}" key Escape
    told "press f" || return 1
    "${x[@]}" key f
    told "press 8" || return 1
    "${x[@]}" key 8
  }
  if keys && wait $pid && /usr/bin/grep -aq '^ui-script: ok' "$tmp/keys.log"; then
    ok "by a real keyboard: E opens the stall the body stands at, Escape closes it, I opens the inventory, F and 8 ask for the item cells"
  else
    kill $pid 2>/dev/null || true
    show "$tmp/keys.log"; fail "the run by real keys did not reach its end"
  fi
  refused "$tmp/keys.log" "the desktop client, by a real keyboard" F "nothing to use"
  refused "$tmp/keys.log" "the desktop client, by a real keyboard" 8 "at full health"
}

browser() {
  command -v node >/dev/null || { fail "--browser needs node"; return; }
  [[ "${SKIP_BUILD:-}" == 1 && -f target/web/gm-client-webgpu_bg.wasm ]] || scripts/build-web.sh > target/web-build.log 2>&1 \
    || { tail -30 target/web-build.log; fail "the web build"; return; }
  local http=$((20000 + RANDOM % 20000)) hub_url hub_hash
  mkdir -p "$tmp/web"; rm -f "${tmp:?}/web"/*
  ln -s "$ROOT"/target/web/* "$tmp/web/"; rm -f "${tmp:?}/web/config.json"
  hub_url="$(sed -n 's/.*"url": "\([^"]*\)".*/\1/p' "$tmp/hub-web.json")"
  hub_hash="$(sed -n 's/.*"cert_sha256": "\([^"]*\)".*/\1/p' "$tmp/hub-web.json")"
  echo "{\"hub\": \"$hub_url\", \"hub_cert_sha256\": \"$hub_hash\", \"dev\": true}" > "$tmp/web/config.json"
  (cd "$tmp/web" && exec python3 -m http.server "$http" --bind 127.0.0.1 >/dev/null 2>&1) &
  pids+=("$!")
  local soft=(); [[ "$SOFTWARE" == 1 ]] && soft=(--software)
  local build name preset script query extra swords=0 kits=0
  # (The desktop's purchase, when it ran and went through, is in the books too.)
  [[ "$DESKTOP" == 1 ]] && /usr/bin/grep -aq '^ui-script: ok' "$tmp/buy.log" 2>/dev/null && { swords=1; kits=1; }
  page() { # log, script, more arguments of web-run
    local log="$1"; query="$(node -e 'process.stdout.write(encodeURIComponent(process.argv[1]))' "$2")"; shift 2
    timeout 240 node scripts/web-run.mjs --url "http://127.0.0.1:$http/?ui-script=$query$extra" --seconds 120 \
      --login "$build@gm.test" --password "a long password" "$@" ${CHROME:+--chrome "$CHROME"} "${soft[@]}" > "$log" 2>&1 || true
    /usr/bin/grep -aq '^GM-DONE ui-script: ok' "$log"
  }
  # The WebGPU buyer is a blade (the action mode), the WebGL2 buyer a frostweaver (the RPG
  # mode, whose keys are read another way: MODES.md 5.5), who wears no sword.
  for build in webgpu webgl; do
    name="Buyer$build"
    extra=""; preset=blade; [[ "$build" == webgl ]] && { extra="&gl=1"; preset=frostweaver; }
    # The form is the page's (filled by the browser's own input events); the character is
    # made on the canvas, then the operator's hand, then the purchase.
    script="$(printf '%s\n' 'wait screen new character' 'field name' "type $name" "click $preset" 'click Create' \
      'wait screen characters' "expect \"$name  $preset  new\"" 'quit')"
    if page "$tmp/browser-$build-new.log" "$script" --register; then
      ok "$build: a new account by the page's form and a character called $name"
    else
      show "$tmp/browser-$build-new.log"; fail "the $build build did not make its buyer"; continue
    fi
    quiet "$tmp/browser-$build-new.log" "$build, making its buyer"
    provide "$name" || continue
    if [[ "$preset" == blade ]]; then
      script="$(printf '%s\n' 'wait screen characters' 'click Play'; purchase; echo quit)"
    else
      script="$(printf '%s\n' 'wait screen characters' 'click Play'; purchase_kits; echo quit)"
    fi
    if page "$tmp/browser-$build.log" "$script" --screenshot "$tmp/browser-$build.png"; then
      if [[ "$preset" == blade ]]; then
        ok "$build: the stall at the counter, a sword and the kits bought, the sword worn, the same through the menu"
      else
        ok "$build: a $preset at the counter, the kits bought and in the inventory"
      fi
    else
      show "$tmp/browser-$build.log"; fail "the $build build did not reach the end of its purchase"; continue
    fi
    if /usr/bin/grep -aq "^GM-BUILD $build" "$tmp/browser-$build.log"; then ok "$build: that build ran"; else fail "$build: another build ran"; fi
    quiet "$tmp/browser-$build.log" "$build, in the town"
    if [[ "$preset" == blade ]]; then
      refused "$tmp/browser-$build.log" "$build, a $preset" F "nothing to use"
    else
      refused "$tmp/browser-$build.log" "$build, a $preset" F "at full health"
    fi
    [[ "$preset" == blade ]] && swords=$((swords + 1))
    kits=$((kits + 1))
  done
  books "$swords" "$kits"
}

[[ "$DESKTOP" == 1 ]] && desktop
[[ "$BROWSER" == 1 ]] && browser
[[ -n "${KEEP:-}" ]] && echo "logs and screenshots kept in $tmp"
exit $status
