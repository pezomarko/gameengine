# Building and running

## Prerequisites

- Rust 1.97.0 with `clippy` and `rustfmt`: `rust-toolchain.toml` names it and rustup installs it on the first build. CI builds with the same one (ci.yml); it moves on purpose, after a newer clippy's lints are met, since warnings are errors there.
- A Vulkan driver. Linux: Arch `vulkan-radeon` / `vulkan-intel` / `vulkan-swrast`
  (software), Debian/Ubuntu `mesa-vulkan-drivers`. Without an ICD the client reports
  "no compatible GPU adapter"; `--software` selects the CPU adapter explicitly.
- ALSA's headers to build the client's sound device (Arch `alsa-lib`, Debian/Ubuntu
  `libasound2-dev`); at run time `libasound` (present wherever there is sound). A machine
  without a sound device plays silently (SOUND.md 4). The client opens ALSA's `default`:
  on a machine where that is a bare codec held by the sound server, or the wrong card (an
  HDMI output with no analogue device, as on the development machine: "unable to open
  slave"), either set `ALSA_CARD=1` (the card's number from `/proc/asound/cards`) or
  install the plugin that routes ALSA's default to the server (`pipewire-alsa`, or
  `alsa-plugins` for PulseAudio; Debian/Ubuntu `pipewire-alsa` / `libasound2-plugins`).
- `curl`, `unzip` (or `python3`) for `scripts/fetch-ericw-tools.sh`.
- Optional: [TrenchBroom](https://trenchbroom.github.io/) to edit maps.
- For the browser build: `rustup target add wasm32-unknown-unknown`; Node 22+ and a Chromium
  only for the browser gate.

## First build

```sh
scripts/fetch-ericw-tools.sh                                   # pinned ericw-tools into tools/
cargo run -p gm-tools -- wad make                              # assets/textures/base.wad + palette.lmp
cargo run -p gm-tools -- map build assets/maps/src/test_room.map
cargo test --workspace
cargo run --release -p gm-client -- --map assets/maps/built/test_room.bsp
```

`map build` runs `qbsp -leaktest`, `vis` and `light -extra -lit`. The output is
`assets/maps/built/<name>.bsp` plus `<name>.lit` (RGB lightmaps). Both are committed; the
`.prt` portal file is not. `map gen-arena` regenerates `assets/maps/src/arena.map`, the 8v8
arena of Phase 3 (symmetric, two team bases, pillars, low cover, side walkways), and
`map gen-town` regenerates `assets/maps/src/town.map`, the town of Phase 6 (a square under an
open sky lit by the sun, houses, a market of 30 stall tiles, 128 spawns), and `map gen-dungeon`
`assets/maps/src/dungeon.map`, the tutorial dungeon of Phase 7 (an entry hall, a passage with
two turns, the gate room with two sentinels, a stair, the Warden's hall).

## Client flags

| Flag | Effect |
|---|---|
| `--map PATH` | BSP to load (default `assets/maps/built/test_room.bsp`) |
| `--palette PATH` | 768-byte palette (default `assets/textures/palette.lmp`) |
| `--bench N` | render N frames with the camera sweeping, print statistics, exit. Always uncapped and without vsync unless `--present` says otherwise |
| `--no-vsync` | `AutoNoVsync` (Immediate, else Mailbox) |
| `--present MODE` | force `fifo`, `relaxed`, `mailbox` or `immediate` |
| `--max-fps N` | CPU-side frame cap in normal play (default 250, 0 = uncapped) |
| `--headless` | render offscreen, no window; used by CI |
| `--software` | force the software Vulkan adapter (lavapipe) |
| `--size WxH` | window or offscreen size (default 1280x720) |
| `--screenshot out.ppm` | headless only: write the last frame as a binary PPM |
| `--connect ADDR` | join a zone instead of playing offline (see Multiplayer) |
| `--cert PATH` | DER certificate of the zone, written by `gm-server --cert-out` (default `zone-cert.der`) |
| `--name NAME` | player name for the zone (default `$USER`) |
| `--build NAME` | preset build to ask the zone for: `ironclad`, `blade`, `frostweaver`, `shade` (default: the zone's default) |
| `--team N` | team 1 or 2 (default 0: the zone balances) |
| `--third-person` | offline and in a replay: start in the third-person viewport (`V` toggles there). In a zone the character's mode is the camera (MODES.md 2) |
| `--seconds N` | exit after N seconds and print the network statistics (scripted runs) |
| `--avatar FILE.gmm` | offline: wear this ingested model (how a creator previews one before uploading) |
| `--crowd N` | offline: N characters standing and walking around the start (benchmarks, `check-avatars.sh`) |
| `--crowd-dir DIR` | the crowd wears the `.gmm` files of DIR, one each, fetched through the model cache as if DIR were the hub |
| `--cache-dir DIR` | the model cache (default: the platform's cache directory + `gamengine/models`) |
| `--cache-mb N` | byte cap of the model cache on disk (default 2048, at least 16) |
| `--vram-mb N` | byte cap of models on the GPU (default 256) |
| `--start X,Y,Z,YAW` | offline: start here instead of at a spawn |
| `--hub ADDR --hub-cert PATH` | play through a hub: the screens (see "Playing: the screens"); with `--user`, `--password` and `--character` the same steps by themselves |
| `--settings FILE` | the settings file (default: `settings.toml` in the user's configuration directory, `gamengine/`) |
| `--offline` | walk the map offline even when the settings name a hub |
| `--ui-script FILE` | a script that clicks through the screens (docs/CLIENT.md 9): gates use it, a bug report can carry one |

Keys in a zone with a market: `B` opens a stall on the tile you stand on, `N` closes your stall.

Companions (COMPANIONS.md) follow and fight on their own; the tactical viewport that gave
them orders was removed on 2026-10-06 (COMPANIONS.md 6). The HUD
shows your health, stamina and focus, the squad with its orders, the creature being fought,
and what the zone says of encounters, loot and trials. `Escape` opens the menu (Quit is
there; `Q` no longer quits), `Enter` the chat line.

Default present mode is **Mailbox** (no tearing, no blocking) with the frame cap, not Fifo.
Reason, measured 2026-09-30 on Arch, X11, xfwm4 with compositing, RADV (Renoir): Fifo and
FifoRelaxed presented exactly one frame per second (every swapchain acquire hit its one-second
timeout) even with the window focused and the monitor awake, while Mailbox and Immediate ran at
thousands of fps. If Fifo works on your setup, `--present fifo` gives true vsync.

Bench output lines start with `bench:` (and `avatars:` when characters were drawn) and are
parsed by `scripts/check-perf.sh` and `scripts/check-avatars.sh`. `slowest_frame` is the index
of the frame behind `frame_ms_max`: a hitch at the start is a warm-up, one in the middle a stall.
With a crowd wearing models, frames count once every model is on the GPU. Frame times
in windowed mode are CPU-side (submit to submit) and, with vsync off, they track GPU throughput.
A monitor in DPMS standby lowers iGPU clocks; wake it (`xset dpms force on`) before measuring.

Running from a terminal without a display (ssh, tty): set `DISPLAY=:0` to use the desktop
session's X server, or use `--headless`.

## Multiplayer (Phases 2 and 3)

One zone process, any number of clients and bots, QUIC on UDP (`docs/PROTOCOL.md`). The zone
loads `assets/content` (abilities and preset builds, `docs/MATRIX.md`) and sends it to every
client, so clients need no content files.

```sh
# terminal 1: the zone on the 8v8 arena. Writes its self-signed certificate for clients,
# prints a report every 5 s.
cargo run --release -p gm-server -- --map assets/maps/built/arena.bsp --listen 127.0.0.1:4433 --cert-out zone-cert.der

# terminal 2: fifteen duelist bots for a minute, four presets, alternating teams; they
# counter-pick (re-spec to whatever beats the enemy's aspects) every ten seconds
cargo run --release -p gm-bot -- --connect 127.0.0.1:4433 --cert zone-cert.der --map assets/maps/built/arena.bsp \
    --bots 15 --secs 60 --behaviour duelist --builds ironclad,blade,frostweaver,shade --teams 1,2 --counter-pick

# terminal 3: you, as a blade on team 1, in third person
cargo run --release -p gm-client -- --map assets/maps/built/arena.bsp --connect 127.0.0.1:4433 --cert zone-cert.der \
    --name pezo --build blade --team 1 --third-person
```

Controls online: mouse look, `WASD`, `Space` jump, **left click** primary, **right click**
secondary, **Ctrl** guard (hold to block, press to parry, whichever the build has), **1–4** the
actives (**Shift** is also active 1), **F1–F4** ask the zone
for preset 1–4 (applied at your next respawn), `Esc` releases the cursor, `Q` quits. The
camera is the character's mode's (MODES.md 2): a gun build looks from its eyes, the others
from behind the shoulder, where the shots go where the crosshair points (VOCABULARY.md 9).
In the action mode **Space** dodges while the kit's dash is ready and the primary pressed
again plays the next stage of its chain; in the gun mode **R** reloads and **1 2 3** take
the gun, the pistol and the knife in hand. In the RPG mode the pointer is free: a **click**
on a body targets it, on the ground walks there, **Tab** cycles the enemies in sight,
**1**–**6** are the kit (with a target, the body walks into range first), the **right
button** held turns the camera and the **wheel** moves it. Players are boxes coloured by team (blue own side, red the other), a white
nose box shows their facing, blocking bodies turn blue-ish, staggered or frozen bodies darken,
hasted ones brighten; corpses are flat grey, bolts small yellow boxes, area effects flat orange
discs. The window title shows the build, team and viewport, health, stamina, focus, kills,
deaths, interpolation delay, the correction count, fps and the last respec reply.

Server flags: `--content DIR` (default `assets/content`), `--default-build NAME` (default
`blade`), `--hz 64|20`, `--report-secs N`, `--ticks N` (stop after N ticks),
`--max-players N`, `--seed N`. The report line carries per-player UDP bytes/s in both
directions (from QUIC's own counters), snapshot payload bytes/s, tick timing, starvation, hits
and kills; the final report adds kills per team. `RUST_LOG=debug` shows joins, malformed
datagrams and connection ends.

Bot flags: `--bots N`, `--secs N`, `--behaviour wander|hunter|hold|duelist`, `--builds a,b,...`
and `--teams 1,2,...` (cycled over the bots), `--counter-pick`, `--seed N`, `--map PATH` (the
bots predict against the same BSP). The summary prints bytes/s per bot, RTT, snapshot gaps,
reconciliation corrections, kills and the re-specs that happened.

The zone accepts empty session tokens (`--open` behaviour) until the hub exists in Phase 4.
Clients trust exactly the certificate they are given; there is no insecure mode.

## Accounts, characters and zone handoff (Phase 4)

The hub (`docs/HUB.md`) needs Postgres. Any 14+ works; the hub migrates its schema at start.

```sh
# a database for development (Arch: pacman -S postgresql; initdb once)
createdb gamengine   # or any URL in DATABASE_URL

# terminal 1: the hub. Writes hub-cert.der (clients and zones trust it) and hub.key (persisted
# signing key, keep it). --wipe empties the database first.
cargo run --release -p gm-hub -- --database-url postgres://localhost/gamengine --listen 127.0.0.1:4400 \
    --cert-out hub-cert.der --key hub.key --zone-secret s3cret

# terminals 2 and 3: two zones under the hub (different maps are fine)
cargo run --release -p gm-server -- --map assets/maps/built/arena.bsp --listen 127.0.0.1:4433 --cert-out zone-a.der \
    --hub 127.0.0.1:4400 --hub-cert hub-cert.der --zone-id arena-a --zone-secret s3cret
cargo run --release -p gm-server -- --map assets/maps/built/test_room.bsp --listen 127.0.0.1:4434 --cert-out zone-b.der \
    --hub 127.0.0.1:4400 --hub-cert hub-cert.der --zone-id room-b --zone-secret s3cret

# terminal 4: you. --register creates the account the first time; the character is created with
# the --build preset if it does not exist. GM_TRAVEL_TO=room-b makes the T key ask for a transfer.
GM_TRAVEL_TO=room-b cargo run --release -p gm-client -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der \
    --user you@example.com --password 'at least 8 chars' --register --character Pezo --zone arena-a --build blade

# a bot that logs in, plays 4 s in arena-a, travels to room-b, plays on, logs out
cargo run --release -p gm-bot -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der --user bot@example.com --password botbotbot \
    --register --character Botko --zone arena-a --travel-to room-b --travel-after 4 --secs 12 --behaviour duelist
```

Zones started without `--hub` still run open (empty tokens, builds live for the session), which
is what the netcode tests use. `--public-addr` tells the hub where clients should connect when
the listen address is not routable. The zone secret (`--zone-secret` or `GM_ZONE_SECRET`) is
shared by the hub and its zones.

Tests: `crates/gm-server/tests/handoff.rs` runs the whole trip (register → enter → travel →
logout) against a database named by `GM_TEST_DATABASE_URL`, which it **wipes**; without the
variable the test prints `SKIPPED`. A throwaway cluster for local runs:

```sh
initdb -D /tmp/gmpg -U gm --auth=trust && pg_ctl -D /tmp/gmpg -o "-k /tmp -p 54329 -c listen_addresses=''" start
psql -h /tmp -p 54329 -U gm -d postgres -c 'create database gm_test'
GM_TEST_DATABASE_URL='postgres://gm@localhost:54329/gm_test?host=/tmp' cargo test -p gm-server --test handoff
```

## Companions and the tutorial dungeon (Phase 7)

`docs/COMPANIONS.md` is the contract. A dungeon without a hub, with three recruits lent by
the zone:

```sh
cargo run --release -p gm-server -- --map assets/maps/built/dungeon.bsp --cert-out zone-cert.der \
    --squads --recruits ironclad,mender,frostweaver
cargo run --release -p gm-client -- --connect 127.0.0.1:4433 --build blade --third-person \
    --map assets/maps/built/dungeon.bsp
# or a headless leader that clears it by itself and prints what it got:
cargo run --release -p gm-bot -- --connect 127.0.0.1:4433 --cert zone-cert.der \
    --map assets/maps/built/dungeon.bsp --behaviour raid --builds blade --secs 420
```

Zone flags: `--squads` lets companions in with their commanders (a town does not);
`--recruits a,b,c` lends those preset builds to fill squad slots that hires left empty (the
tutorial only); a map that posts creatures (`gm_creature`) is a wild zone, and `--wild` makes
any map one; `--arrive-at-entry` starts every arrival at the map's spawns instead of where
the character logged out (dungeons); `--requires trial,trial` (with `--hub`) lets in only
characters that have passed one of those trials.

Hired avatars need a hub. An owner lists a character and goes offline; a leader hires from
the tavern and enters:

```sh
cargo run --release -p gm-bot -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der --user owner@example.com \
    --password '...' --register --character Bulwark --builds ironclad --zone dungeon --list-for-hire 100 --secs 0
cargo run --release -p gm-bot -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der --user you@example.com \
    --password '...' --character Marko --zone dungeon --behaviour raid --hire 3 --secs 420
```

`scripts/check-dungeon.sh` is the gate (offline, over a simulated network, a real zone, and
the load of sixteen leaders); `scripts/check-dungeon.sh --online` plays the whole story
through the hub against `GM_TEST_DATABASE_URL`: recruits first, then three avatars hired with
the coin of the first kill, then the gated zone.

## Avatar models and the town (Phase 6)

`docs/MODELS.md` is the contract. A creator's loop needs no hub:

```sh
cargo run --release -p gm-tools -- model template --frame striker --out striker.glb   # the rig to start from
cargo run --release -p gm-tools -- model ingest --frame striker my-avatar.glb         # every violation, or my-avatar.gmm + preview
cargo run --release -p gm-client -- --map assets/maps/built/town.bsp --third-person --avatar my-avatar.gmm
```

With a hub (previous section; add `--models-dir DIR` to choose where the hub keeps models, the
default is `models/`):

```sh
# once: the first moderator (an existing account), who then lets accounts upload
cargo run --release -p gm-hub -- --database-url postgres://localhost/gamengine --grant-moderator mod@example.com
cargo run --release -p gm-tools -- mod uploads you@example.com --user mod@example.com --password '...'

# the creator: read the terms, certify, upload; then wear it on an offline character
cargo run --release -p gm-tools -- model upload --frame striker my-avatar.glb --user you@example.com --password '...' --certify
cargo run --release -p gm-tools -- model list --user you@example.com --password '...'
cargo run --release -p gm-tools -- model wear --character Pezo --model <id> --user you@example.com --password '...'

# the moderator: the queue, a look at a model, the decision, and later a takedown
cargo run --release -p gm-tools -- mod queue --user mod@example.com --password '...'
cargo run --release -p gm-tools -- mod fetch <id> --user mod@example.com --password '...'
cargo run --release -p gm-tools -- mod approve <id> --user mod@example.com --password '...'
cargo run --release -p gm-tools -- mod takedown <id> --code copyright --reason '...' --reference NOTICE-1 \
    --user mod@example.com --password '...'
```

`gm-tools` reads the password from `GM_PASSWORD` when `--password` is absent, and the hub from
`--hub`/`--hub-cert` (defaults `127.0.0.1:4400`, `hub-cert.der`).

The town as a zone, at the 20 Hz of slow zones, with a crowd:

```sh
cargo run --release -p gm-server -- --map assets/maps/built/town.bsp --listen 127.0.0.1:4435 --cert-out town.der \
    --hub 127.0.0.1:4400 --hub-cert hub-cert.der --zone-id town --zone-secret s3cret --hz 20 --max-players 160

# 100 distinct avatars at the budget ceiling, uploaded, approved and worn by avatar-000..099@bots.test
cargo run --release -p gm-tools -- model synth --count 100 --out /tmp/avatars
cargo run --release -p gm-tools -- hub seed-avatars --dir /tmp/avatars --count 100 --user mod@example.com --password '...'

# the bots stroll around the square; the first twelve open stalls
cargo run --release -p gm-bot -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der --user 'avatar-{i}@bots.test' \
    --password avatar-password --character 'Avatar{i}' --zone town --bots 100 --stalls 12 --behaviour stroll --secs 120
```

`scripts/check-avatars.sh --online` does all of the above against `GM_TEST_DATABASE_URL` (which
it wipes) and checks the result.

## The browser client (Phase 8)

`docs/WEB.md` is the contract. The browser client is `gm-client` compiled to wasm; zones and
the hub accept browsers on a WebTransport listener beside their QUIC endpoint.

```sh
scripts/build-web.sh                         # fetches the pinned wasm-bindgen and wasm-opt, builds target/web/
(cd target/web && python3 -m http.server 8080 --bind 127.0.0.1)   # any static server; localhost is a secure context

# a zone with a web listener, and what a page needs to reach it
cargo run --release -p gm-server -- --map assets/maps/built/arena.bsp --cert-out zone-cert.der \
    --web-listen 127.0.0.1:4434 --web-info-out zone-web.json
cat zone-web.json     # {"url": "https://127.0.0.1:4434", "cert_sha256": "…"}
```

Open `http://localhost:8080/?connect=https://127.0.0.1:4434&cert=<the hash>&map=arena&build=blade&third-person=1`
(a zone directly: allowed because the default `config.json` marks the site as a development
one). Chromium on Linux needs `--enable-unsafe-webgpu --enable-features=Vulkan` for WebGPU;
without it the loader takes the WebGL2 build. Click the canvas to take the pointer; guard is
on `C` as well as `Ctrl` (a browser keeps `Ctrl+W`).

Through the hub: start `gm-hub` with `--web-listen ADDR --web-info-out hub-web.json`, start
the zones with `--hub … --web-listen ADDR`, and put the hub's address into the page's
`config.json`: `{"hub": "<url>", "hub_cert_sha256": "<hash>", "dev": false}`
(`scripts/build-web.sh --config FILE` copies one). The page then shows its login form. A
self-signed web certificate is valid for 13 days and its hash changes at every restart; for
anything but development pass `--web-cert PEM --web-key PEM --web-url https://name:port`.

Flags of both servers: `--web-listen ADDR`, `--web-cert PEM --web-key PEM`, `--web-url URL`
(the address to advertise), `--web-origin ORIGIN` (repeatable: the pages that may connect;
without it any), `--web-info-out FILE`. `gm-bot --web URL [--web-cert HEX]` runs bots through
a zone's web listener.

## Replays, aim statistics and moderation (Phase 9)

`docs/ANTICHEAT.md` is the contract. A zone records fights between players and reports when
started with `--replay-dir`; it logs each client's aim numbers when it leaves.

```sh
cargo run --release -p gm-server -- --map assets/maps/built/arena.bsp --cert-out zone-cert.der \
    --replay-dir replays
# twelve bots whose view moves like a hand, four that aim by program
cargo run --release -p gm-bot -- --connect 127.0.0.1:4433 --cert zone-cert.der --map assets/maps/built/arena.bsp \
    --bots 16 --secs 90 --behaviour duelist --builds blade,frostweaver,shade,ironclad --teams 1,2 \
    --aim hand,sharp,hand,sharp,hand,sharp,lock,flick
cargo run --release -p gm-tools -- replay info replays/arena-*.gmr
cargo run --release -p gm-tools -- replay aim replays/arena-*.gmr --shots lock06
cargo run --release -p gm-client -- --replay replays/arena-*.gmr --follow lock06
```

In the viewer: `[` `]` change the player, `Space` pauses, `,` `.` step a tick, the arrows seek
5 s, `1`–`4` set the speed, `V` third person. In play, `F9`
reports the player under the crosshair.

Flags: `gm-server --replay-dir DIR [--replay-mb-per-hour N] [--min-trust N]`;
`gm-bot --aim brain|hand|sharp|lock|flick,... [--report-after SECS]`;
`gm-client --replay FILE [--follow NAME] [--from SECS]` (with `--headless --screenshot`).
Under a hub the zone uploads its replays and aim numbers, and a moderator works with
`gm-tools mod aim-report | replays | replay-get | reports | report | ban | unban |
reputation | adjust` (ANTICHEAT.md 7).

## Playing: the screens (Phase 10)

`docs/CLIENT.md` is the contract. A client that knows where its hub is needs no command
line: it shows a login screen, the account's characters, a screen to make one, and in the
game a menu (`Escape`: Resume, Travel, Settings, Keys, Leave, Quit) and a chat line
(`Enter`).

```sh
# a hub and two zones, as in "Accounts, characters and zone handoff"; --start-zone says where
# a character that has never been anywhere begins (default: the zone called town)
cargo run --release -p gm-hub -- --database-url postgres://localhost/gamengine --listen 127.0.0.1:4400 \
    --cert-out hub-cert.der --key hub.key --zone-secret s3cret --start-zone town
cargo run --release -p gm-server -- --map assets/maps/built/town.bsp --listen 127.0.0.1:4433 --cert-out town.der \
    --hub 127.0.0.1:4400 --hub-cert hub-cert.der --zone-id town --zone-secret s3cret --hz 20
cargo run --release -p gm-server -- --map assets/maps/built/arena.bsp --listen 127.0.0.1:4434 --cert-out arena.der \
    --hub 127.0.0.1:4400 --hub-cert hub-cert.der --zone-id arena --zone-secret s3cret

# you: the login screen. New account, a character, Play.
cargo run --release -p gm-client -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der
```

Where the hub is can also be written down once, and the client then starts with no
arguments at all (from a menu entry, a file manager):

- in the person's settings (`~/.config/gamengine/settings.toml`, `%APPDATA%\gamengine\`):
  `hub = "127.0.0.1:4400"` and `hub_cert = "/path/to/hub-cert.der"`;
- or in a `client.toml` shipped beside the program (the same two lines; a relative
  `hub_cert` is looked for beside it): what a build for players carries.

With neither, the client says so on a screen and offers the offline walk. The settings also
remember the last email and character, the mouse sensitivity, the size of text and who is
ignored in chat; never a password. A run started without a terminal writes `client.log`
beside the settings. `Ctrl+V` pastes into a field (a password from a password manager).

In the browser the page's own form is the login (the browser can fill and remember it);
everything after it is drawn on the canvas. Chat: `/ignore NAME`, `/unignore NAME`.

A UI script plays the person (`--ui-script FILE`; one command a line: `wait screen NAME`,
`field LABEL`, `type TEXT`, `key NAME`, `click TEXT`, `dclick TEXT`, `expect TEXT`, `say
TEXT`, `where TEXT`, `sleep SECS`, `quit`), and fails with the line that waited in vain and
what the screen showed instead:

```sh
printf 'wait screen login\nclick "New account"\nfield email\ntype me@example.com\n' > walk.ui   # and so on
cargo run --release -p gm-client -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der --settings /tmp/s.toml --ui-script walk.ui
```

## Possessions: the inventory, a stall, what is worn (Phase 11)

`docs/ITEMS.md` is the contract. In the game `I` opens the inventory (also **Inventory** in
the menu) and `E` the stall the body stands at. A sword is worn from the inventory; a worn
weapon adds to the damage of its own kinds and a worn armour takes from it, a place for half
of what its item's edge says.

Nothing drops items for a new world yet except a boss (ECONOMY.md 9), so an operator hands
things out with the hub's own program, on the hub's database, while the hub runs or not:

```sh
HUB="cargo run --release -p gm-hub -- --database-url postgres://localhost/gamengine"
$HUB --grant-coin Aldric 15000                           # 1 g 50 s, through the ledger (reason: grant)
$HUB --grant-item Aldric sword core/iron,frame/oak       # a made item; what the content knows, what the template has room for
$HUB --grant-item Aldric ball 10                         # a stack (MODES.md 11): onto the stack carried, up to its cap
$HUB --place Aldric town 200,-320,25 0                   # where an OFFLINE character stands when it next enters (x,y,z and yaw)
$HUB --audit                                             # the books in a line; exit status 1 when they are not sound
```

A stall is opened standing on a market tile (`B`; `N` closes it), filled from the inventory
(**Sell**: a price in gold and silver), and bought from by anybody who walks up to
it (`E`, **Buy**). A bot can keep one for a test:

```sh
cargo run --release -p gm-bot -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der --user keeper@bots.test \
    --password keeper-password --register --character Keeper --zone town --bots 1 --stalls 1 --sell-at 12000 \
    --secs 900 --behaviour stroll --maps-dir assets/maps/built
# its log says where its stall stands ("stall stands ... x= y= z= yaw="); whatever it is
# handed that can be worn, it lists at 1 g 20 s
```

In a UI script the two keys are `key I` and `key E`, and the pages are called `inventory`,
`storage`, `price` and `stall`.

## People together: parties, channels, a trade, the tavern (Phase 12)

`docs/PARTY.md` is the contract. `P` (or the menu) opens the people: whoever is in the zone,
the party and who asked what; **Invite**, **Join**, **Decline**, **Remove**, **Leave**,
**Trade** (standing within 160 units of the other, who asks back), **Whisper**, **Tavern**.
The chat line knows `/p TEXT`, `/w NAME TEXT`, `/r TEXT`, `/invite NAME` and `/leave`. A
party is the hub's and holds from zone to zone; a member that left the game is let go of
after two minutes (`gm-hub --party-away SECS` for a shorter wait in a test). In a fight a
body's party does not change, and nobody who left a fight comes back into it.

A bot can be the other person:

```sh
cargo run --release -p gm-bot -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der --user ana@bots.test \
    --password ana-password --register --character Ana --zone town --behaviour hold --invite Bojan --secs 60
cargo run --release -p gm-bot -- --hub 127.0.0.1:4400 --hub-cert hub-cert.der --user bojan@bots.test \
    --password bojan-password --register --character Bojan --zone town --behaviour hold --sociable \
    --trade-for 300 --secs 900
# --sociable: joins whoever invites, answers a party's line with "aye" and a whisper with
# "psst yourself"; --trade-for SILVER: offers the newest thing it carries and accepts when
# that much coin is on the other side
```

In a UI script the key is `key P`, and the pages are called `people`, `trade` and `tavern`.

## CI gates locally

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
scripts/check-budgets.sh && scripts/check-budgets.sh --self-test
scripts/check-binary-size.sh && scripts/check-binary-size.sh --self-test
scripts/check-perf.sh                 # windowed, real GPU
scripts/check-perf.sh --software      # what CI runs
scripts/check-netcode.sh              # 16 bots at 150 ms / 3% loss in turmoil, bytes/player/s gate, counter-pick
scripts/check-matrix.sh               # 8v8 arena: a dominant build is countered by re-speccing
scripts/check-swarm.sh                # 200 bots on one zone over real UDP: tick time, RSS, bytes (BOTS=32 for a smoke)
scripts/check-avatars.sh --gate-fps   # 100 distinct avatars in the town: fps, RSS, the cache cap (MODELS.md 11)
scripts/check-avatars.sh --software   # what CI runs: 48 avatars, 16 MiB cap, software Vulkan
scripts/check-avatars.sh --online     # hub + town zone + 100 bots wearing uploads + the client (needs a database and a display)
scripts/check-dungeon.sh              # a leader and three companions clear the tutorial dungeon; 16 leaders at once (COMPANIONS.md 14)
scripts/check-dungeon.sh --online     # the same through the hub with hired avatars, loot, the trial and a gated zone (needs a database)
scripts/check-anticheat.sh            # aim statistics on known traces; a recorded arena: programs flagged, hands not; the replay reads back
scripts/check-anticheat.sh --online   # also through the hub: flags, replays, a report upheld, a ban, a trust-gated zone (needs a database)
scripts/check-web.sh                  # the browser build: sizes of both .wasm, QUIC and WebTransport clients in one zone
scripts/check-web.sh --browser        # also headless Chromium in a zone with 15 native bots, per build (--software: no GPU)
scripts/check-web.sh --browser --hub  # also login, 48 avatars through the browser's cache and a travel (needs a database)
scripts/check-screens.sh              # the screens as tests: every screen whole at every window size, every refusal in words
scripts/check-screens.sh --desktop    # also the windowed client on an Xvfb of its own: a cold start by UI script, then by real keys and clicks (xdotool)
scripts/check-screens.sh --browser    # also both browser builds: the page's form by the browser's own input, then the canvas screens
scripts/check-items.sh                # the gear term, the item content, the inventory and stall screens; with a database the hub and a zone with clients by hand
scripts/check-items.sh --desktop      # also a bot that keeps a stall, and a buyer through the windowed client: looks, buys, wears; then E and I from a real keyboard
scripts/check-items.sh --browser      # the same purchase in both browser builds
scripts/check-party.sh                # the split with people in it, the wire's two types, the people, trade and tavern screens; with a database the hub's parties and two zones with clients by hand
scripts/check-party.sh --online       # also two bots: a party in the town, the dungeon cleared together, the loot split by the hub
scripts/check-party.sh --desktop      # also the windowed client: a party by the page, its line and a whisper, a trade for what the other looted, a hire
scripts/check-party.sh --browser      # the same in both browser builds
scripts/check-sound.sh                # the patches, the mixer and the cues as tests; the walk and the arena fight rendered to WAVs on an Xvfb of its own and read (SOUND.md 7)
scripts/check-look.sh                 # the content checked and the committed bundle reproduced byte for byte, props and icons as tests, the armed crowd's draw cost (--gate-fps: on the real GPU)
scripts/check-look.sh --desktop       # also the windowed client: the inventory as a grid, the sword dragged onto its slot and worn, the tooltip, the hotbar read from --report (needs a database)
scripts/check-look.sh --browser       # the same in the WebGPU build, and its wasm against the cap
scripts/check-sound.sh --browser      # also both browser builds: the audio context running after a click, cues started, the same patch bytes as native (--software: no GPU)
```

`target/` grows without bound across sessions (`target/debug` reached 149 GB and filled the disk
once): `rm -rf target/debug/incremental` is safe at any time (a cache cargo rebuilds), and a
`cargo clean` between phases costs one full build.

`check-netcode.sh` runs the turmoil acceptance tests (`crates/gm-server/tests/netcode.rs` and
`counterpick.rs`) in simulated time and the real-UDP loopback test; all print per-bot and
per-zone numbers with `--nocapture`. `check-matrix.sh` runs the offline 8v8 matches of
MATRIX.md 11 (`crates/gm-bot/tests/arena.rs`).

When the release binary legitimately grows (a new feature), update the baseline in the same
commit with `scripts/check-binary-size.sh --update-baseline` and say why in the commit message.

## Maps in TrenchBroom

1. New map, game "Quake", map format "Standard" (Valve 220 also works with qbsp).
2. Replace the entity definitions with `assets/maps/src/gamengine.fgd` (Map > Entity
   definitions).
3. Add `assets/textures/base.wad` as the texture collection. TrenchBroom previews WAD
   textures with Quake's palette; ours is `assets/textures/palette.lmp`, so previews are
   slightly off-colour in the editor and correct in the game.
4. Save under `assets/maps/src/` and run `cargo run -p gm-tools -- map build <file>`.

Entities: `worldspawn` keys `wad`, `light` (minlight), `_sunlight*`, `_dirt`, `_bounce`;
`light` (point or spot with `mangle`); `info_player_start` (a spawn for any team);
`gm_spawn` with `team` 1 or 2 (0 = any) and `angle`; `func_detail`, `func_wall`,
`func_illusionary`; `gm_zone` is a placeholder for later phases. `gm_creature` posts a
creature of `assets/content/creatures.toml` (`creature`, `encounter`, `angle`); creatures that
share an encounter name fight, reset and are cleared together. `gm_stall_grid` is a market:
`origin` is the centre of the first tile on the ground, `cols` × `rows` tiles of side `tile`
(128) spaced `pitch` (160) apart along +X and +Y, `angle` the way the keepers face, `base_x`
and `base_y` the tile numbers of the first tile (distinct per grid of a map; at most 512 tiles
per map). A brush textured `sky_day` is sky: it lets `_sunlight` in and is not drawn as a wall.

## Development helper: independent reviews

`scripts/dev/gemini-review.py --prompt "..." --file docs/PROTOCOL.md --file crates/...` sends a
prompt plus files to Google AI Studio (Gemini) and prints the answer; it needs
`~/google-ai-studio-api-key` or `$GEMINI_API_KEY`. It is a development aid for design and code
reviews (PROTOCOL.md section 10 records one); CI never runs it and nothing depends on it.
