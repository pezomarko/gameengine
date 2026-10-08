# Sound (v1)

Phase 13 (PLAN.md 11.8). Until now the game was silent. This is the contract for what is
heard, how a sound is made, where it is mixed, what it costs, and how it is proved on a
machine that has no sound device.

Code follows this document; a change to either goes in one commit.

## 1. Principles

1. **Measured in kilobytes.** The browser build has 71,334 bytes of its megabyte left
   (WEB.md 15). `kira`, which PLAN.md 11.2 named for audio, was measured on 2026-10-03 with
   no sound made yet (`default-features = false, features = ["cpal"]`; `cpal` with
   `wasm-bindgen` for the browser): **+62,142 bytes of wasm** (+21,547 packed), +8,999 of
   JavaScript, and +155,880 bytes of the native client. It would leave 9,192 bytes for the
   sounds themselves and for everything after this phase. It is not used. What is used
   instead is the browser's own mixer (Web Audio: nodes the page creates, the browser
   mixes) and, natively, a mixer of a few hundred lines over `cpal` (the one crate added;
   the device is the only thing a crate is needed for).
2. **No asset bytes.** Every sound is **synthesized at start** from a patch written in
   code (an oscillator or a noise, an envelope, a filter, a sweep: 2). Nothing is
   downloaded, nothing is licensed, the two builds make the same samples, and a test can
   hash them.
3. **Nothing on the wire.** Everything a sound needs the client already knows: every body's
   animation state and whether it stands on the ground (PROTOCOL.md 5), the health the
   wire carries for some bodies, projectiles and areas appearing and vanishing, its own
   health and its own predicted actions. A sound is a thing the client *infers* from what
   it would draw anyway; the zone sends nothing for it and never will (a sound is not a
   rule: PLAN.md 6).
4. **Never in the way of a frame or a tick.** The native device's thread renders from
   fixed arrays and allocates nothing; the frame thread hands it short commands and never
   waits for it. In the browser the frame thread only creates source nodes. A sound that
   cannot be started (no device, the browser's audio not yet allowed, every voice busy) is
   dropped or steals a quieter one: the game plays the same without it.
5. **Old-school.** 22,050 Hz, twenty patches, thirty-two voices: what a 1998 engine did,
   done on purpose. Nothing here is a music system.

## 2. How a sound is made

`gm-client/src/sound/synth.rs`. A **patch** is a short program of a few **parts**, each
rendered and summed into a mono buffer of `f32` samples at **22,050 Hz**, once at start:

- a source: `Sine`, `Triangle`, `Saw` (band-limited: a wavetable whose harmonics stop under
  6 kHz), `Noise` (a 32-bit xorshift seeded by the patch's name and the part's index: the
  same every time);
- a pitch: a frequency, with an optional **sweep** to another frequency over the part's
  length (exponential: a multiply a sample), and an optional vibrato (rate, depth);
- an envelope: attack, hold, decay to −60 dB, in milliseconds; a patch is as long as its
  longest part;
- a filter: a two-pole state-variable low-, band- or high-pass with a resonance and a
  cutoff that may sweep too;
- a gain per part; the sum is normalised to a peak of **0.8 times the patch's level**
  (the mix between patches: a step is quieter than a death).

**The two builds make the same bytes.** The arithmetic is IEEE basics only: `sin`, `exp`,
`ln` and `pow` are the module's own series (`synth::det`), not the platform's libm, whose
last bits differ between the desktop and the browser. A test pins every patch by a hash of
its samples (whoever changes a sound changes the hash on purpose) and the browser's
report carries the hash of all of them.

The patches of v1 (3 says when each is heard). None is longer than 600 ms except the
loops, which are rendered 4 s long and made seamless by folding their last tenth over
their first with an equal-power crossfade (3.6 s remain), then shaped by two slow waves
over the whole loop (a gust, a swell) with whole numbers of periods so that they join:

| Patch | Level | Made of | Length |
|---|---|---|---|
| `step_a`, `step_b` | 0.4 | noise, low-passed 1,100 → 700 Hz (`b` 900 → 600), decay 50 ms | 60 ms |
| `land` | 0.6 | a sine 140 → 40 Hz, decay 110 ms; noise low-passed 1,800 → 500 Hz over it | 120 ms |
| `dash` | 0.5 | noise through a band sweeping 500 → 2,500 Hz | 250 ms |
| `swing` | 0.5 | noise through a band sweeping 800 → 3,000 Hz, short | 160 ms |
| `hit` | 0.8 | a thump (sine 110 → 70 Hz) and a noise burst low-passed 2,500 → 800 Hz | 111 ms |
| `stagger` | 1.0 | a deeper thump (90 → 50 Hz, decay 160 ms) and a longer burst | 181 ms |
| `parry` | 0.7 | four inharmonic sine partials (1,320, 1,980, 3,564, 5,412 Hz) each with its own fast decay, and a click of high-passed noise | 183 ms |
| `cast` | 0.5 | a sine rising 330 → 1,320 Hz with a 9 Hz vibrato | 300 ms |
| `launch` | 0.4 | a sine sweeping 600 → 2,400 Hz, decay 80 ms | 100 ms |
| `impact` | 0.8 | noise low-passed 3,500 → 1,200 Hz and a sine sweeping 1,000 → 100 Hz | 129 ms |
| `burst` | 1.0 | a 60 Hz sine with a slow decay and noise low-passed 300 → 150 Hz | 395 ms |
| `hurt` | 0.7 | a saw at 110 Hz with a 14 Hz vibrato, low-passed | 150 ms |
| `death` | 1.0 | a saw sweeping 220 → 55 Hz over 600 ms with noise under it | 600 ms |
| `click` | 0.2 | a tick of high-passed noise, decay 12 ms | 15 ms |
| `blip` | 0.25 | a triangle at 880 Hz, decay 40 ms | 50 ms |
| `chime` | 0.4 | two sines, 660 then 990 Hz | 260 ms |
| `wind` (loop) | 0.5 | noise low-passed at 500 Hz, gusting | 3.6 s |
| `murmur` (loop) | 0.4 | noise band-passed around 600 Hz, swelling: a crowd | 3.6 s |
| `drone` (loop) | 0.5 | three saws at 55, 55.5 and 110 Hz, low-passed at 200 Hz | 3.6 s |

Rendered, the patches take **1,228,060 bytes** of memory (the three loops are 952,560 of
it); a budget of 2 MiB is set (8). A test renders every patch and checks its length,
that its peak is 0.8 of its level, that it is finite and not silent, that a loop's join
is no larger a step than any inside it, and the pinned hashes.

## 3. What is heard, and when

`gm-client/src/sound/cues.rs`. The zone sends every entity as a stream of samples (one
per snapshot: position, animation state, flags, for some bodies a health). The client
keeps them to interpolate between; the sound reads **every sample of every entity once,
in tick order, up to the render tick** (`Sound::feed`, from `ClientState`'s tracks), and
an entity's **removal** once the render tick has passed it. A cue is a **change between
two consecutive samples of one entity**, so that a body standing in a state makes no
sound, a frame drawn twice makes none twice, two snapshots that arrived between two
frames both sound, and nothing depends on the frame rate or the render time's jitter.
The first sample of an entity reads no transition: nothing is inferred about what was
not seen (a body that came into sight mid-swing does not swing).

| Change in a body's samples | Cue |
|---|---|
| `WINDUP` → `SWING` (or → `RECOVER`, when the blow was too short to be sampled) | `swing` |
| → `STAGGER` | `stagger` |
| → `CAST` | `cast` |
| → `DASH` | `dash` |
| → `DEAD` | `death` |
| `PARRY` ended **within 0.25 s** of opening (the zone closes a window that met a blow at once; one that ran its course was an attempt, and is silent) | `parry` |
| the health the wire carries for it fell, not by dying, no oftener than every **0.3 s** (a bleed pulses four times a second) | `hit` |
| in the air, then on the ground, after a fall of **48 units** or **0.15 s** in the air (a tick off a kerb is nothing), not dead | `land` |
| on the ground in any state but `DEAD`, `AIR`, `DASH`: every **64 units** of ground travelled, no oftener than every **150 ms**, alternating | `step_a` / `step_b` |

A jump of more than **200 units** between two samples is a placement (a respawn, a
correction), not travel.

| Projectiles and areas | Cue |
|---|---|
| a projectile's first sample with its owner's body **behind it on its line of flight** (the bolt's direction is on the wire; the body within **96 units** of the line and no more than **1,500** back): it was launched here. A bolt's first sample is its shooter's lag ahead of the muzzle (PROTOCOL.md 7.4's forward step: hundreds of units), so no distance from the body would do | `launch`, at the owner's body |
| the own body's projectile's first sample (the own body is not among the samples, and is always here) | `launch`, at the listener |
| a projectile gone, whose way on (twice its last step) hits the world, or that vanished within **48 units** of a body: it landed | `impact`, where it was last |
| a projectile gone otherwise (it went out of sight), or first seen with no owner in sight behind it (it came into sight) | nothing |
| a harmful area's first sample while its owner is in sight (the own body, or a body in the samples) | `burst` |

**The own body** is the predicted one, read once a frame (`Sound::own`):

| The own body | Cue |
|---|---|
| a predicted `Action::Swing` (at most one a frame) | `swing` |
| a predicted `Action::Fire` or `Action::Area` | `cast` |
| the zone's word on it became `STAGGER` / `DEAD` (`ClientState::own_anims`: every snapshot's state since the last frame, so that none between two snapshots of one frame is missed) | `stagger` / `death` |
| the zone's word left `PARRY` within 0.25 s of entering it | `parry` |
| its health fell, not by dying, no oftener than every 0.3 s | `hurt` |
| landing and steps as for the bodies, from the mover's own ticks: the travel of each tick predicted this frame, measured across that tick alone (a reconciliation snaps the body between ticks and replays into a sink of its own: neither is travel, and nothing is heard twice) | `land`, `step_a` / `step_b` |

Its swings and casts come from its own actions because the zone's word on them is a
round trip late; its staggers, death and hurt come from the zone because only the zone
knows. A predicted swing the zone refuses sounded all the same (the player saw it too).
Offline (`--offline`), the own body's steps and landings come from the local mover.

| At the listener | Cue |
|---|---|
| a button pressed on any screen | `click` |
| a chat line shown (not the own, not an ignored name's, not the own whisper going out) | `blip` |
| an invitation or a request to trade shown (not from an ignored name) | `chime` |

- **At most 8 cues** start in one frame: the nearest to the listener (a crowd of twenty
  bodies landing at once is a few thuds, not twenty).
- **The air**: a loop per map, started at the zone's welcome and faded over a second at a
  change: `wind` outdoors (the arena, the test room), `murmur` and `wind` together in the
  town, `drone` in the dungeon. Which, the map says by its name until a map says it
  itself: a `gm_ambience` key in its worldspawn is believed when present (`wind`,
  `murmur`, `drone`, `none`, or two with a comma).
- **A zone left** (`Sound::quiet`): every voice stops, the air stops, nothing is read
  across it; the next zone's first samples read no transition.

### 3.1 Where it is heard from

The listener is **the own body** (its eye) **facing the camera's way** (`Listener`):
distances are the body's, as the player hears them whatever the view; left and right are
the screen's, in the third-person view as in the first. A cue at a place
has

- a **gain** by distance: `1 / (1 + d / 256)` for `d` in units, nothing beyond **2,048**
  units, the own body's cues and the listener's at 1;
- a **pan** by direction: constant power from the sine of the angle between the camera's
  forward and the direction to the source, so that what is behind sounds as loud as what
  is in front (there is no rear in stereo);
- a **pitch** of its own: steps, hits, staggers, swings and landings vary by ±8% from a
  hash of the entity and the cue count, so that a crowd is not one sound repeated.

### 3.2 Settings

`volume` (0–100, default 70; the master gain is `(volume / 100)²`) and `mute` (the "no
sound" box), on the settings page as a slider and a box, applied at once, saved as the
other settings are (CLIENT.md 8). A muted game still counts its cues (the report of 7
says what *would* be heard).

## 4. The native mixer

`gm-client/src/sound/mixer.rs` and `device.rs`. `cpal` opens the default output device
at its own rate and format (`f32` or `i16`; anything else plays silently), asking for a
**512-frame buffer** (11.6 ms at 44.1 kHz) and taking the device's own when refused;
the callback renders exactly the frames it is handed, in blocks of a fixed scratch of
8,192, as stereo spread over the device's channels (the rest get nothing); its meter
scales what it measured to 512 frames. It renders from:

- **32 voices**, a fixed array: a patch, a cursor (fractional samples), a step (the
  patch's rate over the device's, times the pitch, clamped to 0.25–4), a gain for each
  ear. Linear interpolation between samples. A voice ends when its patch does.
- **Stealing**: a cue that finds no free voice takes the quietest, whose output at that
  instant the new voice carries on and lets die with a 0.5 ms time constant (every patch
  begins at nothing: the step would be a click).
- **Loops** have two slots (the air can be two), each with a gain that moves toward its
  target at 1 per second (the fade at a change of map); a slot faded to nothing is empty.
- **The master**: the sum times the master gain, then a **soft clip**: as it is up to a
  knee of 0.7, past the knee bent toward 1 and never over it (a lone sound is not bent at
  all; a sum of thirty-two is not cut).
- **Commands** (`Cue`, `Loop`, `Master`, `Quiet`) travel from the frame thread in a
  `Mutex<VecDeque>` the callback takes with `try_lock` into a fixed array of 64: if the
  frame thread holds the lock at that instant, the commands wait one block (never the
  callback). A queue nobody drains (no device) drops its oldest past 1,024. The callback
  allocates nothing: a test renders ten thousand blocks under an allocator that panics on
  an allocation (`assert_no_alloc`, a dev-dependency).
- **No device** (CI, a server, a headless run): the mixer exists and is rendered by nobody;
  cues are counted all the same, and `--sound-dump FILE` (7) renders it from the frame
  clock at 22,050 Hz into a WAV instead (at full gain, without the air: what the gate
  measures).

The callback measures itself (`Meter`: microseconds per block, the largest, the frames)
and the report of 7 gives microseconds per 512 frames; the budget is in 8.

## 5. The browser

`gm-client/src/sound/web.rs`. The browser mixes: the page creates one `AudioContext`,
turns every patch into an `AudioBuffer` once (the patches are then let go), and makes a
**pool of 32 chains** once, each a `GainNode` into a `StereoPannerNode` into the master
`GainNode`, which feeds an `AnalyserNode` and the destination. A cue is an
`AudioBufferSourceNode` (the one node the API will not let a page reuse) with its
`playbackRate` for the pitch, connected into a free chain whose gain and pan are set for
it, and started. A chain is free when its latest source has **ended** (the source's own
`ended` event, which fires on a stop as well, through a closure that frees itself after
firing: `Closure::once_into_js`, nothing leaked per cue); the chain remembers which start
its source was, so that an older source's late `ended` cannot free the chain under a
newer one. No free chain: the quietest is **stolen** (its source stopped). Loops are
sources that loop, through a gain of their own into the master, faded over a second;
nothing of the mixer of 4 is compiled into the browser build.

- **Autoplay.** A browser lets a page make sound only after a gesture of the person's.
  The first `pointerdown` or `keydown` on the page is remembered (the click that takes
  the pointer, WEB.md 3.4, usually) and the graph is made at the next frame; a context
  found suspended is resumed every frame; cues before the context runs are counted and
  dropped (`dropped=` in the report).
- **Ramps are anchored.** `linearRampToValueAtTime` ramps from a parameter's previous
  event, which may be long past: every change first cancels what was scheduled and sets
  the present value at `now` (`move_param`), then ramps (a fade) or sets (a cue's gain
  and pan, the master).
- The `AnalyserNode` on the master is read once a second for the loudest RMS seen, for
  the report of 7 and for nothing else.

The web-sys features this adds (`AudioContext`, `BaseAudioContext`, `AudioContextState`,
`AudioBuffer`, `AudioBufferSourceNode`, `AudioScheduledSourceNode`, `AudioNode`,
`AudioParam`, `AudioDestinationNode`, `GainNode`, `StereoPannerNode`, `AnalyserNode`,
`EventTarget`) are bindings; the bytes they cost are in 11.

## 6. Deliberately absent

- Music; a music system. Voice. Positional reverb, occlusion by walls (a sound through a
  wall is as loud as in the open), Doppler, HRTF, more than two channels.
- Sound for abilities by name (every cast sounds alike; every swing; every hit): the
  content says nothing of sound yet (a `sound` key per ability is the obvious next step
  and is not taken here).
- Blocks: a hit that lands on a raised guard is not seen by a client (the body does not
  stagger, and a stranger's health is not on the wire), so it makes no sound. A parry
  does. A hit on a body whose health the wire does not carry (a stranger, an enemy) is
  heard only as the stagger it causes.
- Footsteps by surface, by armour, by speed; sounds of companions' orders; the stall, the
  tavern, coin; a sound for a block, a heal, a status landing.
- A replay plays silently in v1 (the viewer reads bodies the same way, and could; not
  done).
- Sound files, a sound asset budget, a loader, a cache: there are no files.

## 7. Acceptance (`scripts/check-sound.sh`)

1. **The patches and the mixer, as tests**: every patch within bounds and pinned by hash;
   the mixer renders a cue at the right ear and ends it, plays a pitch faster, steals the
   quietest voice and never a loop, fades a loop in and out, clips softly, stops on
   `Quiet`, allocates nothing in ten thousand blocks, and prints its microseconds per
   block.
2. **The cues, as tests**: scripted streams of samples produce exactly the cues of 3
   (one swing per swing whatever the frames, a parry only when it met a blow, steps by
   stride in any walking state and never faster than one per 150 ms, a landing after a
   fall and not off a kerb, a hit per fall of health and not per pulse of a bleed, a
   projectile's launch and impact only when made and landed here, the own body's cues
   from its own actions and the zone's word, a crowd cut to the nearest eight, nothing
   across a clear).
3. **A session rendered to a file** (`--sound-dump FILE`, the frame clock driving the
   mixer at 22,050 Hz, no device needed; the client windowed on an Xvfb of the gate's own,
   on the software GPU): an offline walk (`--offline --script walk`: a second standing,
   five seconds running, a second standing) whose WAV the gate reads in 100 ms windows
   (`scripts/dev/wav-windows.py`): standing (0.4–1.0 s, after the spawn's landing, and
   6.3–7.0 s) at least **60 dB under full scale**, the loudest window while running (1–6 s)
   no more than **35 dB under**, and the report line `sound: cues=N … steps=N` with steps
   ≥ 20, nothing dropped, the block cost within 8; then the arena with fifteen duelists
   (`--script fight`, 20 s, as the web gate does): swings ≥ 1, staggers + hits + hurts ≥ 1,
   launches ≥ 1, nothing dropped, the loudest window no more than 35 dB under.
4. **Both browser builds** in headless Chromium: the same fight with a click on the canvas
   three seconds in (which both takes the pointer and allows the sound), the page's
   `GM-DONE` line carrying `sound: …`: `context=running`, `started ≥ 1`, swings ≥ 1, no
   more than half the cues stolen, and `patches=` equal to the native run's hash.
5. **The sizes**: what the phase added to the WebGPU wasm and to the native client, against
   8 (the client built alone, as the size baseline is).

## 8. Budgets (budgets.toml `[sound]`)

| Budget | Value | Why |
|---|---|---|
| bytes of WebGPU wasm this phase added | 40 KiB, measured 32,223 | of the 71,334 left (1); the measured delta is in 11. A record since 2026-10-08, not a gate: later phases grew the wasm past it, and the web gate holds the total |
| bytes of the native binary this phase added | 300 KiB, measured 128,560 | `cpal` with ALSA, and the mixer. A record likewise; the size gate holds the total |
| rendered patches in memory | 2 MiB | 2 |
| microseconds per 512-frame block, 32 voices, release | 200 | a block is 11.6 ms at 44.1 kHz; the callback must be a small part of it on the integrated machine |
| cues started in one frame | 8 | 3 |
| voices | 32 | 4 and 5 |

## 9. Proposed numbers and open decisions

Proposed (the director confirms): 22,050 Hz; the patches of 2, their lengths, pitches and
levels; 64 units a step, 150 ms between steps; 48 units or 0.15 s for a landing; 0.25 s
for a parry that landed; 0.3 s between hits of one body; 96 units off the line and 1,500
behind for a launch, 48 of a body for an impact; 256 units of reference distance and 2,048 of hearing; eight cues
a frame; 32 voices; volume 70 by default; the loops per map.

Open: **whether sounds become content** (a `sound` key per ability and per creature, so
that a Warden's maul and a dagger differ); **footsteps by surface** (the map knows its
textures); **whether a replay should sound**; **music**, which is a different thing and
a different budget; **whether a stranger's hit should be heard** (it would need the zone
to say so: a flag on the snapshot, bytes on the wire for a sound, against principle 3);
**the callback's priority** (it runs at the priority `cpal`'s thread has; one underrun
in seven seconds was seen on the loaded development machine with the callback costing
39 µs of a 21 ms period (11): a realtime priority needs `rtkit` over D-Bus or an
`RLIMIT_RTPRIO`, and v1 asks for neither).

## 10. Review log

### 10.1 Design review by an independent agent (2026-10-03, before the code)

25 findings (3 High, 12 Medium, 10 Low); the list itself was not kept. The three High
ones, all accepted and built: cues are read from **each entity's sample stream**, once
per sample in tick order (the draft read the frame's picture, which misses a state
between two snapshots of one frame and reads a frame drawn twice twice); a **hit is a
fall of a health the wire carries** and a stagger is its own cue (the draft called every
`STAGGER` a hit); the **own body's swings and casts come from its predicted actions**
(the zone's word is a round trip late), its staggers, death and hurt from the zone. Of
the rest, acted on: the listener is the own body, not the camera (3.1); no blip or chime
for an ignored name's line or asking; the gate's thresholds come from a measured dump
(the walk's steps are at −26 to −28 dB per 100 ms window, standing is digital silence);
the `[sound]` budgets (8).

### 10.2 Design review by Gemini 3.1 Pro (2026-10-03, over this document as built)

8 findings.

1. High, *a reconciliation's replay would re-count steps and landings*: **as designed**.
   The own travel is measured across `local_tick` alone (one new tick each); a
   reconciliation's replay (`ClientState::replay_from`) writes its actions into a sink of
   its own, never into `actions`. The wording of 3 says so now.
2. High, *the `AudioContext` must be made or resumed on the gesture's own call stack
   (Safari), not at the next frame*: **accepted**. The `pointerdown`/`keydown` handler
   makes and resumes the context; the frame builds the graph on it (5).
3. Medium, *LLVM fuses `a * b + c` into an FMA natively and not on wasm32, so the
   hashes differ*: **rejected, with a check**. Rust does not contract floating-point
   operations (an FMA is only ever `mul_add`, written out), and wasm32 has no FMA. The
   claim is measured rather than argued: the gate compares the browser's hash of every
   patch with the native build's (7.4), and they agree (11).
4. Medium, *a lost `WINDUP` snapshot silences the swing*: **accepted**. A swing is heard
   as anything becomes `SWING`, and as `WINDUP` becomes `RECOVER` (3).
5. Medium, *`AudioParam.value` mid-ramp is the base value, so the anchor pops; use
   `cancelAndHoldAtTime`*: **rejected**. The getter returns the computed value, automation
   included, in the current specification and in Chromium, Firefox and Safari;
   `cancelAndHoldAtTime` has no binding in web-sys 0.3.106; and the only ramps are the
   air's one-second fades at a change of map.
6. Medium, *stealing a voice mid-wave clicks*: **accepted**. The stolen voice's last
   output is carried on by the new voice and dies away with a 0.5 ms time constant
   (`Voice::declick`); a test steals at a tone's peak and finds no step (4).
7. Low, *the own body's projectile may be further than 160 units from its predicted
   body*: **accepted, and it went deeper**. The own body is not among the samples at all,
   so its bolts were never heard; and PROTOCOL.md 7.4's forward step puts every bolt's
   first sample its shooter's lag ahead of the muzzle (hundreds of units), so the first
   gate run heard 2 launches of 89 bolts. The rule is now the line of flight (3): the
   own body's bolts are the listener's; another's bolt was launched here when its owner's
   body is behind it on its line, within 96 units of the line and 1,500 back. 76 of 95
   bolts after (the rest came into sight without their owners).
8. Low, *a device that gives 480 frames must get 480, and the meter must scale*: **as
   designed** (the callback renders exactly the frames given, in blocks of its scratch;
   `Meter::per_512` scales); the wording of 4 says so.

Correct as designed, by the reviewer: the `try_lock` queue (a block's delay at worst),
linear interpolation (the aesthetic is the point).

### 10.3 Code review by Gemini 3.1 Pro (2026-10-03, over the module, the diff and the gate)

5 findings.

1. High, *a map's `gm_ambience` is never read when the zone's map is the one already
   loaded* (`switch_map` is not called then): **accepted**. The welcome reads the loaded
   map's worldspawn after setting the air by name (`ambience_of`); a map switched to says
   it as it is switched to, as before. (The reviewer also thought the welcome's `air()`
   overrode `switch_map`'s `air_named`: it does not; the switch runs the next frame.) No
   map carries the key yet.
2. Medium, *the own body's parry is timed by the frame, so a window whose words arrive
   together after a stall sounds as if it had landed*: **accepted**. `ClientState::
   own_anims` carries each word's server tick; the scene times by `tick × dt` (3). A test
   feeds twenty ticks of parrying in one frame and hears nothing.
3. Medium, *the `ended` closure is made before `start`, so a source that fails to start
   leaks its closure*: **accepted**; the closure is set after a start that succeeded
   (`ended` is an event, never fired inside `start`).
4. Low, *two buttons pressed in one frame click once*: **rejected**. One click a frame is
   the design (two at once would be one louder click); a person presses one button.
5. Low, *`Quiet` lets a loop still fading out finish its second*: **accepted**; the
   graph remembers its fading loops until they have stopped and `Quiet` stops them too.

### 10.4 Found by running it

- The gate's first fight heard **no impact** and leaked: `ClientState::prune` dropped a
  projectile's track in the same frame its removal became readable, before the sound read
  it; the sound now reads before the prune, and treats a track that vanished unread as
  removed (`Sound::feed`).
- `--headless` is the offline bench (120 frames, no scripts, no connection), not the
  loop: the gate runs the client windowed on an Xvfb of its own (as the screens gate
  does), on the software GPU.
- A kept gate directory held the previous run's `cert.der`; the bots read it before the
  new zone wrote its own and failed their handshake ("BadSignature") while the client,
  started later, got in. The gate clears what it keeps.

## 11. What was measured

2026-10-03, the development machine (AMD Ryzen 7 4800U, Renoir iGPU, Arch Linux; other
sessions loading it), release builds, nothing estimated.

**Size.** WebGPU wasm **1,009,465 bytes** (345,287 packed): **+32,223** for the phase
(977,242 before); the megabyte has **39,111** left. WebGL2 wasm 3,003,328 (918,847 packed),
+30,829. The wasm-bindgen glue grew 6,267 bytes (110,616 for WebGPU). Native client
**9,611,600 bytes**, **+128,560** (`cpal` with ALSA, the mixer, the dump, the synthesis; the
baseline is updated; built together with the servers it is 9,593,624, their features
unified: the gate builds the client alone first). `kira`, measured before any of this, would have been +62,142 of wasm
and +155,880 native with no sound made (1). Of the phase's own wasm, two things were found
and removed with `twiggy`: a stable sort to pick the eight nearest cues (≈7.5 KB of sort
machinery, now a selection) and a second hash map of fed ticks (the scene keeps them).

**Synthesis.** 20 patches, **1,228,060 bytes** rendered in **32.2 ms** at start (release;
198 ms in a debug build). The browser's hash of all of them equals the native build's
(`0x66439a25e3d99f73`), in every gate run.

**The mixer.** 512-frame stereo block, 44.1 kHz, 31 voices and two loops: **91.5 µs**
(release, under the allocation assertion). In the client: the dump path at 22,050 Hz
**40–45 µs** per 512 frames in the walk, **68–91** in the fight with every voice busy; the
real device **24.8 µs mean / 119 max** (software GPU) and **39.1 / 110** (real GPU) per 512
frames at 48 kHz. Budget 200.

**The device.** ALSA through `cpal`: the analogue codec (card 1) opens at **48,000 Hz, f32,
2 channels**; it refuses 512 frames (dmix's period here is 1,024) and runs at its own.
Underruns: 5 in the first three seconds on the software GPU (the world's load on all 16
cores), **1 in 7 s on the real GPU** (a scheduling gap of more than 21 ms; the callback
itself costs 39 µs of it): the callback runs at a normal thread priority (9). ALSA's
`default` on this machine is the HDMI codec and fails to open (BUILDING.md): `ALSA_CARD=1`.

**What is heard.** The offline walk: **25 steps** in 5 s of running, the spawn's landing at
0.1–0.3 s, standing **digital silence** (0.4–1.0 s and 6.3–7.0 s), the loudest 100 ms
window while running at **−27 dB** (gate: −35). The arena with fifteen duelists, 20 s, by
script: 1,028–1,209 cues (steps 499–697, swings 15–44, casts 189–268, launches 74–98 of
87–123 bolts seen, impacts 70–108, deaths 20–30, lands 15–32, dashes 16–56, hurts 5–14,
staggers 0–3, parries 0–6), the loudest window at −8 to −19 dB, nothing dropped (the dump
has every voice). In the browser the same fight starts **836–1,324 cues** per build (those
before the gate's click at 3 s are counted and dropped: 114–190), steals a chain for 41–148
of them, and the context is `running`; the analyser's loudest RMS 0.076–0.156.

**Tests.** 88 in the client (23 of them the sound's).
