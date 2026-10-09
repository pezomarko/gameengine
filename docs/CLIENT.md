# The client's screens

Status: v1 (Phase 10), as built. This document is the contract for what a person sees and
touches between starting the client and playing, and while playing when the game itself is
not what they are looking at: the screens, the toolkit they are drawn with, who gets a key
press, the chat line, and the settings. PLAN.md 3.3 (**archetype first, math later**), 2.6
and 2.7 (budgets, gates), 2.8 (one client for the desktop and the browser) and 6 (small
numbers, nothing a scammer can dress up) are binding. When the code and this document
disagree, the document wins. Section 12 records the reviews and what running it found.
The inventory, the storage and a stall (Phase 11) are ITEMS.md 6, and the people, a trade
and the tavern (Phase 12) are PARTY.md 8: more screens of this toolkit, under the rules of
this document.

Until Phase 10 the client was started by a command line that named the account, the
password, the character and the zone, and it ended when its connection did. That is how
gates and bots start it and it stays. What is new is everything a person needs who has none
of that.

## 1. Principles

1. **One client.** The screens are drawn by the client's own renderer with the HUD's
   primitives: no toolkit library, no web view, no second code path for the browser. What a
   browser does better than a canvas (filling in and remembering a password) stays the
   page's (4.1).
2. **The screens drive the same requests the command line did.** A command line that names
   a character is the screens on **autopilot**: the same state machine (`front.rs`) taking
   the same steps in the same order, with nobody clicking. A gate that enters by command
   line tests what a person clicks through.
3. **The server decides, the screen shows.** A screen never assumes a request worked. Every
   list on a screen is what the hub or the zone last said; every button that changes
   something waits for the answer and shows a refusal in words a person can act on.
4. **Nothing blocks a frame.** A request to the hub is sent behind the frame's back
   (`hub.rs`: `Hub::call` returns a `Pending` answer) and picked up by a later frame; the
   screen says that it is waiting. Leaving a zone does not wait for the goodbye either.
5. **A person can always get out.** Every screen has a way back; a connection that ends
   returns to the screen before it, with the reason, instead of ending the program.
6. **Measured.** The screens have a size budget on both targets and a frame cost (10).

## 2. Starting

The client talks to one hub. Where it is comes from the command line (`--hub ADDR
--hub-cert FILE`), else from the person's settings (8), else from what the build ships
beside the program (`client.toml`, the settings' format: `hub` and `hub_cert`), else, in a
browser, from the page's `config.json` (WEB.md 5).

| Started with | It does |
|---|---|
| a hub (from any of the above) and nothing else | shows the login screen (4.1) |
| a hub, a user and a password, no character | logs in by itself, then shows the characters (4.2): from there a person chooses |
| a hub, a user, a password and a character | **autopilot**: logs in (registers with `--register`), creates the character if it is new and a build is named, enters the zone named (or the default, 7), plays; a refusal anywhere ends the program with the reason, as before |
| `--connect ADDR` | joins that zone directly, as before; ends when the connection ends |
| `--offline`, or a scripted run (`--bench`, `--seconds`, `--script`, `--replay`, `--crowd`, `--start`) | walks the map offline, or does what the script says, as before: such a run does not read the person's settings at all (they would send it to a login screen, and a ticked "fullscreen" would change what a benchmark measures) unless it names a file with `--settings` |
| no hub to talk to, started by a person | a screen that says so (none named; or the one named cannot be found, or its certificate cannot be read), names the settings file a hub goes into, and offers **Walk around offline** and **Quit** |

The window opens at once in every case, on the map it was started with (`--map`, default
the test room; a browser fetches it): the **backdrop**. While a screen is up and no zone is
being played, the camera turns slowly (4° a second) where the map starts, and no body is
drawn. After a zone was played, its map is the backdrop.

A client started from somewhere else than its build's directory (a menu entry, a file
manager) finds what the build ships (`assets/maps`, the palette, `client.toml`) beside the
program: the working directory first, then the program's own directory and the two above
it (`install_root`). A person's run writes its log to `client.log` beside the settings, for
when there is no terminal to read it in.

## 3. The toolkit

`gm-client/src/ui.rs`. Immediate mode: a screen is a function that runs every frame, lays
its widgets out and learns at once what was clicked. Nothing is kept between frames but the
focus, the scroll positions, the carets and the widget the button went down on.

- **Font** (`gm_model::smallfont`, re-exported by `font.rs`). The HUD's 5×7 dot font,
  extended from capitals to **printable ASCII** and the ten letters `č ć đ š ž Č Ć Đ Š Ž`,
  with an eighth row for what hangs below the line (`g j p q y , ; |`): 104 glyphs, any
  two at least two dots apart. Anything else is drawn as `?`, and cannot be typed into a
  field that is shown to others. Text is shown as written. Since Phase 14 (LOOK.md 2.3)
  it is face 0 of the bundle's atlases and the client's fallback; the screens' words and
  the HUD's are in the text face (Fira Sans Medium, a line of 12 dots) and the titles in
  the title face (MedievalSharp, 19) when the bundle has loaded, measured per glyph, else
  in this one. Those two faces hold a generic set of 308 characters (LOOK.md 2.3: ASCII,
  the Latin-1 Supplement's letters and signs, Latin Extended-A, the typographic marks),
  so a stack's `kit ×3 of 5` and a name in any Latin tongue read as written there.
- **Scale.** One whole-number scale for HUD and screens: 1 below 600 pixels of height, 2
  below 1300, 3 below 1900, 4 from there (a line of text is then 2 to 3 hundredths of the
  frame's height; until the director played at 1080 lines it was 3 from 1000, and the
  inventory filled the frame: LOOK.md 11.4), and never so large that the widest and the
  tallest panel (340 by 360 units since Phase 14's trade window, LOOK.md 4; 300 before)
  would not fit the window. The setting `ui_scale` (1 to 4; 0, the default: by the
  window) chooses one instead, and gives way the same. On a touch screen (WEB.md 3.5,
  since 2026-10-08) the default is the device's pixel ratio rounded, 2 to 4, because a
  finger is wider than a mouse; the fit rule applies to it too. The smallest window is
  640×360.
  The bundle has an atlas made for each scale and the client draws with that one (LOOK.md
  2.2): a dot is 1 to 4 pixels, a texel always one.
- **Widgets.** A panel (a plate with a title), a label, a paragraph (wrapped at words), a
  button (also one that is off: drawn faint, takes no click and no focus; one that is off
  for a reason says the reason beside the pointer once the pointer has rested on it for
  the tooltip's 150 ms, and for two seconds after a press on it, a tap on a phone, so
  that a click on a grey button never looks like a click that did nothing), a row of buttons
  as wide as their words, a text field (one line; optionally shown as stars; a maximum in
  characters and in bytes; left, right, home, end, backspace, delete; no selection), a list
  (rows of columns, one selected; wheel, arrows, Page Up and Down, Home, End; a click
  picks, a double click or Enter activates; a selection made by the screen itself is
  scrolled into view), a checkbox, a slider, a choice (one of a few, in a row). A widget
  is identified by its label within its screen. Since Phase 11: a list may have **no row
  picked** (`ui::NONE`: none is lit, the arrows start from an end, nothing can be
  activated), which is what a screen shows when the thing that was picked is gone; a field
  for a **number** (digits only, and a paste with anything else in it is not taken:
  `12.50` is not 1250); a line in several colours (an amount of coin, a colour a unit).
  Since Phase 12: a list whose rows are **marked** (`list_marked`: a row in the colour of
  what is new, or struck through; the trade window's offers, PARTY.md 6).
- **Focus.** One widget has the keyboard. `Tab` and `Shift+Tab` move it, a click moves it.
  `Enter` presses the button that has the keyboard, and only when none has it the
  screen's main one; `Escape` is the screen's way back. A key held down does not press
  `Enter` or `Escape` again, and with `Ctrl` held a key types nothing.
- **Pointer.** While a screen is up the pointer is free and drawn by the system; mouse look
  is off. A click is a press and a release on the same widget, in one frame or in many; a
  double click is two presses on the same row of a list (not one on a button and one on
  the row that came up under it). The game takes the pointer back by itself only while
  its window has the keyboard.
- **Paste.** `Ctrl+V` and `Shift+Insert` put the clipboard's text where typing would go (a
  password manager's password is not typed by hand): the desktop only (the `arboard`
  crate; X11, and Wayland compositors with the data-control protocol). The clipboard is
  read on a thread of its own (its owner may take seconds to answer) and typed by the
  frame that finds it. In a browser the password goes into the page's own form (4.1),
  where the browser pastes. Nothing is ever copied *out*.
- **What it records.** Everything a frame showed (labels, buttons that are on, fields with
  their values unless secret, rows, boxes, sliders) is kept with its rectangle until the
  next frame: a UI script (9) finds a button by its text, and a test asks what a screen
  said. A button that is off for a reason is kept as a label, `Wear it (off: ...)`, so
  that no script finds it and a test reads why it is off. So is every text that had no room to be drawn whole: a test asks that there is
  none. The cells of a list that were cut to their column are recorded apart (a long name
  in the characters' list may be cut; a price may not, and the screens of ITEMS.md 6 are
  tested for none).

## 4. Screens

### 4.1 Login

Fields: email, password. A box: **show the password**. Buttons: **Log in**, **New
account**, **Quit**. "New account" shows a second password field (there is no reset by
mail: a typo would lock the account away) and a line that says so: *the address is only a
name: no mail is sent to it, and a lost password stays lost*. A password is 8 to 256
characters, whatever the keyboard gives (said before sending). The email of the last
successful login is remembered in the settings; a password never is, and it is cleared from
memory when the hub has answered.

What the hub can answer is said in words: wrong email or password; an account with this
email exists; too many attempts; **banned, with the reason and the day it ends**
(ANTICHEAT.md 6); the hub does not answer.

**In a browser** the login screen is the page's own form (`web/index.html`: email,
password, "new account", which asks for the password twice like the desktop's), so that
the browser can fill and remember them. The client runs
behind it from the start and tells the page what it wants through `gmStatus`: `login`
(show the form, with this notice), `login-wait` (the hub is being asked: the form is off),
`screen` (hide it: the canvas has a screen of its own). What is typed is left for the
client in `globalThis.gmLogin`, which the client empties on its next frame; the page keeps
no copy. No screen in a browser has a Quit button: the tab is closed by the browser.

### 4.2 Characters

The **selector** (since 2026-10-09; `showcase.rs`): one character across the whole frame,
over the map turning behind the screens. In the middle its body as the zone would draw
it, idle, with the prop of its weapon and the prop of its guard (a shield for a shield
wall), in the open with no well under it (the paperdoll of LOOK.md 5, `paperdoll_open`),
turned by a drag across it; its name over it in the title face, under the name a
line that reads the build (`striker in mail  |  water  |  action mode`), and under that
what it is: the preset's name or "custom", where the character is (`in town`, or `not
yet in the world`) and how long it was played. Left, a panel of the **attributes**: the
five with what each buys (`STR  17  blows x1.28`) and what else the body comes to
(stamina and focus with their regen, armour, evasion; health, speed and ward are in the
buys). No points or budgets: they are the character's page's (4.5). Right, a panel of
the **abilities**: the kit's rows (`Sword  weapon`, `Kick  secondary`, the actives, the
guard) and under them the words of the row picked, from its script
(`character::words_of`), the numbers faint on a line of their own and what it does under
them; the weapon's to begin with. Each panel is as tall as what it holds. Arrows on both
sides of the body, and the Left/Right (also Up/Down) keys, go to the previous and the
next character, round the ends; `3 of 10` says which.

The strip at the bottom holds the buttons: **Play** (also Enter; half as wide again as
the others, `showcase::lead_buttons`), **New character** (off when the account has its
ten), **Log out**, **Quit**, and a line under them for the notice. The one played last is shown first. An account with no characters goes to 4.3
at once. Before the hub's content has come the panels say so and the body stands
without its props (the names of its abilities are the content's).

Play asks the hub to enter with no zone named (7): where the character was if that zone is
open to it, otherwise the start zone. A refusal (the zone is full, a trial is asked for,
banned meanwhile) is said in the strip. A character that a zone has not finished putting
away is refused as busy: the entry is asked for again every second, twenty times, and the
screen says what it waits for.

After a zone, the list is asked for again every second until no character is listed as
being in one (the zone's last save), behind the screen: buttons stay on meanwhile, the one
that was shown stays shown, and a click on Play is made as soon as the list has come.

### 4.3 New character

The same selector over the hub's preset builds (the **archetypes**): the **blurb** (one
or two lines of plain words from the content, at most 160 characters) under the ribbon,
a row of their names under it with the one shown edged in gold (`Ui::mark`; the
editor's "start from" row marks its preset the same way), and the arrows and keys cycle
them; the body stands with the archetype's weapon and guard. The strip
at the bottom holds the **name** field (7) with **Create**, **Back** and **Quit** beside
it; an account with no character yet has **Log out** where the others have Back. No
numbers to spend here: the point-buy (MATRIX.md 9) is the character's page in the game,
and a build can be changed later anyway (PLAN.md 3.1). **Create** (also Enter) makes the
character and shows it in 4.2; the hub's rule for names is said before it is asked. The
archetypes are asked for again every second while they are missing (a request can be
lost).

On a phone (780 x 360 units at touch scale) the panels keep their width, the kit's list
scrolls and the body is what is left in the middle: nothing is cut.

Why one at a time (the director, 2026-10-09: "having full character show up (equipped)
in the whole center of the screen with stats on left and skills on right ... arrows left
and right to switch"): a class is chosen by what it looks like and what it does, which
a row of six names and a line of facts never showed; and the screen is the same on a
phone, where a list of six with a paragraph under it did not fit.

### 4.4 Entering

"entering <zone>, waiting for the zone", and **Cancel** (also Escape), which hangs up and
shows the characters. The screen steps aside when the zone has sent the character's
content; a zone that refuses, or a map this client does not have, brings the characters
back with the reason.

### 4.5 In the game

The HUD as before, and:

- **Escape** opens the **menu**: Resume, Inventory, People, Travel, Settings, Keys, Leave, Quit. While it is
  up the body stands (the frames sent to the zone hold no keys) and can be hit: there is no
  pause in a shared world, and the menu says so. A replay and an offline walk have the
  menu too: it is where Quit and the settings are.
- **Travel** lists the zones (name, map, how many play there); a zone that cannot be gone
  to says why instead (here, full, not from a browser). Go asks the zone being played to
  travel (`FromClient::Travel`, as the `T` key does for the zone named on the command line).
- **Keys** says what the keys do: nothing else tells a new player.
- **Enter** opens the chat line (5).
- **I** opens the inventory (ITEMS.md 6.1; under its grid the item bar's four cells, LOOK.md
  3.2, arranged by drag), and **E** the stall the body stands at (ITEMS.md 6): screens
  like the menu's, with the body standing while one is up.
- **K** opens the character (MATRIX.md 9.1): the thirty attribute points with what each
  buys, the frame, the armour, the aspects and the kit, edited anywhere; "Wear it" is
  offered beside the trainer (the client knows one by a body nothing hurts), or anywhere
  in a zone with none, where the zone wears it at the next respawn (the arena) or says why
  not (the dungeon). Away from the trainer, with nothing changed, or with a draft the
  pack refuses, "Wear it" is grey and **says why on itself** (3: the reason beside the
  pointer when it rests on the button or presses it; since 2026-10-08, when the director
  pressed the grey button and took the nothing that followed for a change that did not
  stick). The zone's answer is the page's note and a line on the HUD, so that it is read
  with the page up or closed: `worn, and saved` or `saved; worn at the next respawn`
  (MATRIX.md 9.1: the zone answers once the hub holds the build), else the zone's words
  (`stand by the trainer in the town`, `not in a fight: wait a moment`, `worn here, but
  not saved: ...`). Its shape since the evening of
  2026-10-06 (the director: "confusing and can't even read everything"; Gemini's review of
  a capture against Ether Saga's and Tales of Pirates' sheets agreed on every point): a
  panel 820 units wide where the frame allows, two columns under a row of presets. Left,
  the body: frame, armour (each button with its kit points), aspects, then
  `attributes   n of 30 points left` in gold over five rows of `STR  - 7 +  blows x0.88`
  (a name, the buttons, the value, what the points buy, in words). Right, the kit: the
  weapons, the secondaries over the guards, and the actives in two lists side by side
  (one that scrolls where the panel is narrow), every row in sight at 1080p; the kit's
  budget in gold over the actives; the actives in the build lit like a picked row
  (`RowMark::Picked`), not `[x]`. Under both, a line or two that read the ability under
  the cursor from its script (`character::words_of`: points, stamina or focus, cooldown,
  the aspect it needs, and what it does: `140 u around you: 40 electric, staggers, shock for
  0.1 s`); the bottom row holds the buttons and, beside them, what is wrong with the draft,
  else the zone's word, else `to wear it, stand by the trainer at the town board`. The
  same editor is the game master's Build tab (GM.md 4), with Close in its tab row.
  **A row that cannot be taken as the draft stands is off** (since 2026-10-09:
  `character::why_not`, `RowMark::Off`, drawn in the colour of a button that is off; a
  click on it picks nothing): an ability that needs an aspect the build lacks, a firearm
  in an action or RPG build, any weapon but a firearm and any guard in a gun build, one
  that is in the kit already or shares a cooldown group with one that is, a fifth active,
  and one that would take the kit over its forty points. Pointing at it reads why before
  its words, in the warning's colour (`Musket: a firearm: only a gun build holds one. 2
  pts, ...`). What is wrong with the draft is said with the abilities' names, not their
  numbers (`Stomp needs the ground aspect`).
- **P** opens the people (PARTY.md 8): the party, who asks something, who else is here;
  from it the trade window (which also comes up by itself when the zone opens a trade)
  and the tavern. Under the squad the HUD shows the party's other members, with the
  health the wire carries for those who are here.
- Bottom right while nothing else is there: where this is and the two keys nothing else
  tells of, `town  Esc menu  Enter chat`; and above it, standing at a stall, whose it is
  and its key, `Keeper's stall  E look`.

**Leave** hangs up on the zone (which saves the character through the hub) and shows 4.2
again. **Quit** logs out of the hub and ends the program. Closing the window is Quit.

When the zone's connection ends by itself (a kick, the zone stopped, the network), a client
with a person at it shows 4.2 with the reason; when the hub says the session is no longer
valid, 4.1. A client on autopilot or `--connect` ends, as before. While a zone is played
the client asks the hub something every ten minutes, so that the session (which lives a
day past its last use, 7) is still there when the zone is left.

### 4.6 Settings

Mouse sensitivity (a slider, 0.01 to 0.20 degrees per count), the sound's volume (a
slider, 0 to 100) and "no sound" (a box; SOUND.md 3.2), the size of text (by the window,
or one of 1 to 4; a size the window has no room for gives way, 3), invert the mouse's up
and down, fullscreen (not in a browser: the page has its own
button). Applied at once and saved within a second. Every button pressed on any screen
clicks (SOUND.md 3).

## 5. Chat

`Enter` opens a line at the bottom left; `Enter` sends it, `Escape` drops it. While the
line is open the keys are text and the body stands. A line is 1 to 200 characters after
trimming. The field takes what the font can draw; whatever else another client sends is
drawn as `?`, and the zone relays nothing that cannot be seen (a control character, a
zero-width or a directional mark).

The log shows `name: text` for 12 seconds per line, the last 8 rows, and the last 30 while
the line is open. A line of the zone itself (a refusal, a notice) is marked `* ` and drawn
in another colour: no name can begin with a star (7), and a long line's later rows are
indented, so nothing a person types can look like a line of the zone or of somebody else.
One message takes four rows at most.

`/ignore NAME` stops showing that player's lines (kept in the settings, matched the way
names are kept apart, 7; only what a player can be called is taken); `/unignore NAME`
undoes it; `/ignore` alone lists them. Speech cannot be reported (ANTICHEAT.md 5): not
hearing somebody is what there is.

The zone (PROTOCOL.md 8) **checks what it relays**, in the connection's own task before
the zone's thread sees it. An **account** may say **5 lines in 10 seconds** (a bucket of
5, one back every 2 s; its characters share it, and a connection that comes back finds
it as it left it); a line over that, and a line that is not one, is answered to its
sender alone and not relayed. Thirty refusals that are not yet forgotten (one is, every
ten seconds) end the connection ("flooding the chat"): somebody who overruns a line now
and then never gets there. The zone as a whole relays at most 10 lines a second (a burst
of 30) and tells a sender whose line that cost; chat is the one thing a slow receiver's
queue drops, from half full. Chat goes to everybody
in the zone, as before.

**Channels (Phase 12, PARTY.md 5).** `/p TEXT` says a line to the party, `/w NAME TEXT`
whispers to one character anywhere in the game, `/r TEXT` answers whoever whispered last
(`/r ` turns into `/w NAME ` on the line as it is typed, so that whom it goes to is seen
before it is sent);
`/invite NAME` and `/leave` are the page's buttons as words. A line that begins with `/`
and is none of these is not sent, and the line says what there is. A party's line is shown
`[party] Ana: text`, a whisper `[whisper] Ana: text`, one's own as it went out `[to Bojan]
text`, each in a colour of its own; a name cannot begin with a bracket (7), so nothing
said aloud looks like one. They are chat: the same account's five lines in ten seconds,
the same checks, in the same task. They go through the hub to wherever the hearers play,
and are not counted against the zone's ten lines a second. `/ignore` holds for all of it:
an ignored name's lines and whispers are not shown, its invitation is declined by the
client, its request to trade is not shown.

## 6. Who gets a key

Decided once per frame, in this order:

1. A text field with the focus (a screen's, or the chat line): text and editing keys are
   its; `Escape` and `Enter` are the screen's.
2. A screen (login, characters, new character, entering, the menu and its pages, the
   screen for no hub): `Tab`, arrows, `Enter`, `Escape`; the game gets nothing, keys that
   were held are let go, and the frames sent to the zone hold no keys.
3. The game, as before (`W A S D`, the mouse, `1`–`4`, `F9` to report, and the rest
   of the Keys page), plus `Enter` and `Escape`, and since Phase 11 `I` and `E`, and since
   Phase 12 `P`, and since 2026-10-06 `K`, which open a screen. They are keys only here:
   under rules 1 and 2 they are letters. (`Tab` for the tactical view was removed on 2026-10-06, COMPANIONS.md 6; `V` on 2026-10-07: the camera is the mode's, MODES.md 2. `Space` dodges in the action mode while the kit's dash is ready, `R` reloads in the gun mode; in the RPG mode the pointer is free, a click targets or walks, `Tab` cycles, `1`–`6` are the kit, the right button held turns the camera, MODES.md 5.5.)

`Q` no longer quits (it is a letter one types): Quit
is in the menu, and closing the window still works. In benchmarks nothing changes: no
screen is up, so rule 3 is all there is.

**In a browser** `Escape` gives the pointer back to the browser and never reaches the
page: losing the pointer is what `Escape` is there, so it opens the menu or drops the chat
line (the client asks the browser every frame whether it still has the pointer). The next
click on the canvas takes the pointer again.

## 7. Hub and protocol

The zone protocol stays **v5**; the hub is **v1.6** (HUB.md).

- **The players' messages** (`gm_hub_proto::player`, HUB.md 3.8): the nine requests a
  player's client makes and their answers, as enums of their own. They are the hub's own
  requests in another encoding (a stream that speaks them begins with an empty frame and
  their version); a client that speaks them does not carry the codec of everything a zone
  and a moderator can say, which the browser build paid 89 KB for. The hub answers every
  stream with its own version first: a client of another build is told *that*, in words,
  whatever else changed.
- `Content { session }` answers the content pack a zone would send and the presets'
  **blurbs** (`assets/content/builds.toml`, one per preset, at most 160 characters), so
  that 4.3 can show archetypes before any zone is entered. The blurbs are not in the pack:
  the pack is what zones send, and it did not change.
- `CharacterSummary.last_zone`: the zone the saved position belongs to.
- `Enter` with an empty zone name means: where the character was, if that zone is up, has
  room, asks for nothing the character lacks and can be reached by this client; else the
  hub's `--start-zone ID`; else the zone called `town`; else the first by name. A zone
  named outright that is full answers `Full`.
- A zone tells the hub how many it takes (`ZoneHello.max_players`); the hub counts who is
  there from its own database. A ticket nobody has used yet holds no seat (somebody could
  hold them all with tickets): two that race for the last seat are told apart at the
  zone's door. `ZoneSummary` says how many a zone takes, whether a browser can reach it,
  and what it asks for.
- A zone that will not have a character after claiming it (full after all, a client that
  left during the handshake) gives it back at once (`Release`): the character is offline
  again as it came, where it last stood is untouched, and it can enter elsewhere. A
  ticket nobody arrives with is forgotten when it has run out.
- **Sessions** live a day past their last use (they used to end a day after the login),
  thirty days at most, eight to an account. Logging out ends the account's characters'
  play only when it was the account's last session. An unknown email costs the hub the
  same password hash as a known one.
- **Names** (`gm_hub_proto::names`): two characters or more and 24 bytes at most,
  beginning with a letter, made of letters (ASCII and `č ć đ š ž`), digits, and single
  spaces, hyphens and apostrophes between them; and neither one of the game's own voices
  (zone, system, admin, moderator, gm, staff, ...) nor one of those as a word of its own
  or with a number behind it (`Zone 2`, `GM Bob`). Unique by **skeleton**: small letters
  without their marks, `i I l L 1` as one letter, `0 O o` as one, nothing between the
  letters. `AIdric` cannot be made beside `Aldric`. Migration `0007_names.sql` adds the
  column and keys the names from before it.
- Chat is checked and limited by the zone (5).

## 8. Settings

One file, `settings.toml`, in the user's configuration directory
(`$XDG_CONFIG_HOME/gamengine/`, `%APPDATA%\gamengine\`, `~/.config/gamengine/`); in a
browser, one `localStorage` entry. Flat `key = value` lines, read and written by the
client itself:

| Key | What |
|---|---|
| `hub`, `hub_cert` | the hub's `host:port` and the file its certificate is in (the desktop) |
| `email`, `character` | the last successful login and the character played last: remembered only when a person went through the screens |
| `sensitivity`, `invert`, `fullscreen`, `ui_scale` | 4.6 (`third_person` is read and dropped since 2026-10-07: the camera is the character's mode's, MODES.md 2) |
| `volume`, `mute` | the sound (4.6, SOUND.md 3.2): 0 to 100 (70 to begin with), and off altogether |
| `ignored` | the players not heard (5), comma separated; their lines make no sound either |

No password, no session. A value is a text in double quotes (`\"` and `\\` inside), in
single quotes, or a bare word, and may be followed by a `#` comment. A line whose key this
build does not know is kept as it stands and written back; a file that does not parse is
left alone and the defaults are used (and nothing is written over it). A change is saved
within a second (written beside the file and renamed), and a run that changed nothing
leaves the file alone; of two clients running at once, the one that saves last is the one
whose settings stay. A command-line option wins over the file. `--settings FILE` names
another file: a gate gives each run its own, and a scripted run reads none without it (2).

## 9. UI scripts

`--ui-script FILE` (a browser: the page option `ui-script` holding the text, honoured on a
`"dev": true` site only) feeds the screens what a person would do, one line at a time,
through the same entry points as real events:

```
wait screen characters        until that screen is up
field email                   give the field with that label the keyboard (a click on it)
type someone@example.com      characters, as if typed
key Enter                     Enter | Escape | Tab | BackTab | Backspace | Delete | Left | Right | Up | Down | Home | End | PageUp | PageDown
                              | I | E | P | K | G (the game's own keys that open a screen)
                              | F (the first item cell, the kit's, MODES.md 11.3: pressed for one frame of the game, as a person's key is)
click "New character"         the button, row, box or grid cell with that text
dclick "Aldric"               the same, twice
hover "sword  slash +2.0%"    the pointer over it, and left there (a tooltip after 150 ms)
drag "sword  slash +2.0%" weapon   a press on the first, moved over frames, let go on the second (LOOK.md 2.4)
drag "kit ×3  heals 300, used with F" "bar 8"   the same onto a cell of the item bar (ITEMS.md 6.1)
expect "Aldric"               some text on the screen contains it
expect image item/sword       a picture by its key was drawn (an icon in a slot, a portrait)
say at the characters         print `ui-script: at the characters`
where "Play"                  print `ui-script: where X Y Play`: its middle, in pixels
sleep 2
quit                          print `ui-script: ok` and end the program
```

Screens are named `login`, `characters`, `new character`, `entering`, `game`, `chat`,
`menu`, `travel`, `settings`, `keys`, `title`, `inventory`, `storage`, `price`, `stall`
(ITEMS.md 6), and `people`, `trade`, `tavern` (PARTY.md 8). A line that waits (`wait`, `expect`, `click`,
`where`) gives up after 30 seconds (longer than the screens themselves keep trying): the
script then fails with its line and what the screen showed instead, and the client exits
with an error. After every line the script lets two drawn frames pass, so that the next
line acts on the screen the last one made (a frame that drew nothing, under a covered
window, does not count). `click`, `field` and `where` find what is *called* that: a
button of that name, a field, a box or a slider with that label, a row whose first cells
are that; never something that merely contains it, and never a button that is off. A
script's click is a press and a release in one frame: what is clicked cannot go away
between them. `say` and `where` are for whoever runs the client from outside and sends it
real input (`xdotool`, a browser's debugging port): they say when the screen is ready and
where to click; in a browser they are printed as `GM-SAY`.

A UI script stays in the production browser build: it is honoured on development sites
only, and it does nothing a person at the keyboard cannot.

## 10. Budgets and acceptance (PLAN.md 11.8 Phase 10)

`budgets.toml`: the browser builds inside WEB.md's budgets, the desktop client inside its
10 MiB cap, and `[screens]` for the gate below.

`scripts/check-screens.sh`:

1. **Tests**, no display needed: the toolkit (every widget; `tidy`: at 640×360, 1024×600,
   1366×768, 1920×1080 and 3840×2160 every screen is inside its panel and the frame, every
   button's words inside the button, no two clickable things overlap), the screens against
   a scripted hub (every refusal in words, a ban with its end, a session that ended, a zone
   that refuses, an entry tried again, a slow hub), the menu and the chat, UI scripts, the
   settings file, the font, names, the players' messages. With a database: what the screens
   lean on at the hub and in a zone (`gm-server/tests/screens.rs`), and the chat limits
   (`loopback.rs`).
2. **`--desktop`**: the windowed client on a display of its own (Xvfb, the software GPU),
   started from another directory with a settings file that names only the hub. By UI
   script: New account, New character (an archetype picked from the list), Play; in the
   town a bot's line is seen and the script's line is heard by the bot; the menu's pages;
   Travel to the arena, back to the town, to the arena again (each map loaded both ways);
   Leave; the character is listed as being in the arena; then rounds of Play and Leave; the
   settings file has the email, the character and the box that was ticked, and no password.
   **By real input** (`xdotool`): the email is filled in from the settings, the password
   is pasted from the clipboard with Ctrl+V, Enter logs in, a click plays, Escape opens the
   menu, a click on Quit ends the program. Then a client with no hub named anywhere: the
   screen that says so, the offline walk, the menu, Quit.
3. **`--browser`**: both builds in headless Chromium: the page's form filled by the
   browser's own input events (a click into the field, text, Tab, text, Enter), then the
   canvas screens by UI script: a character, the town, chat both ways with a bot, the
   arena through the menu, Leave, the list.
4. Every earlier gate green: they start the client by command line and so run the
   autopilot.

Measured on the reference machine (Ryzen 7 4800U, Renoir iGPU):

| What | Measured |
|---|---|
| A frame with nothing up, in the town (uncapped, 1280×720) | 0.34–0.35 ms |
| The same with the menu / the settings / the keys page up | 0.36–0.38 / 0.38–0.40 / 0.39–0.40 ms |
| The characters / the new character screen over the backdrop | 0.36–0.37 / 0.39–0.42 ms |
| From the program's start to standing in the town, by script (software GPU) | 0.5–0.6 s (2.6 s with the CPU oversubscribed 2.5 times) |
| One round of Play and Leave, by script | 121–154 ms (577 ms oversubscribed) |
| What 20, 60 and 150 rounds add to the client's RSS | 2.9 MB each: nothing per round; threads 45 before, 44 after |
| Desktop client | 9,474,384 bytes (9,168,512 before; the clipboard is 294,856 of it) |
| Browser, WebGPU build | 967,052 bytes, 324,017 packed (938,472 before the screens; 1,044,174 with them and the hub's whole codec) |
| Browser, WebGL2 build | 2,956,609 bytes, 896,912 packed |
| Tests in the workspace | 310 (266 before) |

## 11. Deliberately absent

- The inventory, what is worn, the storage and a stall came in Phase 11 (ITEMS.md 6); the
  people, parties, the party's line and whispers, a trade between two players and the
  tavern in Phase 12 (PARTY.md 8). Crafting and friends have no screen yet. Sound: Phase
  13.
- The point-buy editor and the model browser (PLAN.md 3.3's "Build & Model Browser"); a
  tutorial; key rebinding; a gamepad; localisation (the screens are in English).
- Deleting or renaming a character (the hub has no such request yet).
- Password reset, email verification, two-factor login, "stay logged in".
- Text selection and copying, an input method editor, right-to-left text, any script
  beyond the font of 3. Paste in a browser's canvas fields (a name, a chat line).
- Logging in again without leaving the zone when a session ended under a player: the zone
  is played on, and the login is asked for when it is left.
- More than one hub, a server browser. A scoreboard and a player list.
- Ending an account's other sessions from one client ("log out everywhere"); a session
  ends by its own logout, by thirty days, by a ban, or by the ninth login.
- Leaving or quitting in the middle of a fight costs nothing yet (open decision, 12).
- Whether Chrome offers to save a password typed into the page's form was not tried by
  hand (the form is sent without a navigation).

## 12. Proposed numbers and review log

Proposed, for the director: the scale rule of 3; 200 characters, 5 lines in 10 seconds and
30 refusals of chat; the zone's 10 lines a second; 12 seconds and 8 rows of log; the rules
for names and what counts as one name; the reserved words; ten minutes between the
client's words to the hub; twenty tries, a second apart, for an entry and for the list; the
fields of the settings; the start zone rule; that `Q` no longer quits; that a browser logs
in on the page's form and not on the canvas; that sessions slide.

Open decision: **combat logging**. Leave and Quit are instant and free: a body about to
die can be taken out of the world by its player. The usual answers (the body stays for
some seconds after the client left; leaving is refused while in a fight) change what a
fight is, so they are the director's.

Open decision: **how many of an account's characters may play at once**. Nothing limits
it today (ten clients, ten characters). It bears on a zone's seats, on chat (the account's
bucket is shared for that reason), and on what a party is.

### 12.1 Design review (before the code)

An independent agent reviewed the v1 draft (Google AI Studio answered HTTP 402 "prepayment
credits are depleted" on 2026-10-02; the review should be repeated there when it has
credit). Fourteen findings:

| # | Finding | Verdict |
|---|---|---|
| 1 | A character can be wedged: a zone that claims it and then refuses the join leaves it "in" that zone; a full zone is found out only by the zone | **Accepted.** The zone gives the character back on every refusal after the claim and when a client leaves mid-handshake; the hub counts a zone's room itself and answers `Full`; the client retries `Busy` and refreshes the list |
| 2 | A browser's login is a dead end: wrong password reloads the page, the form's zone field acts for a link | **Accepted.** The page-form protocol of 4.1; no Quit in a browser; `zone` is a development option |
| 3 | A desktop client without a terminal: relative asset paths, no hub named, errors nobody reads | **Accepted.** `install_root`, `client.toml`, the screen for no hub, `client.log` |
| 4 | In a browser Escape never reaches the page | **Accepted.** Losing the pointer lock is Escape (6) |
| 5 | Passwords: the font's set is too small for them, and nobody types a manager's password | **Accepted.** A password field takes any character and can be shown; paste on the desktop |
| 6 | Sessions end a day after the login, in the middle of play; logging out of one client kicks the other | **Accepted.** Sliding sessions, the client's word every ten minutes, the logout rule. **Not done:** logging in again without leaving the zone (11) |
| 7 | Chat can be made to look like somebody else's line or the zone's; names can imitate | **Accepted.** The zone's mark and colour, indented rows, four rows a message, names by skeleton, reserved words, `/ignore`, glyphs two dots apart |
| 8 | The chat limit sits behind the zone's queue, where a flood has already cost | **Accepted.** Checked in the connection's task; a zone ceiling; droppable fan-out; the kick. **Not done:** a limit per character across connections (one character has one connection), chat in the swarm gate |
| 9 | Dead ends: Play on a character whose zone is gone, travel to a zone that will refuse, the eleventh character | **Accepted.** The entry's fallbacks, the travel list's reasons, New character off at ten |
| 10 | The browser build's budget: the screens will not fit beside the hub's whole codec | **Accepted.** Measured 1,044,174 bytes with the screens; the players' messages brought it to 954,894. **Rejected in part:** compiling UI scripts out of the production build (9) |
| 11 | The gate tests the script, not the input: a real key and a real click never happen | **Accepted.** The run by real input, the browser's form by the browser's own events, rounds of Play and Leave, both maps both ways, layout assertions at five sizes |
| 12 | One scale for every window is wrong at the ends | **Accepted.** Four steps, a setting, a smallest window |
| 13 | The menu only exists in a zone; a new player is never told the keys | **Accepted.** The menu everywhere, the Keys page |
| 14 | Blurbs in the pack bump the protocol for words; an unverified address reads like a verified one; login timing tells which emails exist; Leave blocks a frame for up to a second; leaving mid-fight is free | **Accepted:** blurbs beside the pack (no bump), the line on the form, the decoy hash, the goodbye said off the frame. **Open:** combat logging (above) |

### 12.2 Found by running it

- A script's line acted on the frame before the last line's effect (text typed into the
  field that had the focus a frame earlier): two frames of rest after every line.
- Button words wider than their buttons, a title wider than its panel, a paragraph cut
  off: buttons are sized by their words, and `tidy` asserts the layout at five sizes.
- `g j p q y` without descenders read as capitals: the eighth row.
- **A click swallowed by a button that went grey for a moment.** The list asked for again
  behind the characters screen switched Play off for the length of each request; a click
  that began while it was on and ended while it was off did nothing (one run in six of the
  gate; a person would have clicked again and never known why). The background request
  no longer switches anything off; the entry is made when the list has come. The same
  request used to pull a person off the new-character screen: only the first list after a
  login chooses the screen now.
- A script's click two frames long lost the same race against a travel in flight: script
  clicks are one frame, and the corner of the game screen names the zone, which is also
  what tells a script (and a person) that the travel is over.
- Real keys reached nobody under a display with no window manager until the window was
  given the keyboard: the gate does that, as a window manager would.
- Settings were written at a clean exit only, and in a browser never: saved within a
  second of a change now.
- **A re-entry refused as "logged out".** The browser gate through the hub came back a
  few seconds after its first visit and the zone would not have it: the zone had been made
  to remember a kick for a character it had no body for, so as to refuse a client still on
  its way in. A new entry is not that client. The memory went; a body the hub does not
  have here is found out by its first save instead, which the hub refuses.
- **A logout that beat its own last save.** A client that quits hangs up on the zone and
  logs out of the hub in one breath, and the hub's kick could reach the zone before the
  zone's save of the character reached the hub: with the zone answering such a kick by
  giving the character back, the save then found it gone, and the last position and
  minutes were lost. The zone now knows which characters' leaving saves are on their way,
  and leaves those alone.

### 12.3 Code review (after the code)

Three independent agents, each with one part of the diff and none of the author's
conclusions (Google AI Studio still answered HTTP 402). 23 findings on the client, 5 and a
list of smaller ones on the hub and the zones, 13 on the page and the gate.

**The client**

| # | Finding | Verdict |
|---|---|---|
| 1 | In a browser, after a map download was cancelled or failed, the next entry into that zone played on the map that was still loaded | **Fixed.** A map's hash travels with the map: what is loaded is what `switch_map` loaded |
| 2 | Enter took the screen's main action before the button with the keyboard saw it (Tab to Log out, Enter: the character played; Tab to Back, Enter: a character was made) | **Fixed**, on every screen; tested |
| 3 | A double click on Travel travelled: the list came up under the second press | **Fixed.** A double click is two presses on one row |
| 4 | No way out of the new character screen when the archetypes never arrived; no Log out for an empty account | **Fixed.** Asked for again every second; Log out |
| 5 | The size-of-text slider moved under the hand that set it, and its keys did nothing | **Fixed.** A choice of five |
| 6 | A person's settings changed scripted runs (a ticked "fullscreen" in the frame-rate gate) | **Fixed.** Such a run reads no settings it does not name |
| 7 | A session that ended left a queued entry and a parked list request behind, which then threw out the next login | **Fixed**; tested with a slow hub |
| 8 | The character picked by the screen could be below the rows shown | **Fixed.** Scrolled into view |
| 9 | Ctrl+letter typed the letter (Ctrl+A into a password); keys without a place on the keyboard's map lost their text | **Fixed** |
| 10 | The pointer was taken for a window that did not have the keyboard | **Fixed.** By itself only when focused |
| 11 | Two holes left in the swallowed-click fix: the archetypes' request greyed the buttons too; the frame of an answer drew nothing | **Fixed.** Two actions a frame |
| 12 | On autopilot, Cancel while entering ended the program | **Fixed.** Whoever cancels takes over |
| 13 | A held Enter or Escape repeated (the line it opened closed again; a refused login was sent again) | **Fixed** |
| 14 | Half-size HUD text at scale 1 was half a dot | **Fixed** |
| 15 | A notice cut in mid-sentence, unseen by the tests | **Fixed.** Two lines; what is cut is recorded and asserted on |
| 16 | The settings file: a byte-order mark, a comment after a value and single quotes each lost the hub; unknown keys were dropped; `/ignore a,b` came back as two names | **Fixed.** Two clients saving at once: the last one's stay (8) |
| 17 | The command line's password was never cleared; a browser kept a refused one | **Fixed.** Not done: zeroing memory. **Rejected:** hiding a shown password from a failed script's report (a script is a developer's, and the report is what the screen showed) |
| 18 | A person was still thrown out of the program by a zone naming a bad map, a hub certificate that could not be read, a hub name that did not resolve | **Fixed.** Said on a screen |
| 19 | The clipboard could block a frame for seconds; Quit could wait ten seconds on a hub that does not answer | **Fixed.** A thread; two seconds |
| 20 | The browser's line to the hub reused a session that had closed by itself | **Fixed** |
| 21 | Between two zones an open chat line was invisible and kept the keys; Leave was off | **Fixed** |
| 22 | `click Play` clicked the row of a character called Player while the button was off; rest frames counted frames that drew nothing; the script's patience equalled the screen's | **Fixed.** Keys before text within one frame: left as it is |
| 23 | Cancel during a slow connect sent no goodbye: the zone kept the body ten seconds | **Fixed** |

**The hub and the zones**

| # | Finding | Verdict |
|---|---|---|
| 1 | A ticket nobody used held a seat for good; seven accounts could fill a town with them | **Fixed.** A seat is a character that is there; transits are swept when their ticket has run out, and on a ban. **Open:** a cap on an account's characters at play (above) |
| 2 | Giving a character back by a leaving save wrote the refusing zone as where it last stood, and was itself refused when the build was the reason | **Fixed.** `Release`; the build is checked at `Enter`; a claim that fails after its commit gives the character back |
| 3 | The chat limit was per connection: reconnecting refilled it | **Fixed.** Per account, kept a minute |
| 4 | Refusals never decayed (an honest player kicked hours in); lines that were not lines were not counted | **Fixed** |
| 5 | Sliding sessions had no end, no bound per account, and were swept on every request | **Fixed.** Thirty days, eight, once a minute |
| low | Entry with no zone named used the heartbeat's count and answered `NotFound` when everything was full; a zone could say it takes nobody; a browser could be handed to a zone it cannot reach; a one-letter marked name passed; `Zone 2` and `GM Bob` passed; the migration lower-cased by the database's language; nothing told a zone or a tool of another build so; the room count had no index; streams could ask nothing for ever; chat was dropped only from a full queue; a full zone refused a character that was only replacing itself | **Fixed**, each. Names from before the rule are keyed, not re-judged |
| tests | `last_zone` was never asserted positively; the release was checked by location only; a comment said the hub gives a transit up by itself | **Fixed** |

**The page and the gate**

| # | Finding | Verdict |
|---|---|---|
| 1 | The gates wrote their test hub into `target/web/config.json` and an interrupted run left it there (it had) | **Fixed.** Each run serves a directory of its own |
| 2 | A failed run could end without a FAIL line (a pipeline that found nothing) | **Fixed** |
| 3 | Fullscreen hid the login form and the status line | **Fixed.** The stage is what goes fullscreen |
| 4 | The browser driver left browsers behind and could hang for ever | **Fixed.** Errors end the browser, a watchdog, a `timeout` around it |
| 5 | On a Wayland desktop the "display of its own" was the person's | **Fixed** |
| 6 | Start-up by sleeps, no liveness checks; a kept directory's old files passed for new | **Fixed** |
| 7 | "Leave site?" at the login form | **Fixed** |
| 8 | A client that died left a live form that took a password | **Fixed** |
| 9 | A new account in a browser typed its password once | **Fixed** |
| 10 | Weak checks (a word the row itself contained; the word "password" instead of the password; a missing stamp skipped its bound) and no look at threads | **Fixed** |
| 11 | The bot's "every 3 seconds" was 9.6 in a 20 Hz zone | **Fixed** |
| 12 | (as the client's 20) | **Fixed** |
| 13 | Smaller: the display number raced, clients outside the cleanup, a logout with no form handler on development pages, `"dev": "false"` taken for true, a pointer given late | **Fixed** |

### 12.4 Review by Gemini 3.1 Pro, after the fact (2026-10-03)

The Gemini account had no credit when this phase was written (12.1 and 12.3 are independent
agents'); with credit back, Gemini 3.1 Pro read this document and the whole commit, asked for
what the earlier reviews missed. 2 findings.

1. High, *a character whose last stay a zone has not yet given back counts against that
   zone's room, so on a full zone its `Enter` is answered `Full` (not retried) before
   `begin_enter` could answer `Busy` (retried)*: **accepted, fixed**. The asking character is
   left out of the count (`zone_population(zone, except)`), for `Enter` and for a handoff
   alike.
2. Low, *a paste longer than a field holds is sieved: a character that does not fit is
   skipped and smaller ones after it are still taken*: **accepted, fixed**. The first that does
   not fit ends the paste.

Its verdict on the earlier reviews: sound overall.

### 12.x The selector (2026-10-09): Gemini's review of the 1080p captures

Two reviews (`gemini-3.8-flash --search` and `gemini-3.1-pro-preview`) of the captures
of 4.2, 4.3 and the editor with their source, findings only. Accepted: the blurb moved
from under the body to under the name (both asked); brackets round the tab shown
replaced by a gold edge (both); the derived stats pruned of what the buys already say
(health, speed, ward) and the points and kit tallies dropped from the selector (flash 4,
pro 1 in part); Create and Play wider than the other buttons (flash 5); off rows dimmer
than faint cells (flash 7; pro 4 asked the opposite, lighter: the dimmer wins, an off row
must read as off before it is read); the ability's numbers on their own line, what it
does under them (pro 10); the 360-unit frame no longer cuts the derived rows or the
blurb (flash 8: the blurb left the body's column, the attributes panel is sized to its
rows). Rejected: "the action game" as engine jargon (flash 1: the modes are the game's
own words, MODES.md; it reads `action mode` now); dropping `u` and `u/s` and the
multipliers for "Movement 240" (flash 3: the game is measured in units everywhere and
the character's page says the same); the lock reason on a line of its own and in a
panel of its own (flash 6, pro 5: it is on the one line there is, in the warning's
colour, before the words); hiding the arrows when the tabs are there (flash 10: the
director asked for arrows); hiding the attributes under 800 units (pro 6: they fit); the
editor's columns, aspects grid, dynamic aspect cost and a larger face for its headers
(flash 9, pro 7–9: the editor is another change).
