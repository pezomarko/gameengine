# Wire Protocol

Status: v17 (the off hand and the Taunt's source, section 29; v16 the six elements, section 28; v15 the RPG body's facing, section 27; v14 the crouch seen, section 26; v13 and before as the sections say; v9 after Phase 14: the ability a stance belongs to, section 21; v8 of Phase 14: what a body holds, `Look`, and the pack's prop keys, section 20; v7 of Phase 12: two types for the control stream's two directions, parties, lines through the hub and a trade asked for, section 19; v6 of Phase 11: buying at a stall and wearing, section 18; v5 of Phase 9: reports, section 16; v4 of Phase 8: WebTransport as a second carrier, section 15; v3 of Phase 7: companions
and command, section 14; v2 of Phase 3 with the reliable messages of Phases 4 and 6, sections
12 and 13). `gm-net` implements exactly this document; the test vectors in section 2
are unit tests. Decisions from PLAN.md 2.1, 2.3 and 11.3 are binding here. When the code and this
document disagree, the document wins and the code is wrong; changes to either go in one commit.

Protocol version byte: **7**. Any change to sections 2–5 or to the layout of a message of
the control stream (`FromClient`, `FromZone`; one enum, `Control`, before v7) bumps it. Section 11 lists what v2 changed over v1, section 14 what v3 changed over
v2, section 15 what v4 changed over v3, section 16 what v5 changed over v4, section 18 what
v6 changed over v5, section 19 what v7 changed over v6.

Section 10 records the independent design review this version went through and what changed.

## 1. Transport

- QUIC. Native clients use `quinn`; browsers use WebTransport, a session of which carries the
  same datagrams and the same control stream (`docs/WEB.md` 2: the zone listens for it on a
  second port; what differs there is listed in section 15). Same semantics, one protocol.
- **Unreliable datagrams**: input frames (client → server), snapshots (server → client), pings.
  One datagram is one message; a message never spans datagrams. Payloads are capped at
  **1,100 bytes** (`MAX_DATAGRAM_PAYLOAD`), well under the 1,200-byte initial QUIC MTU minus
  framing, so nothing depends on MTU discovery or on VPN/PPPoE paths. A snapshot that would not
  fit drops the farthest entities' updates for that tick (they are still tracked and catch up on
  the next tick); the server counts the event.
- **Reliable stream**: one bidirectional control stream opened by the client right after the
  handshake. Messages are `bitcode`-encoded values of `FromClient` one way and `FromZone`
  the other (section 8), each prefixed by a big-endian **u16 length**. A message longer than 65,535 bytes is a protocol error.
- **Congestion control**: a fixed window (64 KiB, `FixedWindow`) that never shrinks. The traffic
  is a small application-limited rate; a loss-based controller would throttle 64 Hz datagrams
  under the random loss we must survive (3% loss halves NewReno's window every few seconds) and
  queue stale snapshots. The datagram send buffer is small (4 KiB) so quinn drops the oldest
  queued snapshot instead of delivering it late. The per-player budget gate (section 9) is what
  keeps the rate honest.
- Certificates: the zone presents a self-signed certificate generated at start (or loaded from a
  file). Clients trust a certificate DER passed on the command line or, in Phase 4, receive the
  zone's certificate hash from the hub in the session token. There is no "accept anything" mode.
- Idle timeout 10 s, QUIC keep-alive every 2 s. Keep-alives keep a connection alive whether
  or not its client still plays, so the zone has a rule of its own: **a client that has sent
  no input datagram for 10 s is kicked** (`Kick("no input for 10 seconds")`; 60 s for the
  first input after a join, because a client may be loading the map; a ghost waiting for
  another zone's claim is exempt).
- A `Reject` or a `Kick` is followed by the close, not accompanied by it: after `Reject` the
  zone finishes the stream and waits up to 1 s for the client to hang up, after `Kick` it
  closes 0.5 s later. A connection closed under an unread message can take the message with
  it.
- The entry token (hub-signed, ed25519, HUB.md 3.1) rides in `Hello.token`. A zone started
  without `--hub` accepts an empty token (development and tests); under a hub the token is
  mandatory, the zone verifies it offline, claims the character at the hub, and takes the
  player's name and build from the hub's answer (`Hello.name` and `Hello.build` are ignored).

## 2. Bit packing

All datagram payloads use one bit writer. Rules:

1. Bits are written **MSB-first** into bytes: the first bit written is bit 7 of byte 0.
2. `bits(v, n)`: the `n` low bits of `v`, most significant bit first. `n` is 0..=64.
3. `bool` = `bits(v, 1)`.
4. `uvar(v)`: unsigned variable length. Groups of 7 bits from the least significant group up;
   each group is written as one continuation bit (1 = another group follows) then the 7 data bits.
   At a byte boundary this is exactly LEB128.
5. `svar(v)`: zigzag (`(v << 1) ^ (v >> 63)`) then `uvar`.
6. `finish()` pads the last byte with zero bits. Readers must tolerate trailing pad bits and must
   fail (not panic) on reading past the end.

Test vectors (hex bytes after `finish()`):

| Writes | Bytes |
|---|---|
| `bits(0b101, 3)`, `bits(0b1111, 4)` | `BE` |
| `uvar(0)` | `00` |
| `uvar(127)` | `7F` |
| `uvar(128)` | `80 01` |
| `uvar(300)` | `AC 02` |
| `svar(0)`, `svar(-1)`, `svar(1)`, `svar(-2)` | `00 01 02 03` |
| `svar(-64)` | `7F` |
| `svar(64)` | `80 01` |
| `bits(1, 1)`, `uvar(300)` | `D6 01 00` |
| `bits(0xABCD, 16)`, `bool(true)` | `AB CD 80` |

Quantization (all deterministic, round half away from zero):

| Quantity | Encoding |
|---|---|
| position component | `svar(round(x * 4))`: 1/4 world unit (≈ 0.8 cm) |
| velocity component | `svar(round(v * 8))`: 1/8 u/s |
| yaw | `bits(round(yaw mod 360 * 10) mod 3600, 12)`: 0.1° |
| pitch | `bits(round((clamp(pitch, -90, 90) + 90) * 10), 11)`: 0.1°, 0..=1800 |
| move axis | `bits(round(clamp(a, -1, 1) * 127) as i8 as u8, 8)` |

Dequantization divides back; the simulation runs on the dequantized values on both sides so the
server compares like with like when it reconciles.

Ticks are `u32` and compare with **wrapping arithmetic** (`a.wrapping_sub(b) as i32`), like TCP
sequence numbers. Zones restart long before a wrap (2.1 years at 64 Hz), but nothing may break
if one happens.

## 3. Datagram header

| Field | Bits | Notes |
|---|---|---|
| version | 8 | must equal 1, else the datagram is dropped and counted |
| kind | 8 | 0 `Input`, 1 `Snapshot`, 2 `Ping`, 3 `Pong` |

## 4. Input datagram (client → server)

Sent once per client tick, as soon as the tick's input is sampled. Carries the newest
**1..=4 consecutive frames**: three consecutive datagram losses (0.0003% at 3% loss, once in
a few hours at 64 Hz) cost one tick of movement, anything less costs nothing (PLAN.md 11.3).

| Field | Bits | Notes |
|---|---|---|
| ack_tick | 32 | newest server tick whose snapshot was decoded; 0 = none yet |
| view_tick | 32 | server tick the client is displaying for other entities (its interpolation time); 0 = none. Used for melee lag compensation (section 7.4) |
| frame_count − 1 | 2 | 1..=4 frames |
| first_tick | 32 | client tick of the oldest frame; frame `i` is tick `first_tick + i` |
| frames | 100 each | oldest first (97 before v18) |

Frame:

| Field | Bits | Notes |
|---|---|---|
| buttons | 16 | bit 0 jump, 1 crouch, 2 primary, 3 secondary, 4 guard, 5–8 ability 1–4, 9 interact, 10 unused (the viewport switch until v11: the camera is the mode's, MODES.md 2), 11 command (the command stance, COMPANIONS.md 5.1), 12 reload, 13 scope held (MODES.md 3.2), 14 use a kit (MODES.md 11.3, v12), 15 reserved (must be 0) |
| yaw | 12 | 0.1° |
| pitch | 11 | 0.1° |
| forward | 8 | i8, −127..=127 → −1..=1 |
| side | 8 | i8, right positive |
| ability | 8 | slot activated this tick (1-based), 0 = none |
| held | 2 | the weapon in hand of a gun build (MODES.md 3.7): 0 the primary, 1 the secondary, 2 the knife; 3 is malformed |
| target | 32 | the body an activation this tick is aimed at (MODES.md 5.3); 0 = none |
| use_slot | 3 | v18: the item cell a `use` this tick is of (LOOK.md 3.2), 1–4; 0 = none named, which is the first cell's; 5–7 are malformed |

Movement direction lives in `forward`/`side` only; there are no forward/back/left/right buttons
(the Phase 0 skeleton listed both, which was redundant).

Server rules (the frame ledger):
- **Every frame is executed exactly once**, in tick order. Frames with
  `tick <= last executed tick` and frames already queued are ignored; a frame from a datagram
  that arrives after a newer one still slots into the queue in tick order (UDP reorders), so the
  redundancy is never wasted. Each queued frame keeps the `view_tick` of the datagram that carried
  it. A frame is never reused: when no frame is available the player is simply not simulated
  that tick (no movement, no gravity), and the tick is counted as *starved* for diagnostics.
  Reusing or inventing frames would make `last_input_tick` lie to the client's reconciliation
  (section 7.2).
- Dejitter: the server keeps **one frame in reserve** (a frame runs only when a newer one is
  already queued), so one tick of arrival jitter never starves the simulation, at the cost of one
  tick (15.6 ms) of added input latency. With four or more frames queued it runs two per tick
  until the queue drains. Measured in the acceptance test, this took starvation from 14% of
  ticks to about 1% at 75 ± 10 ms one-way latency.
- Rate limit: a token bucket per client with **1 credit per server tick, burst 8**, one credit
  per executed frame, and at most **3 frames per server tick**. A client cannot execute more than
  64 frames per second on average, so sending inputs faster than the tick rate buys nothing.
  Frames arriving over budget wait in the queue; beyond 32 queued frames the oldest are dropped
  (the client will be reconciled to the server's state; that is the price of flooding).
- `ack_tick` must name a tick actually sent to this client within the last 64 ticks; anything else
  is treated as 0 (full snapshot follows).
- `view_tick` is clamped to `[server_tick - max_rewind, server_tick]` where
  `max_rewind = min(13, half_rtt_ticks + 8)`: the client's measured one-way latency (QUIC RTT
  estimate / 2) plus the interpolation delay (6 ticks) plus 2 ticks of jitter, never more than
  200 ms. A low-ping client cannot claim a 200 ms rewind.
- Malformed datagrams (bad version, short, reserved bits set) are dropped and counted per client;
  100 in a minute kicks.

## 5. Snapshot datagram (server → client)

One per client per server tick. Delta-compressed against the **baseline**: the snapshot of
`ack_tick` the client last acknowledged, provided it is at most 60 ticks old and still in the
server's per-client history; otherwise `baseline_tick = 0` and the snapshot is full.

The server keeps, per client, the **reconstructed** snapshot for each tick it sent: exactly the
entity table the client rebuilds from the wire (section "Reconstruction" below). Deltas are
always computed against that table, never against what happened to be on the wire.

| Field | Bits | Notes |
|---|---|---|
| server_tick | 32 | ≥ 1 |
| baseline_tick | 32 | 0 = full snapshot |
| last_input_tick | 32 | client tick of the last frame executed; drives reconciliation. 0 = none |
| own block | | the client's own resources and statuses, always in full (below) |
| entity_count | uvar | |
| entities | | records, ascending id |
| removed_count | uvar | entities present in the baseline and gone now |
| removed | uvar each | ids |

Own block (never delta-encoded: it is small and the client must adopt it exactly):

| Field | Bits | Notes |
|---|---|---|
| stamina | uvar | whole points |
| focus | uvar | whole points |
| status_count | 4 | 0..=8 |
| per status: status | 5 | `Status` index (MATRIX.md 8; 4 bits until v17) |
| per status: remaining | uvar | frame ticks left, relative to `last_input_tick` |
| per status: magnitude | svar | `round(magnitude × 16)` |
| per status: stacks | 3 | |
| per status: source | uvar | v17: the entity that applied it (0: nobody). A Taunt's is who the body is turned to (MATRIX.md 8) |
| guns | 1 | v11: 1 for a gun build (MODES.md 3.8), then for the primary and the secondary each `magazine` (uvar) and `reserve` (uvar), and 1 bit: the one in hand is being reloaded |
| bar | 4 × uvar | v18: the item bar (LOOK.md 3.2): how many of each cell's stack the body carries, then 3 bits: the cell in use, 1–4, 0 for none (v12–v17: one uvar, the kits, and one bit) |

Entity record:

| Field | Bits | Present when |
|---|---|---|
| id | uvar | always |
| mask | 9 | always. bit 0 SPAWN, 1 POS, 2 YAW, 3 PITCH, 4 VEL, 5 ANIM, 6 HEALTH, 7 FLAGS, 8 STATUS |
| kind | 4 | SPAWN. 0 player, 1 projectile, 2 area |
| spawn info | | SPAWN, by kind (below) |
| pos | 3 × svar | POS. **Absolute** quanta when SPAWN is set, **delta** against the baseline record otherwise |
| yaw | 12 | YAW |
| pitch | 11 | PITCH |
| vel | 3 × svar | VEL. Absolute when SPAWN (or the baseline record has no velocity), delta otherwise |
| anim | 8 (+ uvar) | ANIM. When the stance is a script's (windup 3, swing 4, recovery 5, cast 10), the **acting ability** follows as a uvar: the `AbilityId` of the script (the pack's index and one). ANIM is set when either changes (v9, section 21) |
| health | uvar | HEALTH |
| flags | 10 | FLAGS. bit 0 alive, 1 on ground, 2 guarding (block held), 3 dashing, 4 jump held, 5 script running, 6 parry window or whiff recovery, 7 commanding (in the command stance; own entity only), 8 crouched (MODES.md 3.5; v14, every body), 9 in the RPG mode (MODES.md 5.1; v15, every body) |
| status | 24 | STATUS. A bit per `Status` index: the cosmetic summary for other entities (auras); 16 bits until v17 (`STATUS_BITS`) |

Spawn info: player → `frame` 2 bits (0 colossus, 1 striker, 2 caster, 3 infiltrator), `team`
2 bits (0 none), `aspects` 6 bits (a bit per element, MATRIX.md 5; v16, none is neutral), `armour` 2 bits (cloth,
leather, mail, plate): everything that makes a build readable at a glance. Projectile → `owner`
uvar, `def` uvar (ability index in the owner's kit), `input_tick` uvar (the owner's client tick
that fired it, for matching the owner's predicted copy). Area → `owner` uvar, `def` uvar (0 when
triggered by a projectile or a parry), `radius` uvar (largest extent in whole units), `harmful`
1 bit (it deals damage, or puts a status on whoever stands in it that is not Regen, Haste,
Fortify or Stealth: what its look says, so a client can draw a telegraph and a sanctuary
differently); its `pos` is the origin and it is removed when it expires.

Entity ids are **monotonic** within a zone process and never reused, so a delta can never be
applied to a different entity's baseline record.

Rules:
- A field is sent when it differs from the baseline record, or when there is no baseline record
  (SPAWN set: every field the server sends for this entity is included). SPAWN therefore repeats
  on every tick until the client acks a snapshot containing the entity; the client cannot miss it.
- **Reconstruction**: the decoded snapshot is the baseline table with `removed` ids deleted,
  listed records applied (fields not received are copied from the baseline record), and every
  baseline entity that is neither listed nor removed **carried forward unchanged**. Both sides
  perform this identically; the server's per-client history stores the result.
- An entity that is unchanged since the baseline is not listed at all (carry-forward costs zero
  bytes). An entity that is not scheduled this tick by its distance band is likewise not listed
  and is **not** in `removed`.
- `vel`, the `jump held`, `script running` and `commanding` flags and the own block are sent
  for the **own entity only**. `health` is sent for the own entity, for the bodies of the
  viewer's party (its companions) and for creatures (COMPANIONS.md 13); a record that is not
  scheduled this tick keeps the health of its baseline. Nothing else about other players'
  resources is sent; their `status` mask is the aura, not the numbers.
- Only entities in the PVS of the client's eye leaf are sent (PLAN.md 1.2, 8). An entity counts
  as in the PVS when the leaf of its origin or of its eye point is in the row. The client's own
  entity is always sent, and so is any player within **128 u** of the client's eye whatever the
  PVS says: a body that close can block the client's movement, and prediction cannot handle a
  wall it was never told about (a pillar corner is exactly where this happens). At 4 m the
  anti-ESP value of hiding it is nil. Entities leaving the PVS appear in `removed`.
- **The squad** (COMPANIONS.md 5.2): a client's own companions are always sent, wherever they
  are. While its body is in the command stance **with the button held**, it is also sent
  every entity in the PVS of each living companion's eyes, with the distance band taken from
  the nearest squad member. Nothing is sent that no squad member could see.
- Distance bands (PLAN.md 1.2), measured from the client's eye to the entity's origin:
  full rate to 512 u; every second tick to 1,536 u; every sixth tick beyond (≈ 10.7 Hz).
  Those are ticks of the 64 Hz combat rate; at another rate a band keeps its time, not its
  count, rounded to the nearest tick and never under one (`gm_net::bands`): a 20 Hz town lists
  the half band every tick and the far band every second tick (100 ms), so the far interval
  never exceeds the client's interpolation delay (7.3). Projectiles, the own entity and any
  entity absent from the baseline (first sight) are always full rate.

Client rules:
- The client keeps the last 64 reconstructed snapshots keyed by `server_tick`. A snapshot whose
  `baseline_tick` is unknown is dropped and counted (the server's baseline is the client's own
  ack, so this only happens after the client evicted it).
- Every input datagram acks the newest reconstructed `server_tick`.
- A `removed` entity stays in the interpolation buffer until the client's render time
  (section 7.3) passes the removal tick; it is not hidden the instant the datagram arrives, or
  the last 100 ms of its visible movement would be cut.

## 6. Ping / Pong

`Ping`: `bits(client_ms, 32)`. `Pong` echoes the same 32 bits. Reserved for Phase 4 clock
statistics; Phase 2 uses the QUIC RTT estimate on both ends.

## 7. Simulation contract

### 7.1 Ticks
Server ticks start at 1 and advance at the zone rate (64 Hz combat). Client input ticks start
at 1 and advance with the client's own fixed step; they are **not** synchronised with server
ticks. Reconciliation works in client-tick space (`last_input_tick`), interpolation in server-tick
space (`server_tick`). No clock synchronisation is needed for either. The server may execute 0,
1, 2 or 3 of a client's frames in one server tick (section 4); because every frame runs exactly
once, the state after frame `t` is the same on both sides regardless of when it ran.

### 7.2 Prediction and reconciliation (own entity)
1. Each client tick: sample input, quantize it to the wire frame and **dequantize it back**
   (the server runs exactly those values), run `gm-core` (`sim::step_mover`) for the own entity
   against the newest known positions of other players, store `(tick, input, predicted mover
   state)` in a ring of 128.
2. On a snapshot with `last_input_tick = t`: compare the server's own-entity state with the
   predicted state stored for `t`. If position differs by more than **2 u** or velocity by more
   than **16 u/s**, replace the state at `t` with the server's and replay inputs `t + 1 ..= now`.
   Below tolerance nothing happens (quantization noise, PLAN.md 11.3 "tolerance-based"). 2 u is
   6 ms of running; anything smaller is invisible, anything larger is a real disagreement (usually
   a body-block against a player whose position the client only had interpolated).
3. Corrections are applied to the simulation state instantly; the renderer may smooth the eye
   over up to 100 ms (cosmetic, never affects the simulation). Before a server position is
   adopted it is nudged out of solid (`movement::nudge_position`, QuakeWorld's
   `PM_NudgePosition`): the rounded position can land exactly on a clip plane, which the hull
   tracer classifies as solid and then refuses to move, while the server's true position sits
   `DIST_EPSILON` away. Without the nudge a client pressed against a wall corner froze for as
   long as the server kept it there (found by the acceptance test's correction log).
4. Ability activation, cooldowns, stamina, focus, self-applied statuses, the guard state and
   every `MoveSelf` are part of the predicted mover state and replay deterministically. Attacks,
   projectiles, areas and statuses put on us by others are the server's: the own block's
   resources and statuses replace the predicted ones at the acknowledged frame before the
   replay (a status's `remaining` is re-anchored to the client's frame clock), a cleared `script
   running` flag drops a predicted script the server interrupted, a cleared `guarding` flag
   releases a broken block, and a cleared `parry` flag closes a parry window the server resolved
   (a landed parry ends at once, with no whiff recovery). Projectiles are not predicted beyond spawning a local copy that is
   replaced by the server's when a record with matching `(owner, input_tick)` arrives.
5. A correction counts as *explained* in the diagnostics when, within 8 server ticks, the own
   health, status mask or guard state changed, the entity died or respawned, another body was
   within 128 u (always sent, so it can block), the own entity was in a positional ability, or
   only the velocity disagreed (wall contact resolved a frame apart because of the quarter-unit
   rounding of the adopted position); everything else is an unexplained correction and the
   netcode tests bound those.

### 7.3 Interpolation (other entities)
Render time for other entities is `newest_server_tick - delay`, with `delay` = 6 ticks (94 ms)
plus one extra tick per snapshot gap observed in the last second, capped at 13. Those are ticks
of the 64 Hz combat rate; at another rate the delay keeps the time, not the count:
`ceil(6 × hz / 64)` and `ceil(13 × hz / 64)`, never under 2 ticks (a snapshot on each side) and
a ceiling at least 2 above the floor. A 20 Hz town shows others 2 ticks (100 ms) behind, 5
(250 ms) at worst. The delay shrinks
by one tick per second without gaps. Positions and angles are interpolated between the two
**samples** bracketing the render time. A sample is the entity's record at a tick on which it
changed; a snapshot that carries the record forward unchanged (the band skipped the tick, or
the body did nothing) adds none, it only marks the entity seen. Interpolating between the
carried copies instead would hold a far body for five ticks and cross the whole gap in one:
the walk cycle stalling and the body jumping forward. When a body changes after more than one
band interval of rest, a rest sample one interval back is added first (the body was where it
was through the last tick its band listed it), so the move starts from there rather than
snapping to where it already is. When only the older sample exists (a gap, or a body standing
still), the entity holds its last position; no extrapolation. (Draining the buffer by
time-scaling instead of stepping is a Phase 3 refinement.)

### 7.4 Lag compensation (melee)
A `MeleeArc` activated by a frame is resolved against other entities' positions at the
attacker's `view_tick` (the interpolation time the attacker was looking at), rewound from the
server's position history and clamped as in section 4. The server keeps 32 ticks of history per
entity (the rewind bound plus the longest windup and active window). Projectiles are **not** rewound: they spawn at the attacker's current server position
and are then stepped forward `server_tick - view_tick` ticks (after the same clamp) in the
spawning tick. Each caught-up tick is swept against the targets' **historical** positions at that
tick, so the catch-up hits exactly what the attacker saw and nothing more; a client that inflates
its measured latency (delaying acknowledgements) only buys itself older target positions. The
clamp bounds the catch-up to 200 ms whatever the client claims.

### 7.5 Collision
World: BSP hull traces (gm-bsp). Players against players: axis-aligned box sweeps with the same
32 × 32 × 56 hull, resolved inside the movement trace so sliding and stepping work against
players exactly as against walls. One exception, the same on both sides: a body whose own box,
as its step begins, is inside another's by more than a unit on every axis is not held by that
body for that step (two bodies that ended up in each other, at a crowded spawn or after a
blink, walk apart instead of being stuck for good). Hitboxes for damage are the archetype capsules
(VOCABULARY.md 3). Projectiles are swept as spheres against the world (point hull) and against
capsules.

## 8. Reliable messages

The control stream's two directions are two types (since v7, section 19): what a client
says and what a zone says. Each has its own numbering; three numbers are the same in every
version, so that a client and a zone of different versions can read each other's
handshake: `Hello` is 0 of `FromClient`, `Reject` is 14 and `Kick` is 25 of `FromZone`.
What a version adds goes at the end of its enum.

```
enum BuildChoice { Preset(String), Custom(Build) }

enum FromClient {
    Hello { version: u16, name: String, token: Vec<u8>, build: Option<BuildChoice>, team: u8 },   // 0
    Chat(String),
    Respec(BuildChoice),                       // applied at the next respawn (MATRIX.md 9)
    Travel(String),                            // to another zone (HUB.md 3.3)
    StallOpen,                                 // on the market tile the player stands on
    StallClose,                                // the own stall
    Order { slots: u8, order: Order },         // to the own squad, from the stance (COMPANIONS.md 5.3)
    Bye,
    Report { target: u32, reason: ReportReason },             // ANTICHEAT.md 5
    StallBuy { stall: i64, listing: i64, price: i64 },        // at the stall the player stands at (ITEMS.md 5)
    Wear { item: i64 },                        // an item of the inventory (ITEMS.md 2)
    TakeOff { item: i64 },
    // v7 (PARTY.md 4)
    PartyInvite { name: String },              // a character anywhere in the game, by name
    PartyAnswer { from: String, join: bool },  // the invitation of that character
    PartyLeave,
    PartyRemove { name: String },              // the leader takes a member out
    PartySay(String),                          // a line to the party: chat, with chat's limits
    Whisper { to: String, text: String },      // a line to one character anywhere in the game
    TradeAsk { with: u32 },                    // ask a body here for a trade, or answer its asking
}

enum FromZone {
    Squad(Vec<SquadEntry>),                    // to a commander: its squad, in slot order
    OrderRefused(String),
    Encounter { name: String, state: EncounterState },
    Loot { encounter: String, items: Vec<String>, coin: u32 },
    ReportResult(Result<(), String>),
    Trial { key: String, name: String, passed: bool, detail: String, secs: u32 },
    BuyResult { listing: i64, result: Result<(), String> },   // the answer to StallBuy, always
    WearResult { item: i64, result: Result<(), String> },     // the answer to Wear / TakeOff, always
    Welcome { entity: u32, server_tick: u32, hz: u16, map: String, map_hash: u64 },
    Content { pack: ContentPack, own: Build, team: u8 },      // right after Welcome
    RespecResult(Result<(), String>),
    BuildApplied(Build),                       // the respawn switched the build
    TravelTicket { zone: String, addr: String, cert_der: Vec<u8>, token: Vec<u8>, web: Option<WebAddr> },
    TravelRefused(String),
    Reject(String),                            // 14, whatever the version
    Roster(Vec<PlayerEntry>),                  // to a joiner: everyone here, itself included
    PlayerInfo { id: u32, name: String, team: u8, model: Option<[u8; 32]>, kind: BodyKind },
    ModelRevoked([u8; 32]),                    // forget it, delete it (MODELS.md 7, 8)
    Stalls(Vec<StallEntry>),                   // to a joiner: every open stall of the zone
    StallOpened(StallEntry),
    StallClosed(i64),
    StallResult(Result<(), String>),           // the answer to StallOpen / StallClose
    PlayerLeft(u32),
    Killed { victim: u32, killer: u32 },       // killer 0 = world
    ChatFrom { from: u32, text: String },      // from 0: a line of the zone itself
    Kick(String),                              // 25, whatever the version
    // v7 (PARTY.md 4)
    Party(Vec<String>),                        // the hub's word on the client's party: names, the leader first
    Invited { from: String },
    Heard { channel: u8, from: String, text: String },        // 1 the party's, 2 a whisper, 3 one's own whisper as it went out
    TradeAsked { from: u32 },                  // the body `from` asks for a trade
    TradeOpened { trade: i64, with: String },  // the hub opened it: the window is the hub's from here
}

enum BodyKind { Human, Companion { owner: u32 }, Creature { def: u16 } }
enum Order { Follow, Hold, MoveTo([f32; 3]), Attack(u32) }
enum EncounterState { Engaged, Reset, Cleared { secs: u32 } }
struct SquadEntry { id: u32, name: String, role: u8, order: Order, max_health: u16, recruit: bool }
struct PlayerEntry { id: u32, name: String, team: u8, model: Option<[u8; 32]>, kind: BodyKind }
struct StallEntry { id: i64, pos: [f32; 3], yaw: f32, owner: String, frame: u8, armour: u8,
                    model: Option<[u8; 32]> }
```

`Build` and `ContentPack` are `gm-core::build` types encoded with `bitcode` (the vocabulary
is non-recursive for exactly this reason, VOCABULARY.md 14).

Handshake: connect → client opens the control stream → `Hello` → `Welcome` then `Content` (or
`Reject`, then close). The server sends snapshots from the tick after `Welcome`; the client
sends inputs after `Content`, since it cannot predict without the kit. `map_hash` is FNV-1a 64
of the `.bsp` bytes; a mismatch is a client-side error ("wrong map build"). `Hello.build` names
a preset of the zone's content or carries a full build; the zone validates it against MATRIX.md 9
and rejects the join with the reason when it fails (`None` = the zone's default preset).
`Hello.team` 0 lets the zone balance; the zone keeps the smaller team filled first. A `Respec`
is validated immediately (`RespecResult`) and takes effect at the player's next respawn, when
`BuildApplied` tells the client to switch its prediction; `BuildApplied` is also sent on every
respawn so a client can never run the wrong kit for long. The content pack is 5–10 KB on the
wire and must fit one control message (65,535 bytes).

Travel (HUB.md 3.3): `Travel(zone)` asks the zone to hand the character to another zone; the
zone answers `TravelTicket` (the other zone's address, certificate and a fresh token) or
`TravelRefused`. On a ticket the client says `Bye`, connects to the other zone with the token
in `Hello`, and loads the map that zone's `Welcome` names. Its body stays here as a ghost
(visible, hittable, no inputs run) until the other zone claims it or 10 s pass.

`Hello.name`: 1..=24 bytes of printable UTF-8 after trimming; anything else is rejected.

Who is here (Phase 6): a joiner receives one `Roster` with every player of the zone, itself
included (400 players with 24-byte names and models fit one message), and everybody else one
`PlayerInfo`. `model` is the id of the avatar model the player wears (MODELS.md 7), absent for
the frame's mannequin; a `PlayerInfo` for a known id replaces what was known (a respawn on
another frame changes what is worn). Model ids never ride in snapshots. `ModelRevoked` tells
everyone to forget a model that was taken down.

The market (ECONOMY.md 7): a joiner receives `Stalls`, then `StallOpened` and `StallClosed` as
they happen. A stall's keeper is drawn from the `StallEntry` (frame, armour class, model) as a
body that does not move; it costs no snapshot bytes. `StallOpen` and `StallClose` are answered
by `StallResult`; the zone forwards at most one such request per player per second to the hub
and drops the rest unanswered. `StallBuy` shares that gate and `Wear` and `TakeOff` have one
of their own, and all three are always answered (`BuyResult`, `WearResult`), a refusal by the
gate included: section 18, ITEMS.md 5. A map has at most 512 stall tiles, so `Stalls` always
fits one message.

Minds (COMPANIONS.md): `Roster` and `PlayerInfo` list companions and creatures like players,
with their `kind` (a creature's `def` indexes the content pack's creatures, which is where a
client gets its name and its maximum health). A commander is sent `Squad` whenever a member or
an order of its squad changes, also when an order ends by itself; `role` is 0 heal, 1 tank,
2 scout, 3 dps. `Order` is accepted only from the command stance with the button held, at most
eight a second, for the squad slots named by the bits of `slots`; `Attack` must name a living
body the client is being sent, `MoveTo` a point with a way to it. Anything else is answered
`OrderRefused`. `Encounter` goes to the participants and to whoever stands in sight of the
encounter's creatures; `Loot` and `Trial` to the human they concern (`detail` says why a trial
was not passed, or, for a pass, what the ledger said).

Chat (CLIENT.md 5): `Chat(text)` is relayed to everybody in the zone as `ChatFrom { from,
text }`, `from` being the speaker's entity id. The zone checks it where it arrives, in the
connection's own task, before the zone's thread sees it: a line is 1 to **200** characters
after trimming, with no control character and none that cannot be seen (zero-width and
directional marks). An **account** may say **5 lines in 10 seconds** (a bucket of 5, one
back every 2 s; under a hub the bucket is the account's, shared by its characters and kept
a minute past its last connection, so that coming back does not fill it; without a hub it
is the connection's). A line over that, and a line that is not one, is not relayed but
answered to its sender alone (`ChatFrom` with `from` 0: the zone's own voice, which is
also how a client tells such a line from a player's). Refusals are forgotten one every
ten seconds; **thirty** that are not end the connection (`Kick` "flooding the chat"). The
zone as a whole relays at most **10 lines a second** (a burst of 30); a line over that is
dropped and its sender told. A character the receiver's font lacks is drawn as `?`.

A reliable message is never dropped, with one exception: a client whose queue of 256
undelivered messages is full is disconnected, but a chat line is not queued for a client
whose queue is half full (somebody else's talk is not worth a connection, and the room
that is left is for what must arrive).

## 9. Budgets and the acceptance test

Phase 2 acceptance (PLAN.md 11.8): playable at 150 ms round trip and 3% datagram loss in
`turmoil`; under **30 KB/s per player** in each direction with 16 players, measured as UDP bytes
reported by QUIC (`ConnectionStats`), so QUIC overhead counts. `budgets.toml [net]` holds the
number; `scripts/check-netcode.sh` gates it.

"Playable" is asserted, not felt. The turmoil test (`crates/gm-server/tests/netcode.rs`) runs
16 bots for 30 simulated seconds at 75 ms one-way latency (±10 ms jitter) and 3% independent
loss each way, and asserts:
1. Reconciliation corrections above tolerance that have no visible cause (no hit, no death or
   respawn, no other body within blocking distance): fewer than 1 per bot per 10 s. Corrections
   caused by the server pushing the player (knockback, body-blocks against players whose
   positions the client only had a few ticks stale) are counted and reported but are legitimate.
2. Input starvation: under 1% of server ticks per bot.
3. Snapshot gaps seen by a bot: never more than 4 consecutive ticks (62 ms) missing.
4. Melee with lag compensation: a swing aimed at where the attacker *sees* a target moving at
   full speed registers on the server.
5. Bytes per player per second in both directions under the budget.

Expected at 64 Hz with 16 players in one room: snapshot ≈ 14 B header + 16 × ~6 B records ≈
110 B payload + ~45 B QUIC/UDP/IP framing ≈ 155 B × 64 ≈ 10 KB/s down; inputs ≈ 46 B payload
+ framing ≈ 90 B × 64 ≈ 5.8 KB/s up.

## 10. Design review log

**2026-09-30, v1 draft reviewed by Gemini 3.1 Pro** (independent review requested before
implementation; verdicts are ours):
- Accepted: baseline must be the reconstructed table, not the wire payload (carry-forward rule
  made explicit); reused frames on input starvation would desync reconciliation (frames now run
  exactly once, starvation just skips the tick); catch-up was a speed hack (token bucket added);
  `view_tick` must be bounded by measured latency (clamp added) which also bounds the projectile
  forward step; removed entities must leave at render time, not on arrival; 0.5 u tolerance was
  too tight (2 u / 16 u/s); 4 redundant frames instead of 3; loss-based congestion control would
  throttle datagrams (fixed window); payload cap 1,100 B; monotonic entity ids; wrapping tick
  arithmetic; the acceptance test list in section 9.
- Rejected: "projectile SPAWN can be missed" (SPAWN repeats until acked by construction);
  "drop the removed list" (PVS-boundary flicker is rare and explicit removal is what makes the
  client's state provably equal to the server's; revisit if measured); Cubic/BBR instead of a
  fixed window (the rate is known and gated; a fixed window is what a raw-UDP game does anyway).
- Deferred: time-scaled interpolation buffer drain (Phase 3), `SO_RCVBUF` tuning on the real
  socket (Phase 4 with the 200-bot swarm).

**2026-09-30, implementation reviewed by Gemini 3.1 Pro** (after the acceptance test passed):
- Accepted: late datagrams' frames were dropped because they compared against the newest queued
  tick (now inserted in tick order, duplicates ignored, only executed frames refused); `view_tick`
  was taken from the newest datagram instead of the one that carried the frame (now per frame);
  the projectile forward step swept current positions (now historical, see 7.4); the per-client
  control channel was unbounded (now 256 messages, a client that stops reading is closed); the
  acknowledged frame is kept in the prediction ring for re-comparison; a forward-stepped
  projectile's lifetime is shortened by the lag; per-tick shared body list instead of one per
  player; PVS rows borrowed instead of copied; oversize snapshots drop several records per
  re-encode. The client tick wrap now also restarts the consecutive frame run.
- Rejected: "dead inputs are replayed after respawn" (only frames the server will execute alive
  are replayed; frames run while dead are acknowledged and dropped first).

## 11. Changes in v2 (Phase 3)

- Snapshot: the own block (stamina, focus, statuses) after `last_input_tick`; the entity mask is
  9 bits with `STATUS` (16-bit aura mask); flag bit 5 `script running`; entity kind 2 `area`;
  player spawn info carries team, aspects and armour class.
- Control: `Hello` carries a build choice and a team; `Content`, `Respec`, `RespecResult` and
  `BuildApplied` added; `PlayerInfo` carries the team.
- Measured cost (turmoil, 16 bots, 150 ms, 3% loss, test_room): 5.6 KB/s up, 9.2 KB/s down per
  player (v1: 5.6 / 8.5). With the full kits on the arena map: 5.3 KB/s up, 10.3 KB/s down.

## 12. Changes in v2.1 (Phase 4)

- Control: `Travel`, `TravelTicket`, `TravelRefused`; `Hello.token` mandatory under a hub,
  `Hello.name`/`Hello.build` ignored then. The datagram format is unchanged; the version byte
  stays 2 (sections 2–5 did not change).

## 13. Changes in v2.2 (Phase 6)

- Control: `Roster` (a joiner used to receive one `PlayerInfo` per player), `PlayerInfo.model`,
  `ModelRevoked`; `StallOpen`, `StallClose`, `Stalls`, `StallOpened`, `StallClosed`,
  `StallResult`. Every send of a reliable message disconnects a client that does not drain
  its queue (before, only `Killed` did; the others were dropped silently).
- Interpolation delay is a time, not a tick count (7.3): 2 to 5 ticks at 20 Hz instead of 6 to
  13 (300 to 650 ms).
- The datagram format is unchanged; the version byte stays 2.
- Measured (loopback, the town, 100 strolling players all in view of each other plus one
  viewer): at 20 Hz 4.0 to 5.8 KB/s down and 1.8 to 2.1 KB/s up per player; at 64 Hz 13.4 KB/s
  down and 5.6 KB/s up. At 20 Hz the worst of the 100 bots had 0 or 1 unexplained correction
  in its minute over three runs (the budget of section 9 is one per 10 s).

## 14. Changes in v3 (Phase 7)

- Input: button bit 11, `command`.
- Snapshot: `health` for the viewer's party and for creatures; flag bit 7 `commanding` on the
  own entity; animation state 12 `command`; one more bit, `harmful`, in an area's spawn info;
  own companions always sent; squad sight while commanding.
- Collision (7.5): a body that begins a step inside another is not held by it.
- Control: `Order`, `OrderRefused`, `Squad`, `Encounter`, `Loot`, `Trial`; `PlayerEntry` and
  `PlayerInfo` carry `kind`. The content pack carries `creatures` and `trials`, and abilities
  carry `squad` and `creature`.
- The version byte is 3: the datagram format changed (the area record).
- Measured: one leader with three companions in the Warden fight (simulated network, 150 ms,
  3% loss) 4.7–5.3 KB/s down, 5.5 KB/s up, 0 unexplained corrections in 24 runs of two to four
  minutes; 16 leaders with their squads in the dungeon (loopback, 67 bodies) 17.9 KB/s down on
  average, 19.3 KB/s for the worst. The arena test of section 9 is unchanged (10.3 KB/s down).

## 15. Changes in v4 (Phase 8)

- **A second carrier.** A WebTransport session (HTTP/3) carries exactly sections 3 to 8: the
  datagrams as WebTransport datagrams, the control stream as one bidirectional stream opened
  by the client. `docs/WEB.md` 2 is the contract for the listener, its certificates and what
  a browser does with it. What differs from section 1 on that listener: eight unidirectional
  and eight bidirectional streams are allowed (HTTP/3 opens three unidirectional streams of
  its own and the session's CONNECT is a bidirectional one), and a session whose peer cannot
  take a 1,100-byte datagram is refused. A zone treats clients of both kinds alike.
- Control: `TravelTicket` gains `web: Option<WebAddr>` (the destination's WebTransport
  listener: its URL and, for a pinned certificate, its SHA-256). The version byte is 4: the
  message's layout changed.
- The input-idle kick and the grace after `Reject` and `Kick` (section 1).
- Measured (headless Chromium, loopback, the arena with 15 native duelists): the browser
  client receives 64 snapshots a second with no gap; zone → browser 7.0–8.7 KB/s, browser →
  zone 6.6–6.7 KB/s as the zone's QUIC statistics count UDP bytes (a native client sends
  5.5: HTTP/3 datagram framing and the browser's acknowledgements make the difference); 0 or
  1 unexplained correction in 40 s. A QUIC bot and a WebTransport bot in one zone over real
  UDP: 9.4–9.5 KB/s down and 9.6–9.7 KB/s up each in the test room.

## 16. Changes in v5 (Phase 9)

- Control: `Report { target, reason }` (client → zone; reasons `aim`, `griefing`, `other`)
  and `ReportResult(Result<(), String>)` (zone → client). `docs/ANTICHEAT.md` 5 is the
  contract: one report per client per 30 s, of a client-driven body in the zone or one that
  left within two minutes. The version byte is 5.
- Nothing in sections 2 to 7 changed. What the zone *keeps* changed: each executed frame's
  claimed view tick is remembered beside the clamped one of 7.4 (`Player.view_claimed`,
  bounded to 32 ticks). Hits and projectiles are still resolved against the clamped tick;
  the aim statistics of ANTICHEAT.md 4 read the claimed one.
- A zone started with `--replay-dir` records every tick as a full-zone snapshot with this
  document's snapshot codec (section 5) and its own event list; the file format
  (ANTICHEAT.md 3.2) carries its own codec version, so a later change to the control
  messages leaves recorded fights readable.

## 17. Changes in Phase 10 (still v5)

- No message changed. What the zone does with `Chat` did (section 8): it is checked and
  limited where it arrives, the zone has a ceiling, a full queue drops chat instead of the
  client, and a flood is kicked.
- A join the zone refuses after it claimed the character at the hub (no body free after
  all), and a client that goes away before `Welcome` and `Content` were written, give the
  character back to the hub at once (`Release`, HUB.md 3.8). A character that joins again
  replaces its own body before the zone's room is counted.
- A client now speaks to the hub in the players' messages (HUB.md 3.8) and shows screens
  (CLIENT.md); nothing a zone sees of it is different.

## 18. Changes in v6 (Phase 11)

- Five messages, **appended** to `Control` after `Trial`: client → zone `StallBuy { stall, listing, price }`, `Wear { item }`
  and `TakeOff { item }`; zone → client `BuyResult { listing, result }` and `WearResult {
  item, result }`, each naming what it answers (ITEMS.md 5). The zone asks the hub for all
  three (`ZoneEconOp::StallBuy`, `Wear`, `TakeOff`): it is the zone that knows where a body
  stands and whether it is in a fight.
- **What a version adds goes at the end of `Control`** (of its own enum since v7, section
  19), whichever way it travels: the
  variants before it keep their numbers, so a client and a zone of different versions can
  still read each other's `Hello`, `Reject` and `Kick` and say what is wrong. A unit test
  pins the bytes of those messages to the ones the v5 build produced.
- **Reach**: a body buys at a stall when its feet are within 120 units of the middle of the
  stall's tile along the ground and within 96 above or below
  (`gm_net::control::stall_in_reach`; the client offers a stall by the same function).
- **The fight lock**: what a body wears does not change until it has neither dealt nor taken
  damage for 10 s.
- **Gates**: the stall requests (open, close, buy) share the gate they had (one at a time,
  one a second, per player); wearing has a gate of its own of the same size. A `StallBuy`,
  `Wear` or `TakeOff` the gate stops is **answered** with a refusal, where a stopped
  `StallOpen` or `StallClose` is dropped as before: these three are sent by a screen that
  waits for the answer. A request the zone refuses by its own checks did not ask the hub
  and does not count against the next. Before the gates, in the connection's own task, the
  three are limited to four a second with eight in hand; past that they are dropped unread,
  so a flood costs the tick loop nothing.
- **Patience**: the zone waits ten seconds for the hub's answer to a buy or a change of
  gear, then answers the client itself; a late answer to a change of gear is still applied.
- A refusal's text is for the person and is the zone's or the hub's own words (the table in
  ITEMS.md 5).
- Nothing in sections 2 to 7 changed: gear moves damage in the zone, and a client predicts
  none of it. A replay written before v6 reads as before (its frames carry no protocol
  version).

## 19. Changes in v7 (Phase 12)

- **Two types for the two directions.** `Control` was one enum of everything either side
  says; a client carried the code to write a zone's messages and to read its own. It is
  `FromClient` and `FromZone` now (section 8). The browser build lost 62,834 bytes by it.
  The numbers of `Hello` (0), `Reject` (14) and `Kick` (25) are what they were, and both
  enums have more than sixteen messages, as the one had, so a message's number is still
  written as a plain byte (`bitcode` packs it when there are sixteen or fewer): the three
  messages of the handshake have the bytes they had in v5, and a unit test holds them to
  that. What one side cannot say is not read as something else: a zone's `Welcome` is no
  message of a client's.
- **People together** (PARTY.md 4): `PartyInvite`, `PartyAnswer`, `PartyLeave`,
  `PartyRemove`, `PartySay`, `Whisper` and `TradeAsk` from a client; `Party`, `Invited`,
  `Heard`, `TradeAsked` and `TradeOpened` from a zone. What the zone answers to a request
  is a line of its own (`ChatFrom` from 0); what the party is is in `Party`.
- **Gates**: the party's requests and `TradeAsk` share a gate of their own, of the size of
  the stall's and of gear's; they pass the connection's flood limit first. `PartySay` and
  `Whisper` are chat: checked as a line is and taken from the account's bucket in the
  connection's own task, with `Chat`. The zone's ceiling of ten lines a second is for
  what everybody hears; a party's line and a whisper go through the hub.
- **A health that is no longer sent.** Section 5's delta could say a new health and could
  not unsay one: a record whose health was sent in the baseline and is not sent now (the
  body is no longer of the viewer's party) is written whole, with `SPAWN`, and read
  without a health. Before parties changed nothing ever stopped sending one.
- **Nobody who left a fight comes back into it**: a `Hello` for a character whose last
  body left this zone while it was on the ledger of an encounter that is still engaged is
  answered `Reject("the fight you left here is not over: come back when it is")`; one for
  a character whose body is in such a fight here now is answered `Reject("your body here
  is still in a fight: come back when it is over")`, and the fight goes on with that body.
- A `Whisper` to a name no character can have is answered with a line of the zone (`nobody
  can be called that`) and counts for nothing; a name in any request is checked by its
  bytes before anything else.

## 20. Changes in v8 (Phase 14)

`PROTOCOL_VERSION` 8 (LOOK.md 6.2). The simulation is untouched; the control stream says
what a body holds, so that every client draws the same weapon in the same hand:

- `FromZone::Content` gains **`props: Vec<String>`**: the keys of every prop the zone's
  content names (`ability.prop`, `template.model`), in order of first appearance. The
  client finds the files in its bundle by key (CONTENT.md 6); the zone never reads one.
- `PlayerEntry` (in `Roster`) and `PlayerInfo` gain **`look: Look { held: u16, worn: u16
  }`**: indices into `props`, `u16::MAX` for nothing (`worn` is Phase 16's armour overlay
  and always `NONE` now). A client reads an index past the list as nothing.
- **`FromZone::Look { id, look }`**, at the end of the enum: a body's look changed (a
  weapon worn or taken off, a respec to another primary). The indices are of the session's
  pack: a zone restarts to change content and its clients reconnect.
- The zone chooses: the model of the weapon template worn (the hub's gear reading names
  the templates, HUB.md 3.9), else the primary ability's prop, else nothing; companions and
  creatures by their build.

## 21. Changes in v9 (after Phase 14: seeing the fight)

`PROTOCOL_VERSION` 9 (LOOK.md 13). The simulation is untouched; a snapshot says one thing
more about a body, so that a client can draw a blow where it lands:

- An entity's record carries **the acting ability** with its stance: `EntityState.acting`,
  the `AbilityId` (the pack's index and one; 0 for none) of the script the stance shows,
  written as a uvar right after `anim` and **only when the stance is a script's**
  (`gm_core::sim::anim::acts`: windup, swing, recovery, cast). A body that stands, runs,
  guards or lies dead costs nothing more; a swing costs one byte on each of its three
  changes of stance. The zone sets it with the stance (`Player::acting`, from the running
  script's ability); the client has it as `RenderEntity::acting`.
- With it and the pack every client already has (`FromZone::Content`), a client knows the
  reach, the arc and the times of the swing any body in sight is making: the wedge
  `melee_hit_point` tests (VOCABULARY.md 5.1). Nothing is revealed that the shared
  animation set did not already show (MODELS.md 9: nobody's avatar may hide a windup); it
  is now exact instead of guessed.
- `gm_core::sim::script_anim(ability, elapsed)` is the rule that turns a running script
  into a stance, shared: the zone says it of every body, a client says it of its own body
  from its prediction (a round trip sooner).
- Replays record no acting ability (a replayed body's is 0): the viewer draws no wedges.


## 22. Changes in v10 (the game master's hand)

`PROTOCOL_VERSION` 10 (GM.md). Three messages at the ends of the enums, and one thing more
in the simulation's snapshots:

- `FromClient::Gm(GmOp)`: what a game master asks of the zone (a tempo over every script,
  numbers set outright on one ability, the content as loaded, a healing, a build worn
  now); `FromZone::Gm(GmNews)`: `Granted` after `Content` to a character the zone made a
  game master, `Tuning` with the tuning as it stands, `Refused` with why. A zone answers
  anyone else's `Gm` with `Refused("not a game master")`.
- **`Content` may come again** while playing: the zone's content was tuned (GM.md 2) and
  every client runs the new numbers from then on, the same build on the new pack; a client
  keeps its prediction and its tracks (`ClientState::set_sheet`), it does not start over.
- **An instant area stays for its echo** (section 5, `gm_core::sim::INSTANT_AREA_ECHO_MS`
  = 100 ms): an area with no duration pulses once and then stays on the wire, spent, so
  that it is in a snapshot; before, it was spawned, pulsed and removed within one tick and
  no client ever saw it.
- Hub protocol 10: `HubResponse::Claimed.gm` (the account is a moderator).
- `FromZone::Hit { target, amount, absorbed }` (added 2026-10-06, before v10 was
  committed): to the attacker, every blow its hand landed on another body, with what came
  off the target's health and what the target's block took. The number the client floats
  over the body (LOOK.md 13.8); nothing goes to anyone else, and the own hurts are read
  from the own health in the snapshot. A sparring fight is some blows a second a body:
  bytes on the reliable stream of no account.
- `FromZone::Healed { target, amount }` (added 2026-10-06, likewise): to the healer, each
  pulse of a Regen its hand put on another body, with what the pulse gave back (nothing
  for a pulse at full health). Not sent when the healer is the target: the own health
  says it. Four a second a body healed, for the Regen's seconds.

## 23. Changes in v11 (the three modes)

`PROTOCOL_VERSION` 11 (MODES.md). The input frame grows from 63 to 97 bits (section 4):

- `held` (2 bits): the weapon in hand of a gun build; `target` (32 bits): the body an
  activation is aimed at. Every mode sends both; a mode that has no use for one sends 0.
- buttons 12 (`reload`) and 13 (`scope`); bit 10 (the viewport switch) is no longer read:
  the camera is the character's mode's, never a key's.
- `Build.mode` rides in `Content` and `BuildApplied` with the build (a build stored
  before v11 is read as `action`); `Ability` carries `chain`, `firearm` and `range`,
  `MeleeArc.assist_deg`, `MoveSelf.cancel_recovery`; the statuses `Knockdown` (14) and
  `Launched` (15) take the last two bits of the entity's status mask.
- `Projectile.speed` may be 20,000 u/s (a bullet, MODES.md 3.6); the sweep per tick is
  unchanged.
- The own block carries the firearm in hand's `magazine`, `reserve` and `reloading`
  (section 5, MODES.md 3.8), added with the gun mode.
- The animation states `DOWN` (13) and `RELOAD` (14).
- **The targeted body's health** (MODES.md 5.2): a snapshot carries `health` for the body
  the client's last executed frame named as its `target`, as it does for the client's
  party and for creatures; nothing else of that body changes.

## 24. Changes in v12 (rounds, kits and the quartermaster)

`PROTOCOL_VERSION` 12 (MODES.md 11). Nothing new in the control stream:

- button 14, `use` (a kit, `F`); bit 15 stays reserved.
- The own block carries `kits` (uvar) and one bit, a kit in use, for every build (section
  5): the client adopts both as it adopts the rounds, and drops a use the zone refused.
- The animation state `USE` (15).
- The `reserve` of the own block is now what the inventory holds of the firearm's stack
  (MODES.md 11.2); its encoding is unchanged.
- Hub protocol 11 and the players' protocol 4 (ITEMS.md 4).

## 25. Changes in v13 (the director played the gun again, 2026-10-07)

`PROTOCOL_VERSION` 13 (MODES.md 10.2, LOOK.md 13.11). The simulation's datagrams are unchanged:

- `FromZone::Hit` carries `at: [f32; 3]`, where the blow landed (a bolt's or a blade's point
  on the hull, the body's centre for an area or a pulse): the number is drawn there, not
  over the head, so a headshot reads as one.
- `FromZone::Impact { at, normal }`, appended: a bullet (a bolt at 10,000 u/s or more) met
  the world there; the client leaves a dark mark on the wall for twenty seconds. To every
  session of the zone, a few bytes a shot.

## 26. Changes in v14 (the crouch seen, 2026-10-08)

`PROTOCOL_VERSION` 14 (MODES.md 3.5, 10.2). One bit in the snapshot's entity record:

- `flags` is **nine** bits, not eight: bit 8 `CROUCHED`, set on every body whose crouch
  button is held on the ground (`Mover::crouched`). A client draws that body squatting and
  hangs its name and numbers lower; the RPG target pick and a bot's aim read its hitbox
  `CROUCH_DROP` (16 u) shorter, as the zone does. It is cosmetic for the own entity: the
  client recomputes its own posture from its input every tick and adopts nothing.
- The zone's rewind (7.4) records the posture with the position, so a bolt or a blade is
  resolved against a capsule as short as the body was at the shooter's view tick. Nothing
  of that is on the wire.

## 27. Changes in v15 (the RPG body stands as it was left, 2026-10-08)

`PROTOCOL_VERSION` 15 (MODES.md 5.1, 10.3). One bit in the snapshot's entity record:

- `flags` is **ten** bits, not nine: bit 9 `RPG`, set on every body whose kit is in the RPG
  mode. That body's frames carry its camera's yaw, which says nothing of where it stands
  facing: a client draws it running the way it goes (toward the camera too: no backpedal),
  turning to its `yaw` only for an action, and standing otherwise as it was left
  (`app::facing`). Nothing else reads the bit; the zone's aim, hitboxes and ledger are as
  before.

## 28. Changes in v16 (six elements, 2026-10-09)

`PROTOCOL_VERSION` 16 (MATRIX.md 5, 14). One bit in the snapshot's player spawn info:

- `aspects` is **six** bits, not five: a bit per element in MATRIX.md 5's order (Fire,
  Water, Grass, Electric, Ground, Air). All six clear is a neutral body. Nothing else on the
  wire changes; the elements' indices moved, so a v15 client would read the wrong colours
  and the wrong bit widths, and the version byte keeps it out.

## 29. Changes in v17 (the colossus's arms, 2026-10-09)

`PROTOCOL_VERSION` 17 (MATRIX.md 16, LOOK.md 6.5):

- `Look` gains **`off: u16`**: the prop in the off hand, an index into the pack's `props`
  like `held` (`NONE` for nothing). The zone chooses it: the prop of the build's **guard**
  ability (a shield for a shield wall), else nothing; a gun build's is `NONE`.
- The own block's `status` index is **five** bits (seventeen statuses: `Taunt` is 16) and
  every status carries its **`source`** (uvar): the entity that applied it, which the
  mover reads on both sides to turn a taunted body to its taunter. An entity record's
  `status` mask is **24** bits (`gm_net::snapshot::STATUS_BITS`).
- Nothing else changes; a v16 client would read the own block's statuses one bit short, and
  the version byte keeps it out.

## 30. Changes in v18 (the item bar, 2026-10-09)

`PROTOCOL_VERSION` 18 (LOOK.md 3.2, MODES.md 11.3). Three bits in the input frame and the
own block reshaped:

- The frame carries `use_slot` (3 bits) after `target`: the item cell a `use` (button
  14) is of, 1–4; 0 names none and is the first cell's, so an old habit (`F`) still uses
  the kit. 5–7 are malformed. A frame is 100 bits.
- The own block carries the bar, four uvars (how many of each cell's stack the body
  carries), then the cell in use in 3 bits (0 none), where v12 carried one count and one
  bit. The client adopts them as it adopts the rounds, and drops a use the zone cleared.
- Nothing new in the control stream. Hub protocol 12 and the players' protocol 7 carry
  the bar's arrangement (ITEMS.md 4).
