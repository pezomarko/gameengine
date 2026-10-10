# The three modes — v0 (2026-10-07), proposed; section 11 (rounds, kits, the quartermaster) built the same evening (15d)

Status: a design, nothing built. The director on 2026-10-07: "The Action type (GTA style) /
FPS / 3rd person RPG should be 3 different modes that are not interchangeable during play,
it should be pre selected by character. If character is gun type, you get FPS view, holding
gun, having number of bullets, recoil, spread etc, take as much of mechanics as you can from
Counter-Strike 1.6. GTA style is more like MapleStory 2 or Blade & Soul, where you have
skills and try to combine them and gank. 3rd person RPG should be mouse driven, WASD is ok
as it is in Ether Saga, but focus lock should work with target-actions where you execute an
action and wait for your char to get into range and attack."

What was built instead, and is wrong by this: one control scheme with two cameras, `V`
switching first and third person at any moment (CLIENT.md 4.5, VOCABULARY.md 9). The
camera was treated as a viewport; the director means three **games** on one simulation.

Code follows this document; changes to both go in one commit. The numbers are proposed
(section 9); the director tunes them with the GM hand (GM.md) where it reaches.

## 1. Principles

1. **A mode is the character's, not a key.** It is chosen with the build and shown on the
   archetype at creation (CLIENT.md 4.3). `V`, `--third-person` and the `third_person`
   setting go. A respec at the trainer may change the mode as it changes anything else
   of the build (MATRIX.md 9.1: never in a fight); it takes effect at the `Respawned`.
2. **One simulation, one vocabulary** (VOCABULARY.md 1) still: the verbs, the matrix, the
   bodies, the zone are the same for all three. What a mode adds on the server is listed
   here and nowhere else: a firearm's magazine, recoil and inaccuracy (section 3), the
   chains and the knockdown (4), aim at a named target (5). Everything else is camera,
   control and HUD.
3. **Each mode plays like its root**: Counter-Strike 1.6; MapleStory 2 and Blade & Soul;
   Ether Saga and Tales of Pirates. Where a root and a decision of the plan conflict, the
   decision is written here with its price (bullets, 3.6).
4. **Imbalance is the point** (PLAN.md 4.1). A gun kills at range and by the head; an
   action body cannot be hit while it rolls; an RPG body never misses what it has
   targeted. The counter to each is in the other two and in the ground.

## 2. Which mode

The build carries it, as content (`builds.toml`, `gm-core::build::Build`):

```toml
mode = "gun" | "action" | "rpg"
```

Validated with the build (`BuildError`): `gun` needs a primary with a `firearm` block
(3.2); `action` and `rpg` may not hold one. The client reads the mode from the build it is
given at `Content` and at every `Respawned` and sets its camera, its controls and its HUD
from it; the zone reads it from the same build for the rules of sections 3 and 5.

| | Gun | Action | RPG |
|---|---|---|---|
| Root | Counter-Strike 1.6 | MapleStory 2, Blade & Soul | Ether Saga, Tales of Pirates |
| Camera | first person, the view model (LOOK.md 6.4) | over the shoulder, as today (110 u back) | orbit, far and high (240 u back, 90 up); right drag turns it, wheel zooms |
| The body faces | the view | where it goes; the camera at a swing | where it goes, or its target |
| Aim | the eye ray, through the cone of 3.4 | the camera ray re-aimed from the eyes (today's), plus the magnet of 4.2 | the zone's, at the target (5.3) |
| Movement | WASD, crouch, jump; walk is quiet | WASD, dodge | WASD about the camera, or a click on the ground |
| Attack | hold or tap the mouse; reload | combos and cancels on the hotbar | a target, then actions that wait for range |
| HUD | crosshair that opens, ammo, health | hotbar with cooldowns, combo counter | target frame, hotbar, the ground marker |
| Presets | musketeer (new) | blade, ironclad, shade | frostweaver, mender |

The presets' modes are a proposal; an archetype in `action` could as well be `rpg`. The
tavern's list and the people page say the mode with the frame (`blade: striker in mail,
action`).

## 3. Gun: first person, Counter-Strike 1.6

### 3.1 What is kept from the root

Magazine and reserve; a reload that takes its time; a rate of fire per weapon; a recoil
pattern that is the weapon's and can be learned; a cone that opens with movement, with the
air and with sustained fire and closes when standing still or crouched; a tap, a burst, a
spray as three different decisions; a headshot; the knife as the fast and silent fallback;
a weapon that slows the body by its weight; a scope that narrows the view; no iron sights,
no sprint, no leaning, no regeneration.

### 3.2 The firearm, as content

A `firearm` block on a primary whose steps launch a bolt (`Verb::Projectile`); validated
with the ability (VOCABULARY.md 11):

```toml
[[ability]]
key = "musket"
slot = "primary"
prop = "musket"
firearm = { magazine = 1, reserve = 24, reload_ms = 2800, cycle_ms = 1200, fire = "bolt",
            headshot = 4.0, scope = 0,
            recoil = [[0.0, 2.4]],
            cone = { stand = 0.6, crouch = 0.3, move = 3.0, air = 8.0, shot = 1.2, recover_ms = 400 } }
[[ability.step]]
at_ms = 0
projectile = { speed = 20000, gravity = 0.02, ... spread = 0 }
```

| field | means | range |
|---|---|---|
| `magazine`, `reserve` | rounds in the weapon and carried; the reserve refills at a respawn and at an ammo pickup (ITEMS.md: a stack of rounds is an item). Section 11 proposes the reserve becomes a stack carried and bought, with a hard cap | 1–100, 0–400 |
| `reload_ms` | the reload, a script of its own: `R`, or an empty magazine with rounds carried begins it by itself (since 2026-10-08; before, the trigger had to be pulled on the empty magazine); a stagger interrupts it and the rounds are not lost; a switch of weapon cancels it | 500–6,000 |
| `cycle_ms` | the time between two shots; replaces the primary's cooldown | 50–3,000 |
| `fire` | `auto` (held), `semi` (a click a shot), `bolt` (a click a shot and the body works the action for the cycle; no shot while moving faster than a walk) | |
| `headshot` | the multiplier for a hit in the head band (3.5) | 1–5 |
| `scope` | 0 none; else the zoom (2 = FOV 45, 4 = FOV 20): the secondary mouse button toggles it, the view model is not drawn and the body walks at `move_scale` while scoped | 0, 2, 4 |
| `recoil` | the pattern: the view's kick after the n-th shot of a spray as (yaw, pitch) in degrees, cumulative; past the end it repeats the last; the index resets after `recover_ms` without a shot | up to 32 pairs, each within ±6° |
| `cone` | inaccuracy in degrees (3.4): `stand`, `crouch`, `scoped` (a quarter of `stand` when absent), `move`, `air`, `shot`, `recover_ms` | each 0–15; `recover_ms` 100–2,000 |

### 3.3 Recoil

The pattern is applied twice, to the same effect: the **client** kicks the own view by the
n-th pair after the n-th shot (a punch angle that decays over 150 ms, CS's `v_punchangle`),
and the **zone** turns the n-th bolt's direction by the pair *before* it, the pattern as
it stands after the shots so far, before it rolls the cone. The first shot of a spray is
turned by nothing: it flies where the crosshair is, as the root's does. The player who
pulls the mouse against the pattern lands the spray; the pattern is in the content, so it
can be learned from the file as it was learned from the game. A recoil is not spread: it
is deterministic and shared, and it is what makes the gun a skill.

The bolt leaves the muzzle (the projectile's `spawn`, beside and below the eye) **toward
the point the eye ray meets** (the world or a body, within 16,384 u; `Zone::eye_ray_point`),
not parallel to the look: what is under the crosshair is what is hit, at any distance.
A bolt aimed at a target (5.3) is led to it from the muzzle as before.

### 3.4 The cone

The bolt's direction, after the recoil, is rolled uniformly inside a cone shaped as the
root's (its `m_flAccuracy`: still is nearly exact, a run is a lottery, a spray grows with
the square of its count, a pause forgets it):

```
base  +  move × share(speed / max speed)  +  air (while airborne)  +  shot × min((n / 4)², 4)
```

- `base` is `stand`; `crouch` while the crouch button is held on the ground (the body
  comes down `CROUCH_DROP` = 16 u: the eye, the top of its hitbox and the drawn body
  alike, 3.5); `scoped`
  while the scope is up (the scope is what makes the shot: the musket's is 0.05°, its
  `stand` 6°, so a shot without the scope is the root's no-scope).
- `share(s)` is 0 up to half the body's speed (Shift's walk, a crouched creep), 1 from
  four fifths, smooth between (`sim::move_share`): a walk is as good as standing, a run
  opens the whole of `move`.
- `n` is the index of the shot in its spray (0 for the first, so the first adds nothing);
  the spray is forgotten `recover_ms` after a shot, so click, pause, click is three first
  shots. By the fourth shot the spray adds `shot`, by the eighth four times it.

Today's `spread_deg` on a projectile
(zone.rs, the uniform roll in a cone) is this with a constant cone; a firearm's bolt sets
`spread = 0` and takes the cone from here. The client draws the cone as the crosshair that
opens: four lines whose gap is the cone at 1,024 units. The zone's roll is the only roll;
the client predicts the kick and the opening, never the shot.

### 3.5 The head

The hull is a cylinder (MATRIX.md 3); CS has hit groups, we have none. **The head band is the
top 12 units of the hull** (a sixth of a striker). A bolt that enters the hull within the band
is a headshot: the packet is multiplied by `headshot` **before** armour (MATRIX.md 7 gets a
step: hit zone). Melee arcs and areas have no head. **Crouching shortens the hitbox**: the
capsule loses `CROUCH_DROP` (16 u) off its top while the button is held on the ground, so
the band moves down with the body and a shot at a standing head passes over a crouched one;
the rewind of PROTOCOL.md 7.4 remembers the posture with the position, so the capsule a bolt
or a blade is resolved against is as short as the body was when the shooter saw it. The
world hull (32 × 32 × 56, MATRIX.md 3) does not change: a crouch does not fit under
anything a stand does not. Everyone sees it: the body squats (the shared animation set lays
the squat over any stance, MODELS.md 9) and its name and numbers come down with it; the
`CROUCHED` flag of the entity record carries it (PROTOCOL.md 26).

### 3.6 Bullets are bolts, fast

PLAN.md 4.2 decided projectiles, never hitscan, for the aimbot's sake. A bullet is a bolt
at **20,000 u/s**: across the longest sight line in the arena (about 2,000 u) in a tenth of
a second, a lead of a body's width on a runner at full speed. It is swept per tick as every
bolt is, with the forward step against where targets were (PROTOCOL.md 7.4), so the shot
lands where the shooter saw the body, minus that tenth. If the director finds it does not
feel like the root, hitscan for `firearm`s is a day's work on the same rewind: the forward
step collapsed to one sweep, the aim statistics of ANTICHEAT.md 4 unchanged. Decided by
playing it, not here.

### 3.7 Weapons in hand

Three on the keys of the root: `1` the primary (the gun), `2` the secondary (a pistol, a
firearm with a `cone` of its own, or a thrown knife), `3` the knife (a dagger as every body
has a fist: `melee_arc` at full speed, silent). A gun slows the body by its `move_scale`
while in hand, the knife not at all. The guard slot is empty in this mode (a `gun` build
may not buy one: no parry with a musket). Actives stay (a dash, a vanish); `Shift` walks
(half speed, no footsteps, SOUND.md), not guards. The weapons it wears are the ones these
hands hold (ITEMS.md 2, 2026-10-08): a musket, a pistol, a dagger; a staff or a sword is
refused in words and greyed in the bag. The mode stays the character's: no weapon worn
turns a gunman into a bladesman mid-fight (1, 8).

### 3.8 On the wire and in the HUD

The own body's snapshot carries `magazine: u8, reserve: u16, reloading: bool` (the stranger
sees the stance only). The HUD: bottom right the magazine over the reserve, in the ammo
glyph's colour when below a magazine; bottom left health and stamina; the crosshair of 3.4;
no hotbar, the three weapons as a strip that lights the one in hand; the hit marker and
the numbers of LOOK.md 13.8 as now. Sound: a shot is loud (the reach of the thunderclap),
the reload and the empty click audible at 400 u.

## 4. Action: third person, combos (MapleStory 2, Blade & Soul)

### 4.1 What is kept from the root

Free aim from behind the shoulder; skills with stages that chain into each other when the
key is pressed again in time; a dodge that cannot be hit; animation cancels that are
themselves a skill; knockdowns and launches that open a body to a follow-up; a party that
chains its controls on one body — the gank — and diminishing returns so a chain ends.

### 4.2 The magnet

A melee arc with `assist = 30` turns the body, at the swing's start, up to 30° toward the
nearest living enemy body within `reach × 1.5` that it can see; the zone does it and the
client predicts it, both from the same bodies. Bolts and areas are not assisted. Today's
camera-to-muzzle re-aim stays for everything else.

### 4.3 Chains

```toml
[[ability]]
key = "sword"
chain = { next = "sword_2", window_ms = 400 }
```

While `sword`'s recovery runs and for `window_ms` after it, the same slot plays `sword_2`
instead, a full ability with its own numbers and its own `chain`; the third of a chain
usually knocks down. A chain's members cost nothing in the kit: they come with the first.
The hotbar's cell shows the stage (I, II, III) while the window is open; the combo counter
of 4.6 counts hits in one chain.

### 4.4 Cancels and the dodge

An ability may name what may cut it: `cancel = "recovery"` on a `MoveSelf` (the dash)
means a dash pressed during another ability's recovery ends that recovery now. The dash
with `evades = 150` cannot be hit for its first 150 ms: bolts, arcs and areas pass. It
costs stamina as now, so a body dodges twice, then takes the hit. This is the counter to
the gun: a roll through the shot.

### 4.5 Down, up and the chain's end

Two statuses (MATRIX.md 8, VOCABULARY.md 5.4): `Knockdown(ms)` (the body is on the ground,
cannot act or guard, takes hits in full, rises when the time is out) and `Launched(ms)`
(in the air, same, and falls into a `Knockdown` of half the time); since MATRIX.md 16 a
third, `Taunt(ms)` (the body and its view turned to the taunter and held). **Diminishing
returns**: the second control of the same kind on one body within 10 s lasts half, the
third does nothing and the body is immune to that kind for 10 s. The party frame shows the
mark on the target so a gank is timed, not spammed.

### 4.6 Controls and HUD

As today: the primary on the left button, the secondary on the right, the guard on `Shift`,
the actives on `1`–`4`, the dodge on `Space` when the build has a dash (jump is the dash
without one). The HUD keeps the hotbar with its cooldowns and gets a combo counter at the
right of the crosshair (hits in the current chain, fading two seconds after the last).

## 5. RPG: third person, mouse driven, target-actions (Ether Saga, Tales of Pirates)

### 5.1 What is kept from the root

The camera orbits and the body does not turn with it; WASD moves about the camera; a
click on the ground sends the body there; a click on a body targets it and `Tab` cycles;
an action with a target walks the body into range and then lands, every time; a guard is
held. (The root's repeating primary is not kept: nothing fires by itself, the pace of a
fight is the player's, the director's call of 2026-10-08.)

### 5.2 Target

A left click on a body, `Tab` the nearest visible enemy not yet cycled, `Esc` clears. The
target frame on the HUD (name, build's frame, health as a bar: a targeted stranger's
health goes on the wire to the one who targets it, as a creature's does for everybody,
COMPANIONS.md 8.1). A target is lost when it dies, leaves the zone or has not been seen
for 5 s.

### 5.3 Target-actions

An action pressed with a target:

- **in range and seen**: it fires, aimed by the zone at the target. A `melee_arc` turns the
  body to face it; a `projectile` is launched with the mind's lead (`gm_ai::fighter::lead`,
  the companions' own, from where the target was at the attacker's view tick, where the
  bolt is spawned and stepped forward) and the content's spread; an aimed `area_effect`
  is put on the target. The frame's yaw and pitch stay the camera's for the view: **the aim of an RPG body
  is never the client's** (ANTICHEAT.md 4's statistics skip it: there is nothing to measure);
- **out of range or unseen**: the body turns and walks toward the target along the nav grid
  (COMPANIONS.md 7, built on the client from the same map in 14 ms) until it is in range,
  then fires. One action waits; a new one replaces it; a movement key or a ground click
  cancels it. The walk is made of ordinary frames the client produces, so prediction and
  the ledger are untouched;
- **the primary** is an action like the others: one press, one shot. It repeated every
  cooldown on the target until 2026-10-08; the director plays the spam himself.

Range is the ability's: a melee arc's `reach`, a bolt's `range` (new, content: the musket
600, the crossbow 900, a knife 300), an area's `origin: Aim` radius. Without a target an
action fires where the body faces, as in the action mode without the magnet.

### 5.4 On the wire

`InputFrame` grows for every mode (bits are cheap in the ledger, PROTOCOL.md 4): `target:
u32` (0 none) and `order: u8` (none, the ability in `ability` at the target, move to the
point). A ground click is a `MoveTo` the client resolves to frames; it is not sent. The
zone validates: a target must be a body the sender is being sent, the range the content's,
the sight the zone's trace.

### 5.5 Controls and HUD

Left button: target or move, and on the target the primary at it (the first click picks,
the next attacks, as in Tales of Pirates and Ether Saga: the cursor is a crosshair over the
target, a hand over another body); right drag: the camera, a right tap on the target the
secondary at it; wheel: 120–400 u; `Tab`, `Esc`; the hotbar on `1`–`8` (the primary and secondary join it: the root's bar is one bar); `Shift`
guards; `Space` jumps. A ground marker where the body is going; the target frame top
centre; a ring under the target (LOOK.md 13).

### 5.6 On a phone (2026-10-08)

The director opened the page on a phone and nothing answered a tap (WEB.md 3.5). A touch
screen plays every mode now, with the RPG mode the one made for it:

| | RPG | action and gun |
|---|---|---|
| walk | a tap on the ground (5.5); the stick is not drawn | a stick on the left of the frame, around where the finger landed |
| camera | a drag anywhere; two fingers for the distance | a drag on the right |
| target / primary | a tap on a body, then on the target | a tap on the right: one blow, one shot |
| secondary | a long press on the target, or the `2` button bottom right | the `2` button bottom right, held |
| jump, guard, abilities | the hotbar's cells, as their keys; Shift's cell guards while held | `jump` bottom right; the hotbar's cells (`C` guards, `LMB`/`RMB` fire) |
| menu, Tab | `menu` top right (Escape); no Tab: tap the next body | `menu` top right |

The HUD and the screens are drawn larger on a touch screen (WEB.md 3.5: the device's pixel
ratio as the scale, so a cell is finger-sized), and the controls appear from the first
finger seen. **Proposed numbers**, for the director to play: the stick's rim at 56 dots and
its dead zone of an eighth; a long press at 0.45 s; a tap's primary held 0.12 s; a swipe
across the width half a turn (`touch::LOOK_GAIN` 3 counts a CSS pixel); the stick's share
of the width 45 %. The canvas's text fields (a new character's name, the stall's prices)
type from the phone's keyboard through a box of the page's (WEB.md 3.6); the page's
fullscreen bar and the menu's Fullscreen button are WEB.md 3.7; the chat line is
not a field of the toolkit and has no keyboard on a phone yet (WEB.md 10).

## 6. What goes

`Viewport` in the client (`app.rs`) becomes `Mode`, read from the build. `V`, `buttons::
VIEWPORT`, `--third-person`, `third_person` in the settings and the page's `third-person=1`
flag (WEB.md 5) are removed. VOCABULARY.md 9's table is rewritten to this document's; the
command stance (COMPANIONS.md 5) keeps its key in every mode.

## 7. Phase plan (proposed: Phase 15 in three parts, the editor to 16, the body's look to 17)

| part | builds | played when |
|---|---|---|
| 15a Action | `mode` in the build and the mode fixed on the client; the toggle gone; the magnet, chains, cancels, the dodge, knockdown and launch with diminishing returns; two chains in content (sword, dagger); the combo counter | the director chains a sword three times into a knockdown, rolls through a firebolt |
| 15b Gun | the firearm block, magazine and reload, cycle, recoil and the cone, the head band, the three weapons, the ammo HUD; the musketeer preset; a pistol and a second long gun in content | a spray controlled against the pattern lands; a crouched tap at 1,500 u lands a headshot; the empty click |
| 15c RPG | the target, the target frame, target-actions with the walk, the ground click on the nav grid, the orbit camera; `range` in content; the aim statistics skipping RPG bodies | a frostweaver clicks a dummy, presses the shard, walks into range and lands it; `Tab` across three spar bots |

Where CONTENT.md, LOOK.md and PROTOCOL.md say "Phase 15" they mean the editor (now 16) and
by "Phase 16" the body's look (now 17); they are not rewritten.

Action first because it is today's play made a mode and the smallest step; the gun second
because it is the most new code; the RPG last because it changes the wire. The order is the
director's.

## 8. Deliberately absent (v1)

Wall penetration, a buy menu, iron sights, sprint, leaning, a hard lock in the action mode,
a body walking round other bodies, a mode changed mid-fight, a fourth mode.

## 9. Proposed numbers and open decisions

Every number above. Open for the director: **the presets' modes** (2); **hitscan or a bolt
at 20,000 u/s** (3.6); **the head band** at 12 units and ×4; **whether an RPG body's aim
is the zone's** (5.3: it makes the RPG body the one that never misses and never cheats,
and the gun body the one that can do both); **whether a mode change is a respec** (1); the
**diminishing returns** of 4.5 and whether creatures are under them; **what a body does
when its target walks out of sight** (waits in place, as proposed).

## 10. As built

### 10.1 The action mode (15a, 2026-10-07)

Built as sections 2 and 4 say, with these readings:

- **The mode is in the build** (`Build.mode`, `builds.toml` `mode`), validated with it
  (`BuildError::GunNeedsFirearm`, `FirearmNeedsGun`, `GunHasNoGuard`, `NotSlottable`);
  a build stored before it is read as `action`. The camera follows the mode of the sheet
  the zone sent (`Content`, `BuildApplied`); `V` is a key offline and in a replay only,
  `--third-person` likewise, the `third_person` setting is read and dropped.
- **Chains** are `chain = { next, window_ms }` on an ability and the stages are
  `slot = "extra"` abilities that come with it into the kit (`Kit::chain_next`; the
  kit may hold twelve now, `MAX_ABILITIES`). The pressed slot plays the next stage from
  the moment the running stage is past its last active window (`script_commit`) until
  `window` after it ends; the hotbar's cell shows the stage (II, III). The sword and the
  dagger chain three deep; the third puts the body down (`sword_3`: Knockdown 1.2 s;
  `dagger_3`: Launched 300 u/s up for 0.9 s). The second cut's knockback is 60, not the
  first's 150: a chain's early blows must not carry the body out of the third's reach.
- **The magnet** is `assist` on a melee arc (30° on the chains' arcs): the nearest enemy
  within reach and a half and in sight, with the line of sight traced to twenty units
  short of the body's centre, since the mover's world holds the other bodies as solids.
  The zone reads friend and foe by team and party; the client, which is not told
  parties, reads everyone on another team as an enemy and, in the wild, everyone: a
  swing predicted toward an ally there is corrected by the zone's reading.
- **The dodge** is the dash's `iframes_ms = 150` (already in the vocabulary) with
  `cancel = "recovery"`; Space plays the kit's dash while it is ready, else jumps.
- **Knockdown and Launched** are statuses 14 and 15: no action, guard or movement; the
  `DOWN` stance; diminishing returns per kind (`Player::controls`, ten seconds), creatures
  under them too. (A crouch had no hull of its own until 2026-10-08: 10.2.)
- **The combo counter**: the own blows within two seconds of each other, right of the aim.
- The input frame carries `held` and `target` for every mode (PROTOCOL.md 23), so the
  wire changes once for the three parts.

### 10.2 The gun mode (15b, 2026-10-07)

Built as section 3 says, with these readings:

- **The firearm** is a `firearm` block on a primary or a secondary (`Firearm` in the
  vocabulary, VOCABULARY.md 11 for its ranges); its cycle is the ability's cooldown. The
  mover keeps a `GunState` per hand (magazine, reserve, the reload under way, the last
  shot and the index in the spray); a spawn fills both. The musket (one round, bolt
  action, a kick of 2.4°), the pistol (eight, a click a shot) and the carbine (25, held,
  a pattern of 25 pairs) are content; the pistol has a model of its own since the
  director played it (2026-10-07: `gm-tools content synth pistol`, LOOK.md 13.10), the
  carbine is drawn as the musket until it has one.
- **The kick** is applied twice, as 3.3 says: the client's view is punched by the pair
  after the shot (`Action::Fire.kick`) and the punch falls to a third in 60 ms; the zone
  turns the bolt by the pattern before the shot (`Action::Fire.turn`, `Firearm::turn`)
  before it rolls the cone. The frames sent carry the mouse's aim, never the punch.
- **The cone** (`gm_core::sim::cone_deg`) is the same function on both sides: the HUD's
  crosshair opens by it, the zone rolls in it, added to the bolt's own spread. A crouch
  is the button held on the ground (Ctrl or C): the body comes down 16 u (`CROUCH_DROP`,
  `Mover::crouched`: the eye, the hitbox's top and the drawn body; the own camera eases
  to it) and creeps at half speed; the head band moves down with the hitbox (3.5, since
  2026-10-08). Walking is Shift at half the axes.
- **The head band** (`HEAD_BAND` = 12 u, ×`headshot`, before armour) is read where the
  bolt's sweep meets the capsule, against the rewound capsule as the hit itself is.
- **The reload**: `R`, or an empty magazine with rounds carried, which begins the reload
  by itself the tick after the last shot (since 2026-10-08: the director asked for it;
  before, the trigger had to be pulled on the empty magazine, held on an auto); it ends
  by itself; a stagger or a knockdown drops it and keeps the rounds, and the empty
  magazine begins it again once the body can; a switch of weapon drops it. The stance `RELOAD`. The bolt action fires standing or
  walking, never above half the body's speed.
- **In hand** `1 2 3` (the gun, the pistol, the knife) and the actives on `4`–`7`;
  the hand is what everyone sees held (LOOK.md 6.2: the zone says a `Look` when it
  changes; the own view model reads it from the predicted mover the same frame) and
  the view model is turned to the look by the template's `fit_view` (LOOK.md 6.4);
  the reload lowers and works the view model (LOOK.md 6.4);
  Ctrl crouches; the secondary mouse button toggles the scope of a firearm that has one
  (2 or 4: the field of view and the mouse divided by it, the view model hidden, a
  mask with its lines; no gun in content had one until 2026-10-07, when the musket got
  `scope = 2`, the root's Scout, so the button did nothing the director could see); the own block of the snapshot carries both hands' rounds and
  whether the one in hand is being reloaded, and the client adopts them.
- **The HUD**: the four lines of the crosshair at the cone's angle, the magazine over
  the reserve bottom right (red under half a magazine), "reloading", the hotbar as the
  three weapons (the one in hand lit) and the actives.
- Not built: wall penetration, a scope model; the `musket` at 20,000 u/s is 3.6's bolt,
  hitscan is not built. (The ammo item came the same evening: section 11.)
- **The director played it (2026-10-07, evening)**: "the sniper scope mode seems way
  less precise", "the bullet should leave black mark in environment for some time as it
  is in CS 1.6", "making target slow down for a second or two when hit taken", "our hit
  animation highlight is misleading since it's always centering no matter where we hit".
  Built the same evening (protocol v13, content v4):
  - **The scope is the shot**: while the scope is up the standing (or crouching) cone is
    a quarter of itself (`SCOPED_CONE` 0.25, in `cone_deg` on both sides, so the
    crosshair shows it); the move, the air and the spray open it as before. The mouse is
    still divided by the zoom.
  - **Tagging**: every bullet (the musket's, the pistol's, the carbine's) carries an
    `on_hit` Slow of 0.5 for 1,500 ms, refreshed by the next hit: the body hit walks at
    half speed for a moment, as the root's does. Content, not code.
  - **Bullet marks**: a bolt at 10,000 u/s or more that meets the world says
    `ZoneEvent::Impact { at, normal }`; everyone in the zone gets `FromZone::Impact` and
    draws a dark disc of 3.5 u on the wall, lifted 0.6 u off it, for 20 s (the last 4 s
    fading), 160 at most, the oldest forgotten first. Bodies leave no mark (the blood of
    the root is Phase 17's, with the body's look).
  - **The hit where it landed**: `ZoneEvent::Hit` and `FromZone::Hit` carry `at`; the
    gold number and a spark are drawn at the point of the hull the bolt or the blade
    met (LOOK.md 13.11), the head band's shots at the head. Nothing is drawn at the
    centre of the screen that was not before (the crosshair itself does not react).
  - **The rounds**: the director: "no way to use rounds from inventory, they should be
    auto equipped or something". They are: a stack carried is the reserve, nothing is
    equipped; the hub's logs show his balls and carbine rounds bought at 14:39–14:43 and
    spent within seconds. What was missing was the word: a stack of rounds now says
    which gun it loads ("loads the Musket: carried is loaded, R reloads") in the
    inventory and at the stall. A stack of rounds for a gun that is not in the build
    loads nothing, and says so. The kit heals 300 (a third of a body of about 1,000;
    50 was nothing).
- **The director played it a third time (2026-10-07, night)**: "using scope, from not
  that far, I always see the bullet mark to the upper right of crosshair ... CS 1.6 was
  great at this, spread was very dependant of fire rate and movement, when very still,
  it would be like 99.9% precise ... I'd even say this was not as much of spread issue
  as it is inaccuracy caused by some other shift bug. Also make CTRL crouch". It was a
  shift, twice (content v5):
  - **The kick was on the shot that fired it**: the zone turned the n-th bolt by the
    n-th pair, so the musket's one ball flew 2.4° *above* the crosshair every time (at
    500 u, 21 u up). Now the bolt is turned by the pattern before the shot (3.3): the
    first shot of a spray flies true, the second is where the first's kick put the gun.
  - **The bolt flew parallel to the look** from a muzzle 4 u right and 2 u below the
    eye, so the mark was 4 right and 2 down of the crosshair at any range (under a 2×
    scope, a hand's width). Now the bolt leaves the muzzle for the point the eye ray
    meets (3.3): the mark is under the crosshair.
  - **The cone has the root's shape** (3.4): `scoped` in content (the musket 0.05°, its
    `stand` 6° so a no-scope is a no-scope), a run opens the move's share and a walk or
    a creep none, the spray's share grows with the square of its count instead of its
    count, and the spray is forgotten after `recover_ms` (not twice it): click, pause,
    click. The pistol's `stand` 0.8 (was 1.0), the carbine's 0.45 (0.5), the moves 3–4,
    the airs 8–10.
  - **Ctrl crouches** as it did (and C); what was missing was anything to see or feel:
    the eye now drops 10 u and the body creeps at half speed while the button is held
    on the ground, on both sides (`Mover::crouched`); the own camera eases the drop
    over about a tenth of a second. The hull does not change (the head band stays), a
    stranger is drawn standing.
- **The crouch seen (2026-10-08)**: "can we do crouch animation and target resize so all
  can see and have different hit zone for players who are crouching? so it looks and
  feels more like CS 1.6". Built as 3.5 now says (protocol v14):
  - **The hitbox shortens**: `capsule_at` takes the posture and drops `CROUCH_DROP` off
    the capsule's top; `Mover::capsule` passes its own, the zone's `History` records
    (origin, crouched) per body per tick and the bolt's sweep, the head band's reading
    and the melee wedge all take the rewound posture with the rewound origin. The band
    is read against the shortened top, so a crouched head is a headshot at the crouched
    height and nothing at the standing one (the test: a level musket shot that was a
    headshot on a standing blade at 300 u flies over it crouched; aimed 2.1° down it is
    a headshot again).
  - **The drop is 16 u** (was 10): the eye, the capsule's top and the drawn body all by
    the same number, so the body stands where its hitbox is. A striker's 56 becomes 40
    (the root halves 72 to 36; a capsule of our radius cannot lose that much and stay a
    body). The director may want it deeper or shallower: it is one constant.
  - **Everyone sees it**: `flags::CROUCHED` (bit 8; the flags are nine bits now) on
    every body's record; the client draws the squat (`gm_model::anim::crouch`: the
    thighs 75° forward, the knees folded 137°, the body lowered by the drop, the feet
    where they were; the idle's hanging arms bend to the thighs; a creep rocks the legs
    with the cycle instead of striding) over any stance but the kneeling command, cross-
    faded like a change of stance; a name, a health bar and a hit's number hang 16 u
    lower (`fx::chest_of`). The own body in the third person squats from its prediction
    and is no longer sunk into the floor by the eye's drop (it was lowered twice before:
    the hull origin was taken from the dropped eye). The RPG target pick and the bots'
    aim (`sense::Body::centre`) use the shortened capsule. The fitting room of
    `gm-tools content look` has two crouched columns at the end (still, creeping).
  - **Not changed**: the world hull (no crouching under things), the crouch in the air
    (the button counts on the ground only), the cone's `crouch` value (content, as it
    was).

### 10.3 The RPG mode (15c, 2026-10-07)

Built as section 5 says, with these readings:

- **The target** is the client's (`rpg.rs`): a left click picks the body under the
  pointer (the ray through the last frame's projection against the capsules) or the
  ground there; Tab cycles the enemies in sight by distance; Escape lets go. The frame's
  `target` carries it to the zone, which sends that body's health back (PROTOCOL.md 23)
  and shows it in the frame top centre; a stranger's whole is read as the band's top,
  1,500, a creature's as its own.
- **Target-actions**: `1` the primary, `2` the secondary, `3`–`6` the actives; with a
  target they wait (`Rpg::act`) until the body is within the ability's `range` (95% of
  it) and in sight, then press once; a new press is a new action (the primary repeated
  until 2026-10-08, when the director played it: the spam is the player's). Out of range
  the body walks toward the target on the nav grid (`gm_ai::nav`, built on the
  client from the map at the first click, seeded where the body stands); a click on the
  ground is the same walk. Without a target a key presses at once, as in the action
  mode, with the bolt flying level.
- **The zone's part** (gm-core): an activation with a target within `range` and in
  sight turns the body to it for the script (`lock_yaw`), a bolt is led to it
  (`sim::aim::lead`, the companions' own) and an aimed area is put under its feet; a
  target out of range or unseen leaves the action as it would be without one. The
  statistics of ANTICHEAT.md 4 skip a body in this mode: the replay's roster carries the
  mode (replay format 2).
- **The camera** orbits the body's centre (`orbit_camera`: 240 u back and 90 up, the
  wheel from 120 to 400, pitched between 5° and 80° down), the pointer is free, the
  secondary button held turns it; the frames carry the camera's yaw and a level pitch,
  the body faces where it goes (LOOK.md 13.9), or its target while a target-action's
  turn holds it, and **between actions it stands as it was left**: facing the way it last
  walked, or the target it last turned to, while the camera orbits (the director,
  2026-10-08 midday: "char should face direction it last stopped at, or was left at";
  it was drawn at the camera's yaw, swinging round with every drag of the orbit). An
  action without a target fires the camera's way, so the body turns to it for the
  script and keeps that facing after. (PR 28 of 2026-10-09 made the frame carry the body's
  own yaw instead, so an untargeted cast flew where the body last walked; the director
  the next morning: "my caster was previously mouse controlled and now that is lost ...
  just revert to previous thing": reverted, PR 29. The camera aims the free cast.) Every body in this mode is drawn by the rule, the
  own and the ones seen (snapshot flag `RPG`, protocol v15); `S` walks the body toward
  the camera facing that way, not a backpedal. The own body is drawn facing where the
  mover does while a target-action's turn holds it (`lock_yaw`, predicted on the client
  with the nearby bodies); until 2026-10-08 it was drawn at the camera's yaw, so the
  frostweaver cast its shard facing away from the dummy while everyone else saw it turn. The tracer the client lets fly
  from the hand at the launch (the zone's bolt shows a round trip later) is led to the
  target as the zone leads its bolt, within the ability's range and in sight; it flew
  the look's way, the camera's in this mode, and the director saw the shard leave for
  the horizon. The zone's lead is taken from where the target was at the tick the
  attacker saw (the history's), as the forward step of PROTOCOL.md 7.4 spawns the bolt
  there: until 2026-10-08 it was led from where the target stood now, so the bolt got
  to the point a round trip before the target did and missed every walker, while the
  tracer, led from the client's older picture, flew true, and the director saw his
  shard go to the target and the zone's bolt fly on elsewhere.
- **Marks**: a ring where the body is going, a red one under the target.
- **The buttons** (2026-10-08, the director: "LMB and RMB now seem completely removed
  from 3rd person mouse rpg ... maybe first click highlights target and then cursor
  changes into attack cursor which activates normal attack? it does so in tales of
  pirates and ether saga"): a left click on a body that is not the target picks it; on
  the target it asks for the primary at it, the same target-action `1` asks for (the
  walk into range, one press); on the ground a walk. The pointer shows what a click
  would do (`Rpg::hover`, the window's own cursors: a crosshair over the target, a hand
  over another body, the arrow elsewhere). A right tap on the target, let go within 0.4 s
  without turning the orbit by a degree, is the secondary at it; a right drag is the
  camera as before.
- Not built: a body walking round other bodies (the grid knows the map only), a
  target kept five seconds out of sight (it is let go when it leaves the frame's
  knowledge), the people page's and the tavern's word of the mode.

## 11. Rounds, kits and the quartermaster (2026-10-07, built as 15d: 11.7)

The director, after reading 10.2: "lets put NPC in town for potions (health kits) and
ammunition. Also there should be strict limit how much ammo one can carry, same as in CS,
it's a trade of for FPS, they do still have knife option though." The root's buy menu is
a town stall here, and the root's carry limit is a stack's cap. Proposed and approved the
same evening ("I agree, do it that way and then redeploy"); 11.7 says what was built.

### 11.1 A stack is an item

A third item kind in items.toml beside `weapon` and `armour`: `stack`, with no layers,
no materials and a cap. Four templates:

```toml
[[template]]
id = "ball"          # the musket's
kind = "stack"
cap = 30             # the root's AWP carries 30; the musket drops a body in one or two
[[template]]
id = "pistol_round"
kind = "stack"
cap = 64
[[template]]
id = "carbine_round"
kind = "stack"
cap = 90             # the root's rifles carry 90
[[template]]
id = "kit"
kind = "stack"
cap = 5
heals = 50           # health, over the use (11.3)
```

A firearm names its stack: `firearm = { ammo = "ball", ... }` replaces `reserve`
(VOCABULARY.md 11 and the validator: the stack must exist and be `kind = "stack"`).

The hub (ECONOMY.md, ITEMS.md 4) gets one column, `items.quantity` (1–1,000, default 1).
A stack is one row and one slot; `move_item` into a holder that already carries a stack
of the template **merges** up to the cap and refuses the rest in words ("you carry all
the balls you can"): that is the strict limit, and it holds for a stall's sale, a trade
and a grant alike. A stack never splits in v1 (one drag moves it whole). Gear keeps
`quantity = 1` and never merges. The same column answers ECONOMY.md 9's open question on
stacking materials, if the director wants it answered the same way; nothing here needs it.

### 11.2 The reserve is the stack

The mover's `reserve` is read from the inventory: the quantity of the stack the firearm
names, zero when none is carried. A **reload takes rounds off the stack** and the zone
tells the hub once per reload (`EconOp::Consume { item, n }`, one of the five requests a
second an account has; a reload is never that frequent). The magazine stays with the
body: a respawn **no longer fills anything**; the body comes back with the rounds it died
with in the gun and whatever the stack holds. When both are empty the trigger clicks and
`3` is the knife, as the director says. A spawn into a zone with no stack carried: the
magazine is full once (the gun was issued loaded), and that is all.

The own block of the snapshot already carries `magazine` and `reserve`; the HUD changes
nothing but what the reserve means. The stranger sees the stance only, as now.

### 11.3 The kit

`F` uses a kit in every mode (the potion of the action and the RPG modes, the medkit of the
gun; it is a `buttons::USE` bit, protocol v12, in bit 14 of the two reserved). It is a
script of **1,500 ms**: the hand lowers the weapon (the view model as the reload), the body
walks at half speed, a stagger or a knockdown interrupts it and keeps the kit; it ends with
a `Healed` event of `heals` (the numbers of LOOK.md 13.8 show it) and one kit fewer on the
stack (`Consume` to the hub). Refused at full health and in the air. Kits carried show
beside the ammo, bottom right, in every mode. No regeneration still (3.1).

### 11.4 The quartermaster

A second stall bot in `play.sh people`, **Quartermaster**, on the market tile next to the
Keeper's, strolling as the Keeper does. Its twelve slots list stacks: three stacks of each
kind, granted by `hubctl --grant-item Quartermaster ball 10` (the grant takes a quantity
for a `stack`), and `people` grants again whenever its stall stands empty, as it does for
the Keeper today. Prices in silver, proposed: ten balls 1 s, sixteen pistol rounds 1 s,
thirty carbine rounds 1 s 50 c, one kit 2 s. A buy that would pass the cap is refused in
words before any coin moves; the root sells the remainder at full price, we do not.

The ledger keeps its principles (ECONOMY.md 1): the stacks come from `source` as every
grant does, the coin goes to the bot's purse, not to the sink; the player's stall sells a
stack as the Keeper's does, so a player who buys cheap in town and sells dear at the arena
is doing what the economy is for. A fixed NPC price is a ceiling the players' market lives
under, as the root's buy menu is.

### 11.5 Phase 15d and acceptance

Content v3 (the stack templates, `ammo` on the three firearms), hub v1.8 (the column, the
merge, `Consume`, the grant's quantity), protocol v12 (`USE`, kits on the own block),
the bot's `--sell-at` listing stacks, the quartermaster in `people`, `F` and the kit
script on both sides. Played when: a musketeer buys thirty balls, a thirty-first is
refused in words, spends them in the arena, the counter runs to 0, the trigger clicks, the
knife kills; dies with two in the magazine and comes back with two; a kit at a third
health heals 50 over a second and a half and a hit during it keeps the kit.

### 11.6 Open for the director (decided 2026-10-07: as proposed)

The caps (30, 64, 90, 5) and the prices; whether a death keeps the magazine (as proposed)
or empties it, the root's way; whether the kit works in all three modes or the gun's only;
`F` for the kit; whether materials stack by the same column now; whether a body dropped
to the ground drops its stacks (ITEMS.md 9 asks the same of gear).

### 11.7 As built (15d, 2026-10-07)

Built as 11.1–11.5 say, with these readings:

- **Content v3**: `kind = "stack"` with `cap` and `heals` in items.toml (ball 30,
  pistol_round 64, carbine_round 90, kit 5 heals 50; `layers = []`); `firearm.ammo`
  names the stack and `reserve` is gone from the firearm block (VOCABULARY.md 11:
  `Firearm.ammo`, a key of at most 48 characters, and `load_dir` refuses an ammo that is
  not a stack of items.toml). The musket got `scope = 2` the same day.
- **Hub v11**: migration 0011 (`items.quantity`, 1–1,000). The one item mover
  (`move_item_stacking`) merges a stack arriving in a `character` or `storage` holder
  onto the row of its template there and refuses past the cap in words ("you carry all
  the balls you can"), **before** anything moves; the trade commit does the same per
  item and refuses the trade whole; the spent row lets go of whatever still names it (a
  listing just sold, a trade just committed). A stack arriving where no stack of its
  template is stays a row, within the cap. Stacks into a stall or escrow never merge (a
  listing is a row). `ZoneEconOp::Consume { character, item, quantity }` lowers a stack
  (gone at nothing, `consume` in the item log, the second item sink) and answers the
  reading; a refusal (not carried, fewer than asked) is logged and the reading answers
  anyway. `StallBuy` answers `Gear` (the reading after the buy) instead of `Done`.
  `GearReading.stacks` (item, template, quantity, heals) rides with the gear at the
  claim, after a wear, a buy and a consume, and `HubNotice::Gear { character, reading }`
  goes to the character's zone after a session's request moved items without the zone's
  hand (the storage, a stall of its own, a craft, a decomposition, a trade: both sides).
  `gm-hub --grant-item NAME STACK HOW_MANY` grants a stack (`grant_stack`: onto the stack
  carried, up to the cap). `PLAYER_VERSION` 4: `ItemSummary.quantity` and `cap` (0 for
  what is not a stack); a stack's `what` is `ball ×25 of 30`, a kit's `does` is
  `heals 50, used with F`.
- **The simulation**: `GunState.reserve` is set by `Zone::set_stacks` from the reading
  (the quantity of the stack the firearm's `ammo` names; the kits are the stacks that
  heal, `Player.kit_heal` what one heals); `fill_guns` fills the magazine only. A
  reload moves rounds from the reserve to the magazine as before; the zone compares the
  reserve before and after a body's frames and says `ZoneEvent::RoundsLoaded { id, hand,
  rounds }`, the server tells the hub `Consume` on the row the slot remembers and lowers
  its copy at once. A respawn keeps the magazines and the kits (`Zone::respawn`); a body
  that joins a zone has its guns issued full once.
- **The kit**: `buttons::USE` (bit 14; bit 15 stays reserved), `F` on the client, in
  every mode. `Mover.kits` and `kit_until`; `kit_step` begins a use on the press when a
  kit is carried, on the ground, with no script, dash, reload or command stance under
  way; `KIT_USE_MS` 1,500; a stagger or a knockdown drops it with the kit kept; it ends
  with one kit fewer, the zone heals `kit_heal` (a `Healed` with the body as its own
  source: the green number), says `KitUsed` and tells the hub. A use begun at full
  health is cleared by the zone the same tick and the client drops it on the next
  snapshot (`using_kit` false). The body walks at half speed meanwhile and fires
  nothing; the stance `anim::USE` (15); the view model is lowered and worked as for a
  reload (`kit_progress`). The own block carries `kits` and `using_kit` (protocol v12).
- **The HUD**: "kits N  F" bottom right in every mode, above the ammo in the gun mode,
  dim at none, "using a kit" in yellow while the hands are at it. The inventory names a
  stack `ball ×25`.
- **The quartermaster**: `play.sh people` spawns a second stall bot, Quartermaster, on
  the square (`--sell-at 100`: a silver a listing), granted twelve stacks (three of ten
  balls, three of sixteen pistol rounds, three of thirty carbine rounds, three kits of
  one) whenever it carries nothing; a keeper bot lists every stack it carries beside
  what can be worn. The prices of 11.4 are approximated: every listing is 1 s (a kit
  included) until the bot's `--sell-at` takes a price per template.
- Tests: `gm-hub` `stacks_merge_to_the_cap_and_are_spent_through_the_zone` (the grant
  onto the stack and at the cap, the inventory's words, the reading, `Consume` and its
  refusal, a stall's sale onto the buyer's stack, the refusal past the buyer's cap with
  no coin moved, the books balanced); the gun tests of `gm-core` set the duel's stacks.
- Not built: a stack that splits (one drag moves it whole); a ground pickup of a stack
  (ITEMS.md knows no ground screen); a price per template at a bot's stall; the kit's
  own motion (the reload's is borrowed); the stranger's `USE` stance drawn (it reads as
  idle until the body's look, Phase 17).

