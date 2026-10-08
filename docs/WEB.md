# Web: the browser client

Status: v1 (Phase 8). This document is the contract for the browser build: what the browser
client is, how it reaches a zone and the hub, what it stores, what it costs, and what it
deliberately lacks. PLAN.md 1.2 ("browser build comes free": rendering does, networking does
not), 2.1 (QUIC natively, WebTransport in the browser, one protocol), 2.8 (the browser is a
priority on-ramp) and 11.8 Phase 8 (**a browser client joins the same zone as native clients**)
are binding. When the code and this document disagree, the document wins. Section 12 records
the reviews and what running it found.

## 1. Principles

1. **One client.** The browser client is `gm-client` compiled for `wasm32-unknown-unknown`: the
   same prediction, interpolation, renderer, HUD and viewports. What differs is only what a
   browser forbids: sockets, threads, files, blocking.
2. **One protocol.** A WebTransport session carries the same bytes as a QUIC connection:
   PROTOCOL.md's datagrams as WebTransport datagrams, the control stream as one bidirectional
   stream, the hub's one-request-per-stream as bidirectional streams. No message knows which
   transport carries it; a zone treats both kinds of client alike.
3. **Nothing new on the server but a listener.** Zones and the hub gain a WebTransport listener
   next to the QUIC one. Native clients, bots and zone ↔ hub links stay on plain QUIC.
4. **Measured in megabytes here too.** The download is budgeted and gated like the native
   binary. A feature that only some browsers need is not paid for by the others.
5. **No web stack.** No JavaScript framework, no bundler, no npm. One HTML page, one loader
   script, the `wasm-bindgen` glue, the `.wasm`.

## 2. Transport

### 2.1 WebTransport beside QUIC

- `gm-server --web-listen ADDR` and `gm-hub --web-listen ADDR` open a second UDP port with an
  HTTP/3 WebTransport endpoint (`wtransport` 0.7, which runs on the same `quinn` the QUIC
  endpoint uses). Without the flag nothing changes. The session path is ignored.
- **[CORRECTED]** PLAN.md 2.1 and 11.2 named `wtransport` as the *browser* transport. It is the
  *server* side (and a native test client); in the browser the transport is the browser's own
  `WebTransport` object. There is no Rust QUIC stack in the `.wasm`. The client binds the
  handful of calls it needs itself (`crates/gm-client/src/web/wt.rs`): web-sys keeps
  `WebTransport` behind an unstable-API flag, and the session, a reader and a writer are
  fifteen declarations.
- A second port, not ALPN dispatch on the first: the two endpoints need different certificates
  (2.2) with different lifetimes. One port serving both is possible later and is not built.
- Server side, one type hides the difference: `gm_net::link::Link` is `Quic(quinn::Connection)`
  or `Web(wtransport::Connection)` with `send_datagram`, `read_datagram`, `accept_bi`,
  `open_uni`, `close`, `closed`, `rtt`, `stats`. A WebTransport stream is a QUIC stream after
  its header, which `wtransport` reads and writes, so the stream halves deref to the `quinn`
  streams they are and the framing code is shared. Two calls do differ and are methods of the
  halves: `stop` and `reset` carry an application error code, which on a WebTransport stream
  must be mapped into the range HTTP/3 reserves for it (a raw code reaches the browser as a
  protocol error). The zone's tick loop, its sessions and the hub's request handler take a
  `Link` and are otherwise unchanged.
- **Transport parameters** on the zone's WebTransport endpoint are those of PROTOCOL.md 1 (the
  fixed 64 KiB congestion window, 10 s idle timeout, 2 s keep-alive, the 4 KiB datagram send
  buffer) with one difference: eight unidirectional and eight bidirectional streams, because
  HTTP/3 itself opens three unidirectional streams (control and the two QPACK streams) and the
  session's CONNECT takes a bidirectional one. With PROTOCOL.md 1's zero unidirectional streams
  a session never establishes (found by the first loopback test). The hub's WebTransport
  endpoint keeps Cubic (model downloads). The browser's own congestion controller governs what
  the browser sends (inputs, 6.6 KB/s) and cannot be configured; the client asks for
  `congestionControl: "low-latency"`, which is a hint.
- **One session per connection.** The client opens its session with `allowPooling: false`
  (budgets, statistics and the fixed window are per connection; a pinned certificate forbids
  pooling anyway) and `requireUnreliable: true` (no fallback to a transport without
  datagrams).
- **Datagram size.** A WebTransport datagram costs the HTTP/3 quarter-stream-id varint (one
  byte for a connection's first session) on top of the QUIC datagram frame.
  `MAX_DATAGRAM_PAYLOAD` (1,100 bytes) stays under the 1,200-byte initial MTU with it. The
  server refuses a session whose `max_datagram_size` is under 1,100.
- **Order and loss** are those of QUIC datagrams: unordered, unreliable, never retransmitted.
  PROTOCOL.md 4 to 7 hold unchanged. In the browser incoming datagrams queue in the session's
  `datagrams.readable`; the client sets the queue to 16 datagrams (the property is
  `incomingMaxBufferedDatagrams` in Chromium, where it defaults to **one**, and
  `incomingHighWaterMark` in the specification: both are set), the outgoing queue
  (`outgoingHighWaterMark`) to 16 as well, and `incomingMaxAge` / `outgoingMaxAge` to 250 ms.
  When a queue is full the browser drops the oldest. The outgoing queue matters on a slow
  page: a frame writes one input datagram per tick it stepped, all in one task, and the
  browser sends them only after the task; at Chromium's default of one every input but the
  last written in the frame was dropped before it left, and a zone at 64 Hz saw 15 inputs a
  second from a page at 15 fps and starved the rest (seen on CI's software WebGL, 2026-10-08).
  The client's own queue between its reader task and the frame is bounded the same way (32
  snapshots, oldest out): reader tasks run in a hidden tab, frames do not.
- **Handshake** is PROTOCOL.md 8: the client opens one bidirectional stream, sends `Hello`,
  reads `Welcome` or `Reject`.
- **Refusing and kicking.** A connection closed in the same instant as its last message can
  take the unread message with it, and a browser discards what a stream had queued when the
  session errors. So after `Reject` the zone finishes the stream and gives the client up to
  one second to hang up before it closes; after `Kick` it closes half a second later. Both
  hold for QUIC clients too. `wtransport` closes a session with a QUIC close, so a page sees
  its session end without a code: the reason a player reads is the `Reject` or `Kick` text.
- **A client that sends nothing leaves.** A QUIC connection stays alive on keep-alives alone,
  and a browser's network stack answers them whether or not the page runs. So the zone kicks
  a session that has sent no input datagram for 10 s (`INPUT_IDLE_SECS`; a ghost waiting for
  another zone's claim is exempt). A session that has not sent its *first* input yet has 60 s
  (`FIRST_INPUT_SECS`): a browser fetches the zone's map after `Welcome` and predicts nothing
  until it has it. This is the rule for a hidden tab (3.3) and equally for a frozen native
  client (a window dragged by its title bar for ten seconds on some systems, a debugger).
- **Origin.** The WebTransport endpoint accepts a browser's session only from an `Origin` in
  `--web-origin` (repeatable; compared without case and without a trailing slash; without the
  flag any origin, with a warning at start). A session without the header is a native program
  (`gm-bot --web`) and is let in: it could have sent any `Origin` it liked, so refusing it
  would protect nothing. This is not authentication; the token is. It stops other sites'
  pages from using a visitor's browser as a client.
- **Nothing waits for ever.** The browser client gives the connect and the handshake 10 s
  each, a hub request 15 s, a map 45 s, a model 120 s, a `Bye` half a second; a reader
  dropped before its stream ended cancels the stream; a session whose reliable messages
  nobody drains (4,096 queued) is given up.

### 2.2 Certificates

A browser accepts a WebTransport server in two ways, and both are supported:

1. **A certificate chain to a public root** for a DNS name (production):
   `--web-cert PEM --web-key PEM`, and `--web-url https://zone-a.example.org:4443` for the
   address to advertise.
2. **A pinned hash** (`serverCertificateHashes`; development): the browser accepts a
   self-signed certificate whose SHA-256 it was given, if the certificate is ECDSA P-256 and
   valid for at most 14 days. Without `--web-cert` the process generates one valid for 13 days
   (starting an hour ago, for clocks that run behind) and advertises its hash. **Such a
   process must be restarted within 13 days.** Rotation without a restart is not built.

A pinned hash does not make the *page* servable from anywhere: WebTransport needs a secure
context, so the page itself must come from `https://` or from `http://localhost`. A LAN
party by IP address therefore needs a real certificate for the page's host or a tunnel;
pinned hashes are for development on one machine and for zones behind a properly served page.
`scripts/dev/cloud.sh` is the second case for friends over the internet (2026-10-08): the play
stack of `scripts/dev/play.sh` on the cheapest Hetzner Cloud server (static musl binaries, the
browser build), the page at a real name over https (Caddy, Let's Encrypt), the hub's and the
zones' endpoints on their pinned certificates; `up`, `deploy`, `status`, `logs`, `play`, `down`.

The QUIC endpoint's certificate is unchanged (long-lived, self-signed, pinned by DER).

### 2.3 What a ticket carries

Protocol v4 and hub v1.4:

- `ZoneHello` gains `web: Option<WebAddr>`, `WebAddr { url: String, cert_sha256:
  Option<[u8; 32]> }` (`None` for the hash means "a publicly trusted certificate").
- `ZoneTicket` and `FromZone::TravelTicket` (`Control::TravelTicket` then) gain `web: Option<WebAddr>`.
- A browser client that receives a ticket without `web` says "this zone has no web listener"
  and stays where it is (a travel) or fails (an entry).

The hub's own web address is not discovered: the page is configured with it (`config.json`
next to the page: `{"hub": "https://…", "hub_cert_sha256": "hex" | null, "dev": bool}`,
fetched with `cache: "no-store"`). `gm-hub --web-info-out FILE` and `gm-server --web-info-out
FILE` write `{"url", "cert_sha256"}` at start, for whoever assembles that file: with a
pinned certificate the hash changes at every restart.

## 3. The browser client

### 3.1 Build

`scripts/build-web.sh`: `cargo build -p gm-client --target wasm32-unknown-unknown --profile
web` (the release profile with `opt-level = "s"`), `wasm-bindgen --target web`, `wasm-opt
-Oz`, twice (3.2), and the page with its assets into `target/web/`:

```
index.html  boot.js  config.json
gm-client-webgpu.js  gm-client-webgpu_bg.wasm
gm-client-webgl.js   gm-client-webgl_bg.wasm
assets/maps/*.bsp *.lit   assets/textures/palette.lmp
```

The two tools are fetched pinned and hash-checked by `scripts/fetch-web-tools.sh` into
`tools/web/`, as ericw-tools is; the wasm-bindgen CLI's version must equal the crate's in
`Cargo.lock`, and the script refuses to run when they differ. Any static file server serves
the directory.

### 3.2 WebGPU, and the WebGL2 answer

Two builds, chosen by the loader, because the cost is not symmetric: on WebGPU `wgpu` is a
thin binding to the browser (no `wgpu-core`, no `naga`: the browser compiles the WGSL); for
WebGL2 it links its GLES backend and the shader translator. Measured: **0.90 MB against
2.89 MB** of `.wasm`, 0.30 MB against 0.87 MB compressed (9). `boot.js` asks `navigator.gpu`
for an adapter; if it gets one it loads the WebGPU build, otherwise the WebGL2 build (which
draws through WebGL2 only); with neither it says so and stops. If the WebGPU build then fails
to get a device after all, the page reloads once with `?gl=1`: a canvas that has had a WebGPU
context cannot give a WebGL2 one, so the retry needs a fresh page.

The renderer stays inside `downlevel_defaults` (PLAN.md 11.2), and on the GL backend asks for
`downlevel_webgl2_defaults` (no storage buffers, no compute: asking WebGL2 for more fails the
request). A WebGPU canvas has no sRGB surface format; frames are drawn through an sRGB *view*
format of it, so both builds and the native client produce the same colours. BC1 atlases
upload compressed where `texture-compression-bc` or `WEBGL_compressed_texture_s3tc` exists
and are decoded on the CPU elsewhere, as on native.

### 3.3 What a browser forbids, and what stands in

| Native | Browser |
|---|---|
| a tokio runtime on a thread drives quinn; channels to the render thread | the page's event loop drives `WebTransport`; `wasm-bindgen-futures` tasks push into the queue the frame drains |
| `std::time::Instant` | `web_time::Instant` (`performance.now()`); the std clock compiles for wasm and panics when called, so `scripts/check-web.sh` greps for it |
| `std::thread::sleep` frame cap | none: `requestAnimationFrame` paces the frame (control flow `Wait`, each redraw asks for the next) |
| `pollster::block_on` for the GPU device | the device is requested in a task; the frame that finds it builds the renderer |
| the hub login blocks before the window opens | the login is awaited before the event loop starts |
| `std::fs` for maps and the palette | `fetch` from the page's origin; a map is checked against `Welcome.map_hash` as on native |
| a zone's map is loaded between two frames | the fetch takes many frames: what the zone sent after `Welcome` waits in a backlog (snapshots are dropped, they would be stale) until the map is in |
| four loader threads, a directory with a disk cap | async fetches (at most four in flight), the Cache API with a byte cap (4) |
| closing the connection waits up to a second for the zone's answer | `Bye` is written and the session closed behind it; nothing in a page may block |
| flags on the command line | the query string, same names; a form on the page for the login |
| `Q` quits | nothing: closing the tab is the browser's |
| a panic aborts the process | a panic aborts the module: the hook first writes why to the page's status line |

The simulation runs in fixed ticks from an accumulator as on native, so a 60 Hz display runs
64 Hz ticks correctly. **A hidden tab stops** (`requestAnimationFrame` is not called): the
client sends no input, the zone holds the body still and after 10 s kicks the session (2.1).
On becoming visible again within that time the client drops its accumulator and
resynchronises from the next snapshot; after it, it shows the kick.

### 3.4 Keys the browser keeps for itself

`Ctrl+W` closes a tab and cannot be prevented by a page, and guard is on `Ctrl` while `W` runs
forward. So: guard is also on `C` on every platform; the page asks before unloading while the
client runs (`beforeunload`); and the page's "fullscreen" control requests fullscreen and,
where the browser has it (Chromium), the Keyboard Lock API, under which `Ctrl+W`, `Tab` and
`Esc` reach the game (leaving fullscreen is then a long press of `Esc`). A browser gives the
pointer only to a click: the first click on the canvas takes it, `Esc` releases it (the
browser's rule), the next click takes it again. That `Esc` never reaches the page, so the
client asks every frame whether it still has the pointer, and takes losing it for the key:
the menu opens, or the chat line is dropped (CLIENT.md 6). Browsers apply the system's mouse
acceleration under pointer lock unless asked not to; the client does not ask, so aim feel
differs slightly from native. Listed in 10. The page asks for the pointer itself
(`web::ask_for_pointer`, since Phase 11; winit's own asking left a refusal in the console as
an uncaught rejection): a browser that says no has answered, and the next click asks again.

### 3.5 Fingers (2026-10-08)

The director opened the LAN page on a phone (a Galaxy S23, Firefox) and it ran: the form,
the hub, the town, the WebGL2 build. Nothing answered a tap, because the client listened to
the mouse only and winit reports a finger apart (`WindowEvent::Touch`, a phase and a
position per finger). Now `touch.rs` follows each finger from where it landed and the app
treats what it did as it treats the mouse and the keys (MODES.md 5.6 has the controls):

- **A screen up**: the finger is the pointer. Down is the press of a click, moving drags
  (a list, a slider, the paperdoll), lifting is the release; two quick presses on one
  spot are a double click. One finger at a time; a second one there is nothing.
- **The RPG mode**: a tap on a body targets it, on the target it is the primary, on the
  ground a walk there (5.5 as it is); a drag turns the orbit camera (the right button's
  drag), a long press on the target is the secondary (the right tap), two fingers moving
  apart or together are the wheel (120 to 400 u).
- **The action and gun modes**: the left 45 % of the frame is a stick around where the
  finger landed (56 dots to the rim, a dead zone of an eighth; the keys are taken first
  when any is down); on the right a drag looks and a tap is the primary, held for 0.12 s
  (a tick at least: one shot, one blow). A drag turns by 3 mouse counts a CSS pixel: a
  swipe across a phone's width is half a turn at the default sensitivity.
- **Buttons on the HUD**, drawn from the first finger seen: `menu` top right (Escape:
  the menu, or the target let go first in the RPG mode), and bottom right `jump` with the
  secondary (`2`) in the action and gun modes, the secondary alone in the RPG mode; every
  cell of the hotbar is a button for its key (`1`–`8`, Shift, C, the mouse buttons), held
  while the finger is. The look's pointer lock is never asked for by a finger.
- **The scale**: with `ui_scale` 0 a touch screen is drawn at the device's pixel ratio
  rounded, at least 2 and at most 4 (`ui::touch_scale`): an S23 at 2.625 asks for 3, and
  gets it in fullscreen (2340 by 1080 device pixels: the tallest panel, 360 units, is the
  whole height), 2 with the browser's bars (CLIENT.md 3's fit rule). A dot is then a
  device pixel ratio, so a 40-dot cell is 4 to 7 mm under a finger.

**The canvas was drawn at a third of its pixels.** winit sets the canvas's CSS size and
never its backing size (`width`/`height`), and the client configured its surface from
`inner_size`, which the browser reported in CSS pixels: on a phone of 2.625 device pixels
a CSS pixel the arena was rendered at 892 by 412 and stretched, and a finger, which winit
reports in device pixels, landed at 2.6 times its place, outside the frame (the gate's
stick became a look). Now the surface is the canvas's client size times
`devicePixelRatio` (`web::canvas_device_size`, `app::frame_size`), which configuring the
surface writes on the canvas: 2342 by 1082 in the gate, crisp on any display scaled over
100 % (a 4K desktop at 150 % had the same stretch).

The page (5) keeps a finger on the canvas for the game: `touch-action: none`, no
selection, no tap highlight, no overscroll, `viewport-fit=cover` and no pinch zoom of the
page; with a coarse pointer the fullscreen control is finger-sized and the form no wider
than the screen; and fullscreen locks the orientation to landscape where the browser
allows it. The **phone step of `scripts/check-web.sh`** runs the WebGL2 build offline in
headless Chromium as an S23 held sideways (`web-run.mjs --mobile`: 892 by 412 CSS pixels
at 2.625, touch emulation on) and plays the fingers 6 s in (`--touch 6`: the stick
pushed forward for 1.5 s, a swipe across the right, a tap), then reads the client's report:
`touch=1`, `ui_scale=3`, and `yaw` and `pos` changed between the first report and the
last (the report carries them since this change). The screenshot is `browser-phone.png`
under `KEEP`. What a phone cannot do yet is listed in 10.

## 4. The model cache without a filesystem

MODELS.md 8 holds, with the Cache API in place of the directory
(`crates/gm-client/src/web/store.rs`; the state machine above it, `cache.rs`, is shared with
the native client):

- A model is stored as a `Response` under `https://gm-cache.invalid/<hex id>/<bytes>` in the
  cache `gm-models-v1` of the page's origin. The size is in the key because enumerating a
  cache yields requests, not responses: the index (`id`, bytes, last use) is rebuilt at start
  from the keys alone.
- **The cap is ours, not the browser's**: `cache-mb` (default 128 in the browser, clamped to
  16 MiB – 2 GiB). Room is made
  before a write, least recently used first, the own avatar last; the size is reserved before
  the write so that four concurrent loads cannot each find room for one file. "Last use" is a
  counter in memory: within a session the order is exact, across sessions everything found at
  start is equally old.
- An entry being written is not evicted from under its write, and a write whose entry left
  the index meanwhile (a takedown) is deleted whatever the index says: nothing sits in the
  cache uncounted.
- **One tab.** Each tab keeps its own index over the origin's cache, so two tabs of one
  browser can together hold twice the cap. Not handled (a Web Lock and a re-enumeration
  before evicting would); listed in 10.
- If the browser refuses the cache or a write (quota, a private window), the session runs
  with models in memory only and says so once. If the browser evicted an entry behind the
  index's back, the read misses, the entry is forgotten and the model is fetched again.
- The hash is checked before parsing, on every read from the cache and every download; a
  revoked model is deleted and never stored again in the session.
- Downloads are `ModelGet` on hub streams (2.1), at most four in flight, with the native
  back-off and a 120 s limit each. Parsing (inflate, the strict reader) runs on the main
  thread, **at most one model per frame**.
- The GPU cap (`vram-mb`) is unchanged.

## 5. The page

`index.html` holds a stage with a canvas, a login form (email, password, "new account", which
asks for the password a second time), a status line and a fullscreen control; fullscreen
takes the stage, so the form and the status line are there when the client asks for them. `boot.js` picks the build (3.2), reads `config.json` and the query
string, leaves the options in `globalThis.gmOptions` and imports the build; the client
reports to the page through `globalThis.gmStatus(kind, text)` (`status`, `error`, `stats`,
`done`, and since Phase 10 `login`, `login-wait`, `screen` and `say`).

**The form is the client's login screen** (CLIENT.md 4.1), so that the browser can fill and
remember what goes into it. The client starts at once, behind the form, and says when it
wants it: `login` shows the form with the text as its notice (the hub's last refusal, in
words); `login-wait` switches it off while the hub is asked; `screen` hides it, because the
canvas has a screen of its own (the characters, a new character, the game). On submit the
page leaves `{ user, password, register }` in `globalThis.gmLogin` and empties its password
field; the client takes the object on its next frame and deletes it. A client that died
takes no login: the form goes with the error. Everything after the login is drawn on the
canvas. The page asks before it is left (`beforeunload`) only while something is played:
not at the login form, and not in a scripted run. `say` carries a UI script's word to whoever drives the
browser (`GM-SAY ...` on the console, CLIENT.md 9).

A link may set what a visitor *sees* and nothing that acts for them: `third-person` (offline only since 2026-10-07: in a zone the mode is the camera, MODES.md 2),
`map` (the offline map) and `gl` (force the WebGL2 build). **Only on a
site whose `config.json` says `"dev": true`** a link may also carry the rest of the native
flags: `connect=https://host:port&cert=<hex>` (a zone directly, without a hub), a login
(`user`, `password`, `character`, `register`, `zone`), `name`, `build`, `team`, `seconds`, `report`
(a line of statistics a second), `script=fight` (a scripted player: it walks at the nearest
enemy and attacks), `travel-to` / `travel-after`, `cache-mb`, `vram-mb`, and `ui-script` (the
text of a script that clicks through the screens, CLIENT.md 9). On a real site a
link with `connect` would walk a visitor's client into a stranger's zone, one with `script`
or `travel-to` would play their character for them once they log in, one with `cache-mb`
would resize their cache. With neither a connection nor a login the client plays offline on
`map`, as native does. A map's name, from a link or from a zone's `Welcome`, is letters,
digits, `_` and `-` only on every platform: it becomes a URL here and a file name natively.

The password is sent to the hub over the WebTransport session (TLS 1.3). The page's form
field is emptied on submit and the client deletes the password from `gmLogin` (or, on a
development site, from the options object) as it reads it and clears its own copy once the
hub has answered; the session id lives in memory only, so a reload logs in again. The
client's settings (CLIENT.md 8) are one `localStorage` entry, `gamengine.settings`: the last
email and character, the mouse, the size of text; never a password.

**A new build reaches every browser** (2026-10-06): `scripts/build-web.sh` writes the
build's stamp (twelve hex digits of the wasm's, the glue's and the loader's hash) on the
page's `<script src="boot.js?v=...">`, and `boot.js` carries it to the module it imports
and to the wasm it hands `init` (`module_or_path`). A browser revalidates the page itself
on every visit; the files under it are cached by heuristics (a file not changed in three
days is kept for hours) and would otherwise be an old client against a new zone, which is
`Reject("protocol version mismatch ...: reload the page")` and a ticket spent for nothing
(the character is "still in a zone" for the hub's transit sweep, a minute). Only the stamp
is required of the server; the play stack's servers (`scripts/dev/play.sh web`) also send
`Cache-Control: no-cache`, so that a `build-web.sh` without a changed stamp is seen too.
The zone's map is fetched as `assets/maps/NAME.bsp?v=<the hash the zone told>` (and the
`.lit` the same): a browser that cached last build's map under the plain name — the
director met it on 2026-10-06, the town rebuilt with its trainer and dummy, "this site's
copy of the zone's map is another build" from the LAN — has nothing under the stamped one.
The page's first map (the `map` link option, offline) has no hash and is fetched plain.

## 6. Security notes

- The page's origin, the hub and the zones may be three different hosts. WebTransport has no
  CORS; the `Origin` check of 2.1 is the server's only word on who may embed a client.
- Everything a zone or the hub receives from a browser is what it receives from a native
  client, and is trusted exactly as little. Rate limits are per IP and per session as before.
- `serverCertificateHashes` pins exactly one certificate; a zone address or hash arriving in a
  ticket comes from the hub over an authenticated session.
- A zone can send a client anywhere with a `TravelTicket`; that was so before. The zone a
  client is in is one the hub gave it a ticket for, which is why `connect=` is a development
  option (5).
- The `.wasm` is public, as the native client's source is (PLAN.md 8).

## 7. Tests and tools without a browser

- `gm_net::link` tests: a WebTransport session carries framed messages and datagrams both
  ways; a wrong certificate hash is refused.
- `crates/gm-server/tests/loopback.rs`: a QUIC bot and a WebTransport bot in one zone over
  real UDP, both with their snapshots and each seeing the other.
- `crates/gm-server/tests/handoff.rs`: after the native round trip, a WebTransport session to
  the hub's web listener registers, creates a character, gets a ticket that names the zone's
  web listener (and one without for a zone that has none) and plays in the zone through it.
- `gm-bot --web URL [--web-cert HEX]` is the same bot on a zone's web listener.

## 8. Acceptance (PLAN.md 11.8 Phase 8)

`scripts/check-web.sh`:

1. Greps the code the browser build compiles for the std clock (3.3).
2. Builds both `.wasm` and gates their sizes (9).
3. Runs the transport tests of 7.
4. `--browser`: an arena zone with `--web-listen`, 15 native duelist bots, and headless
   Chromium (`scripts/web-run.mjs`: the DevTools protocol over Node's own WebSocket, no npm)
   running the page with `script=fight` for 40 s, once per build. Passes when the build drew
   through the API it is for, the zone saw a WebTransport session, the client counted at
   least 55 snapshots a second, had no more unexplained corrections than the netcode budget
   allows, dealt and took damage (the zone's "session at leave" line), stayed inside `[net]`
   in both directions as the zone's QUIC statistics count UDP bytes, inside the memory and
   first-frame budgets, and held the frame rate. `--software` runs Chromium on SwiftShader
   (what CI has) and reports the frame rate without gating it.
5. `--browser --hub` (needs `GM_TEST_DATABASE_URL`): a hub with a web listener, a town of 48
   bots wearing uploaded avatars and an arena, the browser logging in through the page's
   `config.json` with a 16 MiB cache against 18.3 MB of models. Passes when all 48 models
   were fetched, drawn and none failed, the cache never exceeded its cap, a second visit with
   the same browser profile took at least 20 models from the cache, and a travel to the arena
   ended there on a WebTransport session.

## 9. Budgets and what was measured (`budgets.toml [web]`)

Measured on the development machine (Ryzen 7 4800U, Radeon Vega iGPU, Chromium 150 headless
with `--enable-unsafe-webgpu --enable-features=Vulkan --use-angle=vulkan`, which gives the
page the real GPU; WebGL2 through ANGLE on the same GPU):

| What | Budget | Measured |
|---|---|---|
| `gm-client-webgpu_bg.wasm` | 1 MiB; **2 MiB from Phase 14** (below) | 905,937 bytes |
| the same, brotli -q 11 | 384 KiB; **768 KiB from Phase 14** | 298,756 bytes |
| `gm-client-webgl_bg.wasm` | 3.25 MiB | 2,899,339 bytes |
| the same, brotli -q 11 | 1 MiB | 871,977 bytes |
| JavaScript: glue + loader + page | 176 KiB | 111 KB (WebGPU), 166 KB (WebGL2), uncompressed |
| navigation to the first frame, loopback | 3 s | 142–192 ms (WebGPU), 258–292 ms (WebGL2) |
| wasm linear memory, 8v8 arena | 64 MiB | 3.3 MiB (WebGPU), 4.5 MiB (WebGL2) |
| wasm linear memory, the town with 48 avatars | 64 MiB | 8.6–9.3 MiB |
| the browser's cache, 48 avatars (18.3 MB) under a 16 MiB cap | 16,777,216 | 16,715,540 (44 models), read back entry by entry; the client's own count agrees to the byte |
| models from the cache on the next visit | 20 | 32–37 of 48 |
| frame rate, arena with 15 bots | 55 fps | 60.0 (the display's rate; p99 17.6–17.9 ms) |
| frame rate, the town with 48 avatars | 55 fps | 60.0 (p99 18.2–18.6 ms) |
| snapshots per second | 55 | 64 (0 gaps) |
| zone → browser, UDP bytes per second | `[net]` 30,720 | 7,000–8,700 |
| browser → zone, UDP bytes per second | `[net]` 30,720 | 6,600–6,700 |

The browser sends 6.6 KB/s where a native client sends 5.5: HTTP/3 datagram framing and
Chromium's acknowledgement pattern, counted by the zone's QUIC statistics.

The whole first download of the WebGPU build, compressed: 299 KB of `.wasm`, about 25 KB of
JavaScript and HTML, and the arena's 0.4 MB map with its lightmaps.

**The megabyte, raised (2026-10-03).** With 39,111 bytes left after Phase 13 and a phase of
screens ahead (LOOK.md), the director allowed the WebGPU build "double or triple, as long
as it runs smoothly on a 100 Mbps connection". The cap is **2 MiB** raw and **768 KiB**
packed: at 100 Mbps the raw file is 0.17 s and the packed one under 0.07 s, under the
map that comes with it, and the WebGL2 build at 2.9 MiB already reaches its first frame in
258–292 ms on loopback. The WebGL2 caps do not move. Every phase still reports what it
added: Phase 14 measured **1,104,519 bytes (374,635 packed)** for WebGPU, +95,054 over
Phase 13 (the atlas and its faces, grids, drags and tooltips, the hotbar, props, the
paperdoll pass, the bundle), and 3,094,152 (946,533 packed) for WebGL2. The page now also
fetches the content bundle (`assets/built/content/`: the manifest and the thinnest atlas
at start, 2 KB and 50 KB, then the atlas of the UI's scale, 124 to 283 KB, once per
scale; a prop of 2–16 KB when first seen), copied by `scripts/build-web.sh`. With the
atlases per density (LOOK.md 2.2 and 11.4) the builds are **1,112,187 bytes (376,460
packed)** and 3,103,848 (948,678); with the fight's effects and protocol v9 (LOOK.md 13)
**1,136,246 (383,589 packed)** and 3,128,065 (956,743); with the fingers and the
canvas's device pixels (3.5, 2026-10-08) **1,249,355 (421,092 packed)**, +12,000 over the
1,237,363 before it, and 3,240,891 (991,920).

## 10. Deliberately absent

- On a phone (3.5): the chat line and the stall's prices (no keyboard without a field the
  browser knows); the hotbar past eight cells; a second stick for the camera in the gun
  mode (the right drag is the look there, and a tap the shot); the look's sensitivity for
  fingers as a setting of its own (the gain is a number in `touch.rs`); iOS, where
  Safari has no fullscreen on a phone and WebTransport only since 2025 (untried).
- Rotation of pinned certificates without a restart (2.2); one UDP port for both transports.
- Two tabs sharing one cap, and a last-use order that survives a reload (4).
- Threads (`SharedArrayBuffer`, which needs cross-origin isolation headers on every host) and
  therefore off-thread model parsing.
- Touch controls and a mobile layout; ETC2/ASTC model atlases (BC1 is decoded on the CPU on
  GPUs without it, which costs memory: 4 MB per 1024² atlas instead of 0.7 MB).
- `unadjustedMovement` under pointer lock (raw mouse input, 3.4).
- Model upload from the browser (still `gm-tools model upload`). (The inventory, the trade
  window and sound, absent in v1, came with Phases 11, 12 and 13: 14, 15, 16.)
- A service worker / offline install; saved logins.
- Safari- and Firefox-specific testing: the gate runs Chromium. The client needs WebTransport
  with datagrams and either WebGPU or WebGL2; a browser without them gets a sentence saying
  what is missing.
- A scripted player that fights well: `script=fight` exists to prove damage flows both ways
  (it lands a handful of hits in 40 s against duelists that block and strafe), not to win.

## 11. Changes to other contracts

- PROTOCOL.md v4: `TravelTicket.web`; section 1 names WebTransport as a second carrier; the
  input-idle kick and the grace after `Reject` and `Kick`.
- HUB.md v1.4: `ZoneHello.web`, `ZoneTicket.web`, `--web-listen` and the certificate flags.
- MODELS.md 8: the browser's cache (4 above).
- PLAN.md 2.1, 11.2: `wtransport` is the server side [CORRECTED]; 2.8 gains the two builds.

## 12. Review log

Google AI Studio answered every request of this session with HTTP 402 ("prepayment credits
are depleted"), so the two reviews were done by independent reviewers of another kind: fresh
agents with no access to the author's reasoning, given the document or the diff and asked for
defects. The director asked for Gemini; it should be run over this document and the diff once
the account has credit again.

### 12.1 Design review (before coding)

Fifteen findings; thirteen accepted, two accepted in part.

| # | Finding | Verdict |
|---|---|---|
| 1 | A hidden tab is never disconnected: keep-alives are answered by the browser's network stack, and reader tasks run without frames, so the client's queue grows without bound | **Accepted.** The zone kicks after 10 s without input (both transports); the client's snapshot queue is bounded, oldest out (2.1, 3.3) |
| 2 | The datagram queue is named `incomingMaxBufferedDatagrams` in Chromium and defaults to one; the draft gave no number | **Accepted.** Both names set, 16 datagrams, 250 ms (2.1) |
| 3 | A pinned hash does not make the page servable by IP: WebTransport needs a secure context | **Accepted.** Stated in 2.2; the "LAN by IP" claim is gone |
| 4 | `downlevel_defaults` cannot be requested on WebGL2; a WebGPU canvas is not sRGB | **Accepted.** WebGL2 limits on the GL backend, an sRGB view format (3.2) |
| 5 | `Reject` and `Kick` can be lost when the close follows at once; the close code never reaches the page | **Accepted.** Finish, then a grace before closing (2.1) |
| 6 | PROTOCOL.md 1's zero unidirectional streams cannot carry HTTP/3; session options unspecified | **Accepted.** Already found by the first test; options in 2.1 |
| 7 | `stop`/`reset` through the deref send raw QUIC codes on WebTransport streams | **Accepted.** Methods of the halves with the mapping (2.1) |
| 8 | web-sys needs an unstable-API flag for WebTransport; no Keyboard binding; `gm-hub-proto` pulls quinn, rand and ed25519 into the wasm | **Accepted.** Own bindings, Keyboard Lock in `boot.js`, target-gated dependencies: the wasm tree has no QUIC, no RNG, no tokio |
| 9 | `std::time::Instant` panics at run time on wasm; after a panic callbacks re-enter a dead module; `NetClient::close` blocks | **Accepted.** `web_time`, a grep gate, a panic hook that tells the page, a non-blocking close |
| 10 | No recovery when the WebGPU build gets no device; the "WebGL" build carried both backends | **Accepted.** Reload once with `?gl=1`; the second build draws through WebGL2 only |
| 11 | A `connect=` link works on the production origin | **Accepted.** Development sites only, and the same for a login in a link (5) |
| 12 | The hub's hash in `config.json` goes stale at every restart | **Accepted.** `--web-info-out`, `no-store` (2.3) |
| 13 | Rebuilding the cache index needs a `match()` per entry | **Accepted.** The size is in the key (4) |
| 14 | Keyboard Lock only in fullscreen with a long-press exit; pointer re-lock needs a gesture; no `unadjustedMovement` | **In part.** Documented (3.4, 10); the client keeps winit's pointer lock |
| 15 | The page cannot measure its UDP bytes | **In part.** The gate reads the zone's QUIC statistics for the session; the page's own counters (payload bytes) are reported beside them |

Checked by the reviewer and found right: drop-oldest in the browser's datagram queue; the
rules of `serverCertificateHashes`; no pooling with pinned certificates and one session per
connection in `wtransport`; the datagram overhead inside 1,200 bytes; clean FINs on streams;
`wgpu` without `wgpu-core` and `naga` on WebGPU; `bitcode` on wasm32; synthetic responses in
the Cache API.

### 12.2 Code review (after coding)

Thirteen findings; eleven accepted and fixed, one accepted as the intended behaviour, one
rejected for now.

| # | Finding | Verdict |
|---|---|---|
| 1 | The idle kick fires on a browser still fetching the zone's map (no input before `Content`, which waits behind the fetch); the kick then waits behind the fetch too | **Fixed.** 60 s for the first input, 10 s after it; a disconnect in the backlog is shown at once (2.1) |
| 2 | A link on a production site could carry `script`, `travel-to`, `seconds`, `cache-mb`: a visitor who then logs in has their character played, moved, or their cache resized | **Fixed.** Links set only what is seen; the rest is for development sites; the cap is clamped (5) |
| 3 | `check-web.sh`: an empty value passed every `check_max`; `--hub` without `--browser` ran nothing and passed; failures inside the hub run were swallowed; the clock grep missed the crates the client links; a renamed test module would "pass" with zero tests; the cache cap was checked against the client's own counter | **Fixed**, each: non-numbers fail, the flag combination is refused, the hub run fails the script, the grep covers the linked crates (and `now_secs` no longer exists on wasm), a test run must run tests, and the cache is read back entry by entry through the DevTools protocol |
| 4 | A `close()` during the connect left the session open (the zone kept the body); several early returns dropped the session without closing it; a stalled `Bye` held the close up | **Fixed.** Every way out closes; `closing` is checked after each step; the `Bye` has half a second |
| 5 | A takedown or an eviction during a cache write left an entry the index did not know: uncounted, and served again next session | **Fixed.** Entries being written are not evicted; a write whose entry is gone is deleted (4) |
| 6 | `Welcome.map` went unvalidated into a URL (and natively into a path) | **Fixed** on both platforms (5) |
| 7 | A travel ticket with an unusable address closed the old connection first (native behaviour changed by the refactor) | **Fixed.** The ticket is followed only if this platform can reach the zone |
| 8 | The password stayed in the page's options object and its form field | **Fixed** (5) |
| 9 | Two tabs can hold twice the cap; the last-use order is lost at a reload, so the first eviction of a session is arbitrary and can take the own avatar before it is pinned | **Rejected for now.** Stated in 4 and 10; the cost is a refetch, never the cap of one tab |
| 10 | No cap on queued reliable messages; no timeout on the handshake, the hub's requests, a map | **Fixed** (2.1) |
| 11 | A timed-out or oversized download was dropped but not cancelled: the hub kept sending | **Fixed.** A reader dropped unfinished cancels its stream |
| 12 | The origin allow-list compared raw strings; a session without `Origin` was refused, so bots could not use a restricted listener | **Fixed** (2.1) |
| 13 | A native client whose event loop stalls for 10 s is now kicked where keep-alives used to carry it | **Accepted as intended**: the rule is the same for every client (2.1, PROTOCOL.md 1) |

Checked by the reviewer and found right: the loader's in-flight count under rationed
decoding; no `RefCell` borrow across an await; the bounded snapshot queue; lengths bounded
before reads; the WebTransport error-code mapping; ghosts exempt from the idle kick; the
grace periods not usable as a slow-loris lever; native `Welcome` and map-load ordering
unchanged; `C` free as a key.

### 12.3 Found by running it

- A WebTransport session never established with the zone's transport parameters: zero
  unidirectional streams (2.1).
- `wasm-opt --all-features` emitted an import encoding browsers do not ship ("invalid import
  kind"); the build names the features Rust's wasm target uses.
- The scripted player landed nothing in three runs of about twenty. It was swinging from 120
  units with weapons that reach 72, and aiming bolts at where a strafing duelist had been.
  With the reach, the bolt's speed and its wind-up taken from the content it lands 2 to 11
  hits in 40 s (ten runs, none without).
- On Chromium's software GPU the WebGL2 build runs at 41 fps, and the zone then sends that
  client 19.6 KB/s instead of 8.4 (still inside `[net]`); the WebGPU build on the same
  software GPU holds 60 fps and 8.3 KB/s. Not investigated further: a client slower than the
  tick rate is a client on a machine without a GPU.

### 12.4 Review by Gemini 3.1 Pro, after the fact (2026-10-03)

The Gemini account had no credit when this phase was written (12.1 and 12.2 are an
independent agent's); with credit back, Gemini 3.1 Pro read this document and the whole
commit, asked for what the earlier reviews missed. 3 findings.

1. High, *evicting a model from the GPU revokes it from the browser's cache for the
   session*: **wrong**. `Loader::remove` (which revokes) is reached only from the cache's
   `refuse`, that is a takedown or a model that does not decode; a GPU eviction removes the
   entry and the slot and leaves the store alone.
2. High, *`refuse` does not await `send.finish()`, so the FIN is never sent*: **wrong**.
   `SendHalf` dereferences to `quinn::SendStream`, whose `finish` is synchronous in quinn
   0.11 (it was a future in 0.10).
3. Low, *the control messages queued before the session was live are not counted in
   `tx_bytes` when they are flushed*: **accepted**, fixed.

Its verdict on the earlier reviews: sound.

## 13. Changes in Phase 10 (the screens, docs/CLIENT.md)

- The page's form is the client's login screen (5): `gmStatus` gained `login`, `login-wait`,
  `screen` and `say`; what is typed travels in `globalThis.gmLogin`. A new account types its
  password twice. Fullscreen takes the stage, so the form is there in fullscreen too.
- A link may no longer set `zone` on a production site; `ui-script` is a development option.
- The browser speaks the players' messages to the hub (HUB.md 3.8) and no longer links the
  codec of the hub's whole protocol: 89 KB less. With the screens the WebGPU build is
  **967,052 bytes (324,017 packed)**, the WebGL2 build **2,956,609 (896,912 packed)**; the
  glue, loader and page are 116 KB and 170 KB. The budgets of 9 are unchanged.
- Losing the pointer lock is the Escape key (3.4). A hub session that closed by itself is
  opened again by the next request. After a map download that was cancelled or failed, the
  next entry fetches the map again (it used to play on the one still loaded).
- The gates serve the page from a directory of their own: `target/web/config.json` is no
  longer rewritten by a test run.
- `scripts/web-run.mjs --login EMAIL --password PW [--register]` fills the form by the
  browser's own input events; the driver ends the browser on every way out.

## 14. Changes in Phase 11 (possessions, docs/ITEMS.md)

- The browser shows the inventory, the storage and a stall (ITEMS.md 6) and asks the hub
  for them in the players' messages (`PlayerRequest::Econ`, HUB.md 3.9): the WebGPU build
  is **1,005,086 bytes (335,607 packed)**, the WebGL2 build **2,993,885 (908,530
  packed)**, 38 KB more than Phase 10. The budgets of 9 are unchanged; the megabyte has
  43,490 bytes left.
- **The WebGL2 build drew no town.** The map was black, with the bodies, the stalls and the
  HUD in place: wgpu's GL backend cannot be told what a texture will be viewed as and
  guesses from its layer count (one layer: a plain texture; six: a cube; a larger multiple
  of six: a cube array), and the town has twelve textures where the arena has seven. The
  client said so in the browser's console and played on, and no gate had looked at the
  town in that build since Phase 8. The world's texture array now never has such a count
  (`render::array_layers`), and the three gates that run a browser (`check-web.sh`,
  `check-screens.sh`, `check-items.sh`) fail when the client logs an error, when the
  browser reports a rendering error of its own (`scripts/web-run.mjs` now listens to the
  browser's log as well as the page's console), and on an uncaught exception.
- **The pointer is asked for by the page itself.** winit asked the browser for the pointer
  and let a refusal (no person's click behind the asking: a page that entered the game by
  itself, an entry that took longer than a click lasts) fall into the console as an
  uncaught rejection, on every such entry. The page now asks (`web::ask_for_pointer`) and
  takes the refusal as the answer it is; the next click on the canvas asks again (3.4).
  The gate through the hub clicks the canvas twenty seconds in and asks the browser who
  holds the pointer (`web-run.mjs --click-canvas`): until now no gate had.

## 15. Changes in Phase 12 (people together, docs/PARTY.md)

- The browser shows the people, the trade window and the tavern (PARTY.md 8) and speaks the
  party's messages of protocol v7. **The build got smaller with three screens more**: the
  control stream's one enum, `Control`, carried the code to write a zone's messages and to
  read a client's own; split into `FromClient` and `FromZone` (PROTOCOL.md 19) the WebGPU
  build lost **62,834 bytes**, and dropping the whole-message `{:?}` formats of the client's
  logging **27,741** more. With the screens in it the WebGPU build is **977,242 bytes
  (332,973 packed)**, 27,844 less than Phase 11's, the WebGL2 build **2,972,499 (906,803
  packed)**. The budgets of 9 are unchanged; the megabyte has **71,334 bytes** left.
- `twiggy top` on a build with `CARGO_PROFILE_WEB_STRIP=false CARGO_PROFILE_WEB_DEBUG=0` is how
  the bytes were found: the one codec was 83 KB of the build.
- A trade window polls the hub once a second through the players' messages (`TradeView`);
  nothing else of this phase touches the page, the transport or the cache.

## 16. Changes in Phase 13 (sound, docs/SOUND.md)

- The browser plays through its own Web Audio nodes (SOUND.md 5): one `AudioContext` made
  by the handler of the first `pointerdown` or `keydown` on the page (the click that takes
  the pointer, 3.4, usually), the twenty patches as `AudioBuffer`s, a pool of 32 gain and
  panner chains, a source node per cue. Nothing of the native mixer is compiled in. Headless
  Chromium gives the context a fake output: it runs after the gate's click, and the gate
  reads the page's `sound:` line out of `GM-DONE` (cues started, the context's state, the
  hash of every patch rendered, which must equal the native build's).
- **Measured**: the WebGPU build is **1,009,465 bytes** (345,287 packed; +32,223 for the
  phase; 977,242 before), the WebGL2 build **3,003,328** (918,847 packed; +30,829). `kira`
  would have cost +62,142 of wasm with no sound made (SOUND.md 1). The budgets of 9 are
  unchanged; the megabyte has **39,111 bytes** left. The web-sys bindings for the audio
  nodes cost mostly in the glue (the WebGPU build's grew 6,267 bytes, to 110,616); what the
  phase added to the wasm is the synthesis, the cue rules and the graph. Two things of it
  were found with `twiggy` and removed: a stable sort to pick the eight nearest cues (≈7.5 KB
  of sort machinery, now a selection) and a second hash map of fed ticks.
