# The look: props in hands, the skin, the screens as grids, the hotbar (v1)

Phase 14 (PLAN.md 11.8), second half; CONTENT.md is the first. The game plays but looks
like its test harness: a grey mannequin with empty hands, panels of flat colour, lists of
words in a five-by-seven font, three bars and no sign of what a body can do or when it can
do it again. This is the contract for what changes: a weapon drawn in the hand of whoever
wears or swings one, a toolkit that draws pictures, the screens of ITEMS.md 6 and PARTY.md 8
redone as grids of icons with tooltips and drag, and a HUD with a portrait, the party and a
hotbar whose cells show their cooldowns — the menus of Ether Saga Odyssey, measured in
kilobytes.

Code follows this document; a change to either goes in one commit. v0 was the proposal
(2026-10-03, reviewed in 11.1); the director's decisions are in 9; v1 is what Phase 14
built (11.2: found by running it; 12: measured). The same evening the director played it
and three things changed (11.4): an atlas per UI scale and drawn faces instead of one
atlas of dots magnified (2.2, 2.3), the grip (6.3), two pixels a dot at 1080 lines
(CLIENT.md 3). Then, having played it in a browser, the fight had to be seen: section 13
(the wedge a swing hits and its slash, bolts, areas, sparks, names over bodies; protocol
v9).

## 1. Principles

1. **Everything the toolkit draws is still quads into one atlas, in one draw call.** The
   HUD of Phase 7 draws rectangles and glyphs as quads into a font atlas (`hud.rs`). The
   atlas becomes RGBA and holds the skin, the icons and two real fonts beside the old
   glyphs (CONTENT.md 5.2); a picture is a quad with other texture coordinates. No new
   pipeline, no second texture, no text rasteriser in the client.
2. **Never wait for art** (CONTENT.md 1.4). Until the atlas is loaded, and on a machine
   whose bundle is missing, the toolkit draws exactly what it draws today. Every piece of
   the skin has a plain fallback (a plate, a line, a glyph); a missing icon is a glyph.
3. **The rules of the screens do not change** (ITEMS.md 6): a thing is picked by what it
   is, nothing is picked once it is gone, nothing is said the hub did not say, a screen
   takes only the answer it waits for. Icons, tooltips and drag are new ways of pointing at
   a thing and acting on it, not new rules. Every action a drag does, a button does too, so
   a UI script and a keyboard reach everything.
4. **The sim and the wire carry keys, never looks** (CONTENT.md 1.5). A body's `Look` is
   two indices; the client finds the files.
5. **Measured.** The hotbar and the panels cost a fraction of a frame on the iGPU; a prop is
   one more draw call for a body that holds something; the browser build stays under its
   megabyte or the director raises it knowingly (9).

## 2. The toolkit, second version (`ui.rs`, `hud.rs`, `font.rs`)

### 2.1 Primitives

`Canvas` keeps `size`, `rect` and `text` and gains:

| Call | Draws |
|---|---|
| `image(rect, piece, tint)` | a piece of the atlas stretched to `rect`, times `tint` (white = as painted) |
| `frame(rect, piece)` | a **nine-slice**: the piece's corners unscaled (a texel a pixel at the atlas's own scale), its edges stretched one way, its middle both ways; insets come from the atlas's piece table |
| `wedge(centre, radius, from, to, colour)` | a filled circular sector in sixteenths of a turn, as a fan of triangles: the cooldown sweep |
| `layer(n)` | everything after this goes to layer `n` (0 plates and wells, 1 bodies drawn into the screen (5), 2 pictures and text, 3 the tooltip and the dragged icon, 4 the cursor, unused); the HUD issues one draw per layer in order. The game's own HUD (3) stays on layer 0 in call order, so a screen's plates cover it as they covered the bars before there were layers |
| `text` with a `font` | `Font::Small` (the five-by-seven of today), `Font::Text` (the atlas's proportional face, a line of 12 dots), `Font::Title` (a line of 18); a face not in the atlas falls back to `Small` |

An icon drawn in a slot or a portrait is recorded as `Seen { kind: Image, text: <icon key>
}` (`item/sword`, `portrait/striker_mail`), so a test or a script asks `expect image
item/sword` the way it asks for a label.

### 2.2 The atlases (`ui.gma`, `ui2.gma`, `ui3.gma`, `ui4.gma`)

**One atlas per density**: texels a dot, 1 to 4, the UI's scales (CLIENT.md 3). The client
draws with the atlas of its scale, so **a texel is a pixel and nothing is magnified**: a
glyph is rasterised from its outline at the size it is seen at, an icon is baked at 64
pixels for a scale of two and not at 32 and doubled, a frame's hairline is a pixel or two
whatever the scale. (v1 had one atlas of dots drawn two to four times larger with nearest
sampling, which is what "ultra low res" was: 11.4.)

Each is written by `gm-tools content build` (CONTENT.md 5.2) and read in one call with
`miniz_oxide` (already linked): `GMA2`, the **density**, `w`, `h` (each ≤ 2048), the **piece
table** (name hash, rect, nine-slice insets), the **glyph tables** (per face: the line
height and the ascent; char → cell, bearing, advance), the **icon table** (key hash →
cell, `32 × density` square), then the RGBA8 texels, one zlib stream whose inflation is
bounded by `w × h × 4` (the atlas has its own reader and its own bound, not the `.gmm`
reader's 2 MiB). **What a layout counts with is in dots and is the same in every one of
them**: a face's line height and ascent, and a glyph's advance, written in quarters of a
dot; cells, bearings and insets are texels. A screen is therefore laid out the same at
every scale and with whichever atlas, and a test of the tool holds the four to that
(`the_atlases_differ_in_texels_and_in_nothing_a_layout_counts`).

The manifest lists them (`atlases`: density, file, hash, bytes). The client reads `ui.gma`
at start and asks every frame for the atlas of the scale it draws at
(`Content::want_atlas`): a change of scale (the window, the setting) reads the other file
once (in a browser: fetches it), and until it is here the HUD draws with what it has, the
same layout, coarser. A scale without an atlas takes the densest one under it. The
sampler magnifies with nearest (a nine-slice's middle, a thinner atlas standing in) and
minifies with linear (small print at half the scale, a panel that had to give way to a
small window).

The five-by-seven glyphs of `font.rs` are copied into each too (a dot a square of the
density), so the client draws with one texture whether an atlas loaded or not: the
**built-in fallback atlas is made as RGBA8 at start** from the same glyph bitmaps (white,
the dot as alpha), at density 1.

Pieces the toolkit asks for, by name (`skin.toml` maps each to a picture and its insets):

`panel` `panel_title` `well` `button` `button_hot` `button_down` `button_off` `field`
`field_focus` `slot` `slot_hot` `slot_picked` `slot_worn` `slot_off` `bar_frame` `bar_fill`
`portrait_frame` `hotbar_cell` `hotbar_key` `tooltip` `check_off` `check_on` `slider_rail`
`slider_knob` `scroll_rail` `scroll_knob` `cursor` `cursor_drag` `coin_gold` `coin_silver`
`mark_new` `mark_taken` `mark_worn`

A missing piece draws its fallback (a plate or a line of today's colours). A tint is the
colour the old toolkit used (the skin may be greyscale and tinted, or painted).

A piece is `name.png` at one texel a dot, with `name@2x.png`, `name@3x.png` and
`name@4x.png` beside it: the same piece drawn finer, each exactly that many times the
first one's size (the tool refuses another size). A density without a file of its own is
the first picture enlarged, so one picture is enough to begin with. `skin.toml`'s insets
are in dots. A nine-slice's middle is stretched, so nothing in a middle may vary along the
way it is stretched: v1's grain became squares the size of a fingernail; the pieces are
shaded top to bottom instead (`scripts/dev/skin-gen.py` draws all four densities, each
four times oversampled).

### 2.3 Fonts

Two faces rasterised by the tool from font files in `assets/content/ui/` (OFL or CC0 only,
named in LICENSES.md), **once per density from the outlines**, with the coverage the
rasteriser gives as alpha: `Text` (**Fira Sans Medium**) and `Title` (MedievalSharp), each
for the **same generic set of characters** (13.12, since 2026-10-08; before it ASCII and
Gaj's ten letters, and a stack read `kit ?3 of 5`): printable ASCII; of the Latin-1
Supplement the signs `¡ § « ° ± · » ¿` and everything from `À` to `ÿ` (the accented
letters of the western tongues, `×` and `÷` among them); Latin Extended-A whole, `Ā` to `ž`
(Gaj's letters, and the Polish, Czech, Slovak, Hungarian, Romanian, Turkish and the rest);
and the typographic marks the game and its players write, `– — ‘ ’ “ ” … ‹ › • − → €`: 308
characters (`gm_tools::content::charset`). A character the font has no glyph for is left
out of that face rather than baked as the font's box (MedievalSharp lacks `→`), and the
client draws what a face lacks as `?`, as before. `skin.toml` gives each its height in dots
from the top of its ascenders to the bottom of its descenders (15.5 and 20). The metrics
are made for layouts and are the same at every density: the **ascent** is the height of
the face's capitals to the nearest dot (9 and 14: the baseline lies that far under the
line's top, and an accent stands over the line), the **line** is that and the descent (12
and 19), an **advance** is the outline's own to the nearest quarter of a dot (the pen
runs in quarters and each glyph lands on a whole pixel). The client knows nothing of
TrueType. The scale rule is CLIENT.md 3's (whole dots, 1–4 by the frame or the setting);
layouts count in dots as before.

The words of the game's own HUD (the bars' numbers, the hotbar's keys and names, the
squad, the messages, the corner hints) are in the text face too when the atlas has it
(`Hud::print`, `Hud::label`), and small print (the corner hint, a slot's name that is
wider than the slot) is the same face at half the scale, never under one. The
five-by-seven face remains what is drawn without a bundle.

### 2.4 Widgets

Kept: label, button, field, list, check, slider, paragraph. New:

- **Grid** of **slots**: `n` columns of 36-dot cells (a 32-dot icon with a 2-dot rim); a
  slot holds a thing by id (an item, a listing, an offer), its icon, a corner mark (`worn`,
  `new`, `taken back`, a stack count later) and, under a stall's cell, its price. Hover
  shows the tooltip; a press picks; a press that moves four dots with the button down starts
  a **drag**. A grid whose things do not fit is scrolled in rows: by the wheel and the keys
  as lists are today (CLIENT.md 3), and by a **scrollbar** at its right edge (`scroll_rail`,
  `scroll_knob`: a drag on the knob, a click on the rail), which lists get too, so a
  machine without a wheel reaches everything.
- **Tooltip**: after 150 ms of hover over a slot, the item in full as 6.1 of ITEMS.md puts
  under the list today (what it is, its budget, everything it does, what is worn in its
  place, what it is made of), in a `tooltip` frame beside the pointer, inside the frame.
  Over a hotbar cell: the ability's name, cost, cooldown and what it does (the pack's
  words). Over a party frame: the member's name and state.
- **Drag**: the icon follows the pointer on layer 3 (`cursor_drag`); a **drop target** is
  any slot or panel that says it takes the thing (the weapon slot takes a weapon, the trade
  pane takes an unworn item, the storage grid takes an item, the stall grid at one's own
  stall opens the price page); a drop elsewhere is nothing. The drop does exactly what the
  button for it does (`Wear`, `Offer`, `Store`, `Take`, `Sell`), through the same code,
  with the same gate (one change a second, ITEMS.md 5).
- **Paperdoll**: a rect that a body is drawn into (5): the own frame, armour tint or model,
  the held prop, idle, turning as the pointer drags across it.
- **Portrait**: a 32-dot icon from the manifest (CONTENT.md 5.3: a frame's mannequin or
  official avatar, head and shoulders) in a `portrait_frame`.
- **Bar** with a frame piece and a tinted fill, numbers inside.

UI scripts (CLIENT.md 9) gain `hover NAME` (the pointer there), `drag NAME TARGET` (a
press on the first, the pointer moved a quarter of the way a frame, let go on the second:
real input is still the gate's xdotool) and `expect image KEY`.

A grid draws every slot of its holder (24 for the inventory, 60 for the storage, 12 for a
stall, six and twelve in a trade), the empty ones as wells. A cell is found by a script as
its row was (`sword  slash +2.0%  worn`; a listing adds its price), and the `Seen` of the
picked thing's words under the grid stays, so every script and test of Phases 11 and 12
runs unchanged.

## 3. The HUD in the game

### 3.1 Top left: the own body

A `portrait_frame` with the own portrait, the name, and the three bars (health, stamina,
focus) in `bar_frame`s with numbers, as today's bars but framed; aspects as the two element
colours on the frame's rim; the armour class as the frame's metal. Under it, **party
frames**: one per other member present (PARTY.md 8; health is on the wire for party
members, PROTOCOL.md 5), a portrait, the name, a health bar, dimmed when away or dead; at
most four.

### 3.2 Bottom centre: the hotbar

One cell per ability of the kit in the order a hand finds them: `LMB` primary, `RMB`
secondary, `C` guard, `1`–`4` the actives (Shift is also 1: `sim_input`); `app::hotbar`
makes the cells, and `--report` prints them (`hotbar=LMB:sword:ready:1.00,...`: key,
ability, state, how ready). Each cell: the
ability's icon (CONTENT.md 3), its key in a `hotbar_key` tab, and its state, read from the
own predicted mover (`Mover::cooldowns[slot]` against the predicted tick, the ability's
`cooldown.ticks`, its `cost`, the own stamina and focus, `statuses.silenced()` for an
elemental):

- **ready**: the icon as painted;
- **cooling**: a dark `wedge` sweeping clockwise from twelve as the cooldown runs out, the
  seconds left in the cell when more than one;
- **unaffordable** (cost above the resource): the icon dimmed and the resource's bar
  flashes once on a press;
- **silenced**: a slash across an elemental's icon;
- **active** (its script runs, or the guard is held): a bright rim.

A cell is a button too: a click fires the ability as the key would, so a tablet has a way.
Nothing here is a rule: the mover decides, the cell only shows what it will decide.

**The item cells** (2026-10-08; the co-owner: "there should be an item bar now, not later.
there will be more items"). After the abilities, a wider gap (14 dots) apart, **four cells
for the stacks the character uses with a key** (MODES.md 11.3, ITEMS.md 1): on `F`, `8`,
`9`, `0`, the keys free in every mode (the gun's weapons take `1`–`3` and its actives
`4`–`7`, the RPG's kit `1`–`6`; `G` is the game master's page). `F` is the first cell's
key, so `F` uses a kit as it always did: the kit sits there unless the player moves it.
A cell holds a **template** (`kit`; a stack merges, so what is held is the kind, not a
row): it shows the stack's icon (CONTENT.md 3; its name in small print while a stack has
no icon), its key in a `hotbar_key` tab, and the count `×3` bottom right; while the use
runs (1,500 ms) the same clockwise sweep as a cooldown; and it is drawn **bare** when the
character carries none of its stack or nothing is set on it ("it should obviously be
empty if there's nothing to use"). A press that uses nothing says why in yellow over the
item cells for a second (MODES.md 11.3). A cell is a button for its key, like the
abilities' (WEB.md 3.5). `app::item_cells` makes them; `--report` lists them after the
abilities in `hotbar=` (`F:kit:ready:1.00`, `8:-:empty:1.00`; `using` while the sweep runs)
and in `items=F:kit:3,8:-:0,...` (key, template, carried), with `item_refused=<words>`
while a refusal shows.

The arrangement is **the character's, kept by the hub** (ITEMS.md 4: `EconOp::Bar`,
`SetBar`; `GearReading.bar` carries it to the zone with the gear) and made in the
inventory (CLIENT.md 4.5, ITEMS.md 6.1): under the grid a row of the four cells as drop
slots named `bar F`, `bar 8`, `bar 9`, `bar 0`; a stack dragged from the grid onto one
sets it (and leaves the cell it sat on before: a kind sits on one cell, and the hub
refuses it twice), a cell dragged onto another swaps the two, a cell dragged into the
grid empties it (2.4's drag, the toolkit's own). A character that never arranged it has the first
stack it carries that heals (the kit) on `F`. Using a cell: the input names it
(PROTOCOL.md 4, `use_slot`), the zone counts the cell's stack from what the character
carries and applies what the template does (`heals` for the kit; another effect in
`items.toml` would be applied the same way), and tells the hub to consume one.

### 3.3 Above the hotbar: statuses

The own statuses (the own block of every snapshot) as 24-dot icons with a ring of the time
left and the stack count; in the pack's status order; harmful ones with a red rim, helpful
with green.

### 3.4 Glyphs

An ability or status without a picture is drawn by the client as its name in the cell
(`sword`, `firebolt`, `parry`; whole, all the hotbar's names in small print when any of
them is wider than a cell; a status's as much as fits), in the HUD's words: plain on
purpose, so that a missing picture is seen and fixed, but a new ability has a cell the
moment it exists. (v0 proposed glyphs by verb kind; the letters say more for
less, and the pictures are the plan: CONTENT.md 8.) An item with a model gets its icon
baked; one without (an armour, today) shows its first two letters in the title face.

### 3.5 Kept

The corner hints (the zone, `Esc menu`, `E look`), the crosshair, the chat (CLIENT.md 5):
skinned where they have a frame,
otherwise as they are.

## 4. The screens as grids

Every page keeps its name, its buttons and its words (so every UI script of the gates still
runs), and shows its things in grids instead of rows:

| Page | Grid(s) | Besides |
|---|---|---|
| inventory | the 24 slots, 6 × 4 | the purse as two coins with numbers (gold, silver: PLAN.md 5.1 as corrected); the **equip** panel beside the grid: the paperdoll (5) with the weapon and armour slots at its side; the picked thing's words under them (three lines) and the tooltip on hover; **Wear**/**Take off**, **Sell**, **Store**, **Storage**, **Close**; a drag onto the weapon or armour slot wears, a worn thing dragged into the grid comes off |
| storage | the 60 slots, 8 across, scrolled | **Take**, **Back** (a drag between storage and inventory is Phase 15's: the two are pages, not one screen) |
| stall | the keeper's 12 listings, 6 × 2, the price under each cell | the buyer's purse, the picked thing's words and price, **Buy** / **Take back**; a listing is picked, never dragged |
| price | two fields: gold, silver | **List it**, the whole said back in words |
| trade | what you give (six slots) and what they give (six), each with its coin and `accepted` on its header line, marks on their cells (`new`, `taken back`, a dim struck cell for what is gone), the carried things in two rows of six under them | a drag from the carried into your offer offers, one back takes back; **Offer**, **Take back**, **Set coin**, **Accept**, **Cancel**; the 3 s lock and the `changed` word exactly as PARTY.md 6 |
| tavern, people | rows, as PARTY.md 8 (portraits in rows are Phase 15's) | skinned |
| menu, settings, login, characters, new character | skinned panels and widgets | as CLIENT.md 4 |

The tallest panel is now the trade's: `ui::PANEL_HIGH` is 360 units (300 before), so a
window of 720 lines holds scale 2 exactly and one of 600 falls to scale 1 (CLIENT.md 3).
An empty equip slot says what goes in it whole (`weapon`, `armour`), in small print when
the word is wider than the slot.

A grid cell is found by `ui.find` by the words its row had, so `click "sword  slash
+2.0%"` still picks it, and the equip panel's slots are found as `weapon: sword` and
`armour: nothing`.

## 5. Bodies drawn into a screen

The paperdoll is the game's own character renderer drawing into a rectangle of the
screen: after the world's pass and the HUD's layer 0, **a second render pass** with the
colour attachment loaded and **the depth cleared** draws the own body (its model or
mannequin, its armour tint, what it holds, idle, lit flat) with `set_viewport` and
`set_scissor` to the panel's rect and a camera of its own (`Renderer::render_with_doll`:
eye 82 units in front at chest height, looking back; the body turned by a drag across
the well); the viewport and the scissor are **set back to the whole frame** before the
HUD's layers 1–4 close the frame. The pass exists only in frames that have a paperdoll,
so the game's frame cost does not move. The doll's draws ride in the same block buffer as
the world's (`Characters::prepare_with_dolls`), after them.
The selector of CLIENT.md 4.2 draws the same body **in the open** (`Ui::paperdoll_open`,
`Paperdoll.open`): no well, the map turning behind the screens showing through, and the
camera further back (`DOLL_OPEN_DISTANCE` = 46 u before the body's origin, at 34 u up, a
degree down) so the whole body stands in the rectangle; its props are the hub's pack's
abilities looked up in the manifest (`ability_prop`), since no zone has sent a prop list.

## 6. Props in hands

### 6.1 What is held

A body holds **one prop**, chosen by the zone and sent as an index (4): the `model` of the
weapon it wears (ITEMS.md 2) if it wears one, else the `prop` of its primary ability
(CONTENT.md 3), else nothing. Companions and creatures hold their primary's prop (a hired
avatar wears no gear yet: ITEMS.md 9). A stall keeper holds nothing in v1.

### 6.2 On the wire (protocol v8, hub v1.9)

- The pack (`FromZone::Content`) carries **`props: Vec<String>`**: the keys of every
  `ability.prop` and `template.model` in the content, in order of first appearance. A zone
  reads `items.toml` for these names (it reads the abilities already); it never reads a
  model. The list and the indices into it are **of that session**: a client holds the pack
  the zone sent and every `Look` it gets from that zone indexes that pack (a zone restarts
  to change content, and its clients reconnect and get the new pack).
- `PlayerEntry` (the `Players` message) gains **`look: Look { held: u16, worn: u16 }`**:
  indices into `props` (`NONE = u16::MAX`; an index past the list is read as `NONE`);
  `worn` is always `NONE` in Phase 14 and is the armour overlay of Phase 15.
  **`FromZone::Look { id, look }`**, appended at the end of the enum (ITEMS.md's lesson),
  when a body's look changes.
- `GearReading` (hub → zone) gains **`templates: [String; 2]`**, the **keys** of the
  templates worn by place (empty when nothing is worn): a hub and a zone on different
  content versions then disagree about nothing but what to draw; the zone maps a key it
  knows to its `model` key, then to the prop index, and one it does not know to an empty
  hand. One more reading when wear changes is one `Look` to everybody who has the body
  (nothing on the snapshot).
- Companions and creatures: their `Look` is computed by the zone from their build.

### 6.3 Drawing

A prop `.gmm` (CONTENT.md 4) has every vertex on bone 0 with weight 255. It is drawn by the
character pipeline unchanged: one more `CharacterDraw` with a uniform block of the usual
layout (24 matrices, scale, tint, light: MODELS.md 9) in which matrix 0 is **the wearer's
skinning matrix of `prop_r` times the translation to that bone's pivot times the grip**
(MODELS.md 2: `prop_r` follows `hand_r`; `gm_model::pose::prop_attach`) and the other
twenty-three are unused, the prop's own position
scale, the material `tint` of the item's core (CONTENT.md 3; white for a bare prop) and the
wearer's light. A custom avatar that carries a `prop_r` bone places the grip where its hand is; one
that does not gets the frame's pivot. The prop is in the model cache like any model, keyed by
the manifest's hash for its file; a prop not yet loaded is not drawn (never a stand-in: an
empty hand is correct for a moment).

**The grip** (`gm_model::pose::grip_right`): a prop is held **in the fist, across the
forearm**, not as the arm's continuation. Its business end (+X of hand space) points ahead
of a hanging arm and is tipped 12° towards the elbow, so a sword at rest is level with its
tip a little raised; its edge follows the knuckles. (v1 laid the blade on along the arm:
a sword pointing at the ground like a walking stick. The director, playing it: "it should
be perpendicular to the arm, like held in hand": 11.4.) What is not swung is laid
otherwise by its row's `fit` (CONTENT.md 3.1), a turn about glTF's Y: the **crossbow** and
the **musket** lie along the arm (carried muzzle down, levelled when the arm is raised),
the **staff** stands 20° nearer upright than a blade.

The animation set gains one thing: in the **cast** stance the arms are raised from where
they hang (a turn about the shoulders' axis) instead of swept round from the T-pose, so
that what a fist holds across the forearm stands up (a staff raised) and what lies along
it points where the arm does (a crossbow levelled); the arms look the same as before. The
swing, the windup and the rest move the arm as they did and the prop follows the hand:
the windup carries a blade back over the shoulder and the swing brings it across. (The
crossbow's bolt already leaves `spawn.weapon` = 16 u ahead of the hand, VOCABULARY.md; the
musket's the same.) A long gun in one hand is a compromise until a two-handed hold exists
(its muzzle is near the ground at rest and a little high when levelled).
`gm-tools content look KEY --out x.png` draws the mannequin holding a prop in nine stances
from the front and from its right: the fitting room on paper (CONTENT.md 9).

### 6.4 The view model (first person)

Counter-Strike's root (the director, 2026-10-03): in the FPS viewport the own body is not
drawn, and the held prop is drawn as a **view model**: the same `.gmm`, placed in view
space (`Avatars::view_model`: 26 units out, 11 to the right, 10 under the eye (14, 7 and 6 until it was seen to fill half the view in a swing, 13), its business
end along the look, turned 8° inward and tipped 4° up), with a bob read from the own
travel (a figure of eight a stride of 64 units long) and a kick on a launch or a swing
(the own predicted actions: back 3 units and up 14°, decaying over a sixth of a second).
It is drawn in the world pass with the world's depth: a weapon can clip into a wall one is
pressed against (the near plane is 4 units); a pass of its own is written down for Phase
15 with `fit_view` per template, which the manifest already carries. Third person shows
the prop in the hand as everybody sees it.

**The view fit** (2026-10-07, 13.10): a prop laid along the arm by its `fit` (the
crossbow, the musket: 6.3) lay across the view, its muzzle to the left of the look. A
template's `fit_view` (CONTENT.md 3.1) is a second fit in the same terms, applied to the
built prop in the view only (`gm_model::pose::view_fit`: the same matrix in model space,
after the attach): the crossbow and the musket are turned back by their 100° and 88°, the
pistol by its 88° and brought 0.3 m nearer the eye, a short thing. A prop no template
fits is drawn as built.

**The hand in the gun mode** (MODES.md 3.7): the view model is the weapon switched to,
`1 2 3`, read from the own mover and the manifest (the ability in hand, its `prop`) the
frame the key is pressed; the zone tells everyone the same hand with a `Look` when it
changes (6.2: the musket, the pistol, the knife's dagger), so a stranger's body holds
what the gunner switched to.

**The reload** (MODES.md 3.2): while the firearm in hand is being reloaded (the own
predicted mover's reload, 0 to 1 over its `reload_ms`), the view model is brought down
4.5 units, 3 nearer and 3 to the left, tipped 24° and rolled 40° over towards the off
hand, worked at it with a bob of nine cycles a reload, eased in and out over the first and
last 22% of it. The stranger sees the `RELOAD` stance (13.3). Offline, `--prop KEY` in
the first person is the fitting room of the view model: the prop in the view with the
stride's bob; `R` held works a reload over and over.

### 6.5 The off hand (2026-10-09)

The director: the colossus wants "an oversized sword ... and also shield, so it looks a bit
more appropriate" (MATRIX.md 16). A body holds a second prop in its **left hand**: the
`prop` of its build's **guard** ability (CONTENT.md 3.1; `shield_wall` holds `shield`), else
nothing; a worn item does not reach the off hand yet (a shield item is a template with
`held = "left"` for a later phase). On the wire it is **`Look.off`** (protocol v17, section
29 there), chosen by the zone with `held` and sent in the same `Look`; a gun build's is
`NONE`. It is drawn as `held` is (6.3), by the wearer's skinning matrix of **`prop_l`** at
that bone's pivot times **the left grip** (`gm_model::pose::grip_left`, `prop_attach_left`):
the right grip mirrored through the body's plane, which is not a rotation, so it is the
right grip turned half round about the business end instead; a shield's face (+Z of its
model, the back of the fist's side) then stands out of the back of the hand, and the
guard stance, which already raises the left forearm across the chest, raises the shield in
front. The fitting room draws it: `gm-tools content look shield --left`.

## 7. The purse: silver and gold

The director dropped copper (2026-10-03). The ledger's integer is **silver**; **100 silver =
1 gold**; every number in the database, the wire and the tests keeps its value (what was
30 copper is 30 silver). No migration divides anything: there is no live economy, the only
databases are test ones and the director's play stack, which are wiped (`start --fresh`);
a database from before Phase 14 is not carried over, and HUB.md says so. ECONOMY.md 2 and
12 are rewritten to the director's scale (2026-10-03: "top gear tens of gold; fully crafted
with top stones, near a hundred"): a meal 1 s, standard gear 20–50 s, a boss component
1–5 g, a top item 10–30 g, one fully crafted with a boss shard and top gems 60–100 g, a
12 h hire 50 s – 2 g, a carry 2–10 g; the cap on a single coin grant 5 g. ("Infuse and
imbue with stones" is, in this economy, the deterministic crafting of ECONOMY.md 4 and the
two gem sockets: no casino, PLAN.md 0.) The screens show two coins (`coin_gold`,
`coin_silver`) with numbers, gold in threes; the price fields are two. `--grant-coin`,
`--sell-at`, `--trade-for`, `--list-for-hire` take silver. PLAN.md 0 and 5.1 are corrected.

## 8. Budgets and acceptance (PLAN.md 11.8 Phase 14)

`budgets.toml` `[content]` and `[look]`:

| Number | Proposed | Why |
|---|---|---|
| `max_bundle_bytes` | 2 MiB | the whole of `assets/built/content/`: what a browser may have to fetch over a session |
| `max_atlas_bytes` | 524,288 | one atlas: a client fetches `ui.gma` before the first screen and the one of its scale after (measured 82,847 / 207,092 / 332,624 / 470,976 at one to four texels a dot with the generic character set of 2.3; 393,216 for 49,641 / 123,997 / 200,633 / 282,595 with ASCII and Gaj's letters; 262,144 while there was one atlas) |
| `max_prop_gmm_bytes` | 131,072 | CONTENT.md 4 |
| `max_webgpu_wasm_bytes` (WEB.md 9) | 2,097,152 | raised from 1 MiB by the director (9); the phase reports what it added |
| `max_native_added_bytes` | 262,144 | the native client |
| `max_page_ms` | 0.8 | inventory with the paperdoll, a frame on the iGPU (today 0.36–0.47 ms for a page) |
| `max_hud_ms` | 0.15 | the hotbar, statuses, portrait and party frames, a frame on the iGPU |
| `max_prop_draw_ms` | 0.5 | 100 bodies holding props in the town over the same scene without (the avatars gate's scene) |
| `min_fps` | 60 | the avatars gate with every body armed |

`scripts/check-look.sh [--desktop] [--browser] [--gate-fps]`: (1) `gm-tools content check
--built`: the tables and their looks, and the bundle rebuilt and compared with the
committed one byte for byte; the bundle's, the atlas's and every prop's bytes against
`[content]`; (2) the unit tests of the format, the ingestion, the baked icons, the looks,
the grids and the scripts; (3) the offline town with 48 synthetic avatars, all holding the
sword, against the same crowd bare: the prop draw cost (`--gate-fps`: on the real GPU, with
the fps budget); (4) `--desktop`: own Xvfb, own hub and town, a walker with a hammer build;
a character made by script, handed a sword and a cuirass; by UI script it opens the
inventory (a grid of pictures: `expect image item/sword`), **drags the sword onto the weapon
slot** and the zone answers `worn`, hovers for a tooltip; its last `--report` line says the
hotbar's first cell is the sword, that two bodies hold something and two props loaded; a
screenshot at every step is kept with `KEEP`; (5) `--browser`: the same by the WebGPU build
in headless Chromium, with the wasm's bytes against WEB.md 9's cap.

## 9. Proposed numbers and open decisions

Proposed: 36-dot cells, 32-dot icons, 24-dot status icons, 150 ms to a tooltip, four dots to
a drag, the paperdoll's camera, four party frames, sixteenths of a turn for the wedge, the
faces and their sizes (2.3), the grip's 12° and the fits of the staff, the crossbow and
the musket (6.3), two pixels a dot from 600 to 1300 lines (CLIENT.md 3), every budget in 8.

**Decided by the director, 2026-10-03:**

1. **The browser's megabyte may be doubled or tripled** "as long as it runs smoothly on a
   100 Mbps connection". The WebGPU cap becomes **2 MiB** (`max_webgpu_wasm_bytes`
   2,097,152; packed 786,432): at 100 Mbps two raw megabytes are 0.17 s and the packed
   file under 0.07 s, and the WebGL2 build, which is already 3 MiB, loads today in 142–292
   ms to the first frame on loopback (WEB.md 9). The byte hunt is no longer the gate of the
   phase; the phase still reports what it added. WEB.md 9 and `budgets.toml` carry the new
   number with this reason.
2. **Drag and drop is a must, and keyboard shortcuts stay a must**: every drag has a
   button and a key (principle 3), as proposed.
3. **The purse's scale** (7): top gear is tens of gold, and fully crafted with top
   components it nears a hundred; ECONOMY.md 12's scale is rewritten to that.

Open still (small; the implementation picks and says so): where the weapon rests out of a
fight (v1 holds it always); which two faces (Fira Sans Medium and MedievalSharp, both OFL,
named in LICENSES.md: v1's pixel face was part of what read as low resolution, 11.4);
the party frames' reach (v1: members present in the zone). (The marks on a body, flat
boxes since Phases 3 and 6 that the director had to ask about, were decided the same
evening: "nametags would work", 13.4, and a ring for the aspects.)

## 10. Deliberately absent

Tooltips on the HUD's own cells (the hotbar, the party frames): the HUD has no pointer in
the game; a key that shows the hotbar's words is Phase 15's. Armour drawn on the body
(Phase 16: avatars per frame and class, CONTENT.md 3.2); minimaps
(PLAN.md 0: none, ever); a quest log, an experience bar (no levels); (names over bodies
were absent in v1, with a cube for the side: the director asked what the cube was and
took names instead, 13.4); a cursor theme in the browser
(the page's cursor is the browser's outside pointer lock); animations for holding a prop
(the shared set moves the hand; a real set is later work); dropping an item on the ground
(ECONOMY.md 3 has `ground`, no screen yet); a crafting screen (its grid is this toolkit's,
the phase is later).

## 11. Review log

### 11.1 Design review (Gemini 3.1 Pro, 2026-10-03, over CONTENT.md and this document, v0)

Fourteen findings in all (CONTENT.md 12.1 has the seven on that document). On this one:

| # | Finding | Verdict |
|---|---|---|
| 2 | Reading the stored copper as silver multiplies every balance by 100; migrate by dividing | **Rejected.** No live economy exists; test and play databases are wiped. Written down in 7 so nobody carries a pre-14 database forward |
| 3 | A block of one matrix does not fit a pipeline that binds 24 | **Accepted** (wording): the block is uploaded whole with the hand's matrix at index 0 (6.3, CONTENT.md 3.1) |
| 6 | An RGBA8 pipeline with an R8 fallback atlas fails validation or draws blocks | **Accepted** (wording): the fallback is made as RGBA8 at start (2.2) |
| 7 | A props list "by first appearance" shifts indices when a model changes; old packs draw the wrong weapon | **Rejected.** The pack and the `Look` indices are of one zone session; a zone restarts to change content and its clients get the new pack (6.2). The alternative (template and ability indices on the wire, the client resolves) was weighed and costs the client the worn-or-bare rule for nothing |
| 8 | HUD layers after the paperdoll inherit its viewport and scissor | **Accepted** (wording): set back to the whole frame (5) |
| 12 | A 1024² RGBA8 atlas is 4 MiB and the `.gmm` reader's inflation bound is 2 MiB | **Accepted.** The atlas's reader bounds inflation by `w × h × 4` (2.2) |
| 13 | Grids scroll but there is no scrollbar: no wheel, no reach | **Accepted.** `scroll_rail`/`scroll_knob` on grids and lists (2.2, 2.4) |

### 11.2 Code review (Gemini 3.1 Pro, 2026-10-03, over the phase's diff with this document)

Twelve findings over the two halves of the diff (CONTENT.md 12.3 has the five on the
servers and the pipeline); on the client:

| # | Finding | Verdict |
|---|---|---|
| 1 | `Some(a)` moves the mutable reference before `a.avatars.doll` | **Rejected.** It compiles and runs (a reborrow); the gate's desktop run draws the doll |
| 2 | `drop_slot` starts no drag: a worn thing cannot be dragged off the equip panel | **Accepted.** A press on what the slot holds starts a drag out of it (2.4) |
| 3 | A cuirass dropped on the weapon slot asks the zone to wear it there | **Accepted.** A drop counts only for the slot's place; the zone would have refused it anyway |
| 4 | The browser waits for the manifest and the atlas before the first frame, against "never wait for art" | **Rejected.** They are fetched beside the map, which the page already waits for (61 KB against a 400 KB map); a site without a bundle (404) starts at once with the built-in font. The principle is about a missing or late *model*, which is still drawn as nothing |
| 5 | No tooltips on the hotbar's cells and the party frames (2.4) | **Accepted as a gap**, written down in 10: the HUD is not a `Ui` screen (no pointer in the game) and a tooltip there needs the pointer's own rules; Phase 15 |
| 6 | The seconds left are not drawn on a cooling cell (3.2) | **Accepted.** Drawn when a second or more is left |
| 7 | The HUD's portrait is not recorded as a `Seen` image | **Rejected.** The HUD is not a screen and records nothing; scripts read the hotbar from `--report` (3.2) |

### 11.3 Found by running it

- **The HUD's ink drew over a screen's plates.** The first HUD used the layers as the screens
  do (plates 0, ink 2); a screen's panel on layer 0 is drawn after the HUD's plates but
  before the HUD's ink, so the bars' numbers and the hotbar's keys floated over the
  inventory. The game's HUD now stays on layer 0 in call order (2.1); the layers are the
  screens', for the paperdoll between their plates and their ink.
- **A view model pushed too early.** The online frame decides what is drawn, and the
  avatars' frame begins after it, clearing the draws: the first view model vanished and
  something else was seen in its place. The decision is kept (`ViewModel`) and pushed once
  the frame has begun.
- **Props never arrived in the browser.** The fetches landed in an inbox that nothing
  polled: `Content::poll` runs every frame now, one prop a frame, as the avatars' loader
  does (the desktop reads a prop the moment it is first asked for).
- **A grid picked listings by the wrong id.** A stall's cells took the item's id from the
  thing they were made of; the pick is the listing's (ITEMS.md 6). Nothing could be bought
  until it was.
- **An empty list dropped the pick.** The grid wrote `NONE` back while the hub had not yet
  answered, and the first answer's first thing was no longer picked as a list's first row
  always was; the pick is left alone while the list is unknown or empty.
- A name as long as a name gets, with the dearest coin beside it, did not fit the trade's
  header at the smallest window: the name is shortened on purpose (`Abcdefghijkl.. gives`)
  rather than cut, which the tidiness test refuses.
- The trade window is the tallest panel now: `PANEL_HIGH` 360.

### 11.4 Found by the director playing it (2026-10-03, the evening of the commit)

"The game is barely playable but the new UI is ultra low res, did you take screenshots?
What are those colours below characters and boxes above them? Having the sword as an
extension of the arm is wrong, it should be perpendicular to the arm, like held in hand."

- **The screenshots were taken at 1280 × 720 only, and judged against the document**
  (which asked for dots, magnified). At 1920 × 1080 the scale was 3: a 12-dot pixel face
  36 pixels high in blocks of nine pixels, 32-dot icons as 96-pixel mosaics, the skin's
  grain as squares, the inventory 1,020 × 940 pixels of a 1,080-line frame. Looking at the
  frame the director sees, and not at the one the gate uses, is the lesson. What changed:
  an atlas per density drawn at that density (2.2), outline faces (2.3), the skin drawn
  per density without grain, **two pixels a dot from 600 to 1,300 lines** (CLIENT.md 3: the
  inventory at 1080 lines is 680 × 620), the HUD's own words in the text face, names
  shown whole (the hotbar's, the equip slots'). The gate now fails unless the atlas in
  use has the density of the UI's scale (`ui_scale` and `atlas_density` in `--report`).
- **The sword was the arm's continuation** because 6.3 said "the blade on along the arm":
  written without picturing a hand. The grip is a fist's now, the fits lay the crossbow,
  the musket and the staff, the cast stance raises the arms from the hang (6.3), and
  `gm-tools content look` shows any prop in every stance, which is how the grip was
  chosen this time.
- **The marks on a body were not understood**: the plate under the feet is the body's
  aspects (MODELS.md 9: an avatar may look like anything, so its element must be read at
  a glance), the cube over the head its side. Nothing was changed; what they should be is
  the director's call (9).
- **Gates that had been failing since the commit**, found by running all of them again:
  two of this gate's test counts asked for more tests than the filters match (`-p gm-model
  prop`: one, not three; `-p gm-ingest --test ingest prop`: three, not five); the screens'
  gate looked for the font's tests in the client (they moved to `gm_model::smallfont` with
  the font); the party gate's desktop character was given 50 silver for a hire that costs
  150 (the purse was cut with the copper, the price was not). All three count and pay
  what is there now, and the tool's three new tests are counted too. The phase's report
  said the gates passed; these parts of them had not been run again after their last edit.

### 11.5 Code review of 11.4's change (Gemini 3.1 Pro, 2026-10-03, over its diff)

| # | Finding | Verdict |
|---|---|---|
| 1 | In the browser the fetched atlas lands in a one-place inbox: two fetches that land between two frames (the scale changed twice while the page was hidden) and the later one asked for is lost, never asked for again | **Accepted.** The inbox is a list and every arrival is read; the one of the density asked for last is kept |
| 2 | A glyph's cell is written with its sides as bytes: over 255 texels at density 4 it is truncated | **Rejected.** `validate` refuses such a glyph on both sides and the tool refuses to make one ("a smaller face") |
| 3 | The grip's matrix may be a mirror | **Rejected.** Its determinant is +1; a test says so now |
| 4, 5 | The width and the pen lose the fractions of the quarter-dot advances | **Rejected.** Both are sums in quarters converted to pixels at the end; only the glyph's quad is put on a whole pixel |
| 6 | The atlas reader slices without checking its position | **Rejected.** `Reader::bytes` checks the end with `checked_add` against the buffer and returns an error |

## 12. What was measured (2026-10-03, the reference machine)

- Pages, uncapped, the Radeon iGPU, by `--report` while a UI script holds each (whole
  frames, the town behind): the game with the skinned HUD, portrait and hotbar **0.42 ms**
  (0.36–0.37 before the phase), the inventory with its grid, tooltip and paperdoll pass
  **0.54 ms**, the storage 0.53, the menu 0.61; `[look].max_page_ms` 0.8 and `max_hud_ms`
  0.15 hold.
- Props: 48 synthetic avatars in the town, all armed, **+0.006 ms** a frame over the same
  crowd bare on the iGPU (1.336 → 1.342 ms, 745 fps), +0.20 ms on the software GPU
  (`[look].max_prop_draw_ms` 0.5).
- The gate's desktop run: the inventory open, the sword dragged and worn, the tooltip and
  the report in 12–14 s of a software-GPU client; five screenshots in `KEEP`.
- Sizes: WebGPU wasm **1,104,519 bytes (374,635 packed)**, +95,054 over Phase 13 (WEB.md 9:
  the cap is 2 MiB now); WebGL2 3,094,152 (946,533); the native client **9,747,456 bytes**, +135,856 (`ci/baselines/gm-client-size`
  updated); the bundle 114,614 bytes.
- Tests: 380 in the workspace (367 before); the client's 86 cover the grids, the drag, the
  tooltip, the scale rule and the scripts' new verbs.
- After 11.4 (the same evening): the four atlases **49,641 / 123,997 / 200,633 / 282,595
  bytes** (512 × 180, 512 × 632, 512 × 1,364, 1,024 × 1,220), the bundle **713,302 bytes**
  in 11 files; WebGPU wasm **1,112,187 bytes (376,460 packed)**, +7,668; WebGL2 3,103,848
  (948,678), +9,696; the native client **9,757,712 bytes**, +10,256 (baseline updated).
  The gate's desktop run at 1280 × 720 draws at two pixels a dot with the atlas of two
  texels a dot; the browser's (a 600-line page) at one with one. By eye at 1920 × 1080 on
  the software GPU: the game, the inventory with a tooltip, at scale 2 and (chosen) 3.
  Not measured again: the pages' frame cost on the iGPU (the quads are the same number;
  the texture is larger; no display was logged in). The prop cost on the software GPU
  read anything from −0.13 to 1.45 ms in single pairs of runs with the play stack and
  other sessions loading the machine (a difference of two 37 ms frames); over seven
  pairs the middle was **0.30 ms** for the sword (0.20 in the phase: a blade across the
  arm covers a few more pixels than one along it), and the gate now takes the middle of
  five pairs instead of one (with three it still read 0.53 once, the play stack's bots
  fighting beside it).

## 13. Seeing the fight (2026-10-03, after the director played in the browser)

"Animations and effects are poor and non-existent: using a skill just moves the arm a bit
and the damage area is guesswork. If we have a sword and swing it, either the sword or the
blast arc should do the damage, and both should be observable by the player to be able to
aim." And of the marks on a body: "nametags would work."

What a blow hits was always exact in the zone (a wedge of a reach and an arc at the
instant the windup ends, VOCABULARY.md 5.1) and never drawn. This section draws it.

### 13.1 Principles

1. **What is drawn is what does the damage.** The wedge on the floor and the slash in the
   air are made from the same numbers `melee_hit_point` tests with: the reach and the arc
   of the ability the body is acting, at the place and the facing it had. Nothing is
   decided here: the zone alone says who was hit.
2. **Everybody's blows, not only the own.** A snapshot says which ability a stance belongs
   to (protocol v9, PROTOCOL.md 21: one byte with a script's stance), and every client has
   the pack, so an enemy's windup shows where it will land as exactly as the own.
3. **The own body acts at once.** Its stance in a swing or a cast, the wedge and the slash
   come from its own prediction (`script_anim`, the rule the zone uses), not from the
   zone's word of it a round trip later; a bolt flies from the hand the frame it is let go.
4. **Effects are triangles, made every frame.** Coloured, unlit, see-through, in the
   world; one draw call after the bodies, blended, tested against the depth and not
   written to it (`fx.rs`, the entities' shader). No textures, no particles kept.

### 13.2 What is drawn (`fx::Effects`)

| When | What |
|---|---|
| a body winds up a swing | **the wedge on the floor** at its feet: outline, a faint fill, and a brighter fill running out to the reach as the windup runs out |
| the swing lands (the stance turns to `swing`) | **the slash**: the wedge lit, and at chest height a band standing at the reach and a flat ribbon inside it, sweeping from the right edge of the arc to the left in the active time (never under 0.10 s: a sword's 45 ms would be three frames), bright at its leading edge, gone 0.24 s after |
| a bolt in flight | a streak along its way, never longer than it has flown (13.8), and a bright head in a halo five times its radius (seen end-on by whoever shot it); the own bolts in their damage's colour (the aspects' colours; steel for a blow), from the hand at once for 0.16 s, then the zone's |
| an area | a disc of its radius on the floor with a rim and a ring running outward, orange if it hurts and green if it helps; **a burst** when it appears: a wall rising on its edge and a flash, 0.35 s |
| a body's health drops (where the frame knows it: the own, the party's, the squad's, a creature's) | a spark at its chest, the body lit for 0.14 s; the own: the frame's edge red for a third of a second, and the number lost (13.8) |
| the zone says the own hand landed | the number dealt, in gold over the body hit (13.8) |
| the zone says the own hand's Regen gave another body health back | the number, in green over the body healed (13.8) |
| the zone says the own hand landed, since v13 | the number and a spark **where the blow landed** (13.11), not over the head |
| a bullet meets the world | a dark mark on the surface for twenty seconds (13.11, MODES.md 10.2) |
| always | **the ring of its aspects** at a body's feet (one colour, or a half each) instead of v1's square plate |

Colours: the own swing warm white, a friend's blue, anybody else's orange-red.

### 13.3 Stances and the view model

- **Every stance has its own time to be reached** (`gm_model::anim::fade_secs`): 70 ms
  into a windup, 50 into a swing, 160 out into the recovery, 120 elsewhere. At the single
  120 ms of before, a sword's 90 ms windup and 45 ms swing never reached their poses: that
  was "moves the arm a bit". The swing's pose is straighter and the body turns further
  into it.
- **The own stance is predicted**: while the own script runs, `script_anim` of it; when
  it is over here and not yet there, what the feet do; dead, staggered, guarding,
  dashing and commanding stay the zone's word.
- **The view model is carried across the view** by the own stance (drawn back to the
  right in the windup, cut across to the left, let go), and stands twice as far from the
  eye as before (26 units): at 14 it filled half the frame in a swing.

### 13.4 Names over bodies

A name over every body in sight within 900 units whose head a ray from the camera reaches
(no names through walls), in small print with a shade: blue for the own side (team, squad,
party), orange for a creature, red for another team, white otherwise; under it a thin bar
of its health where the frame knows the health and its whole (the squad's, a creature's).
The cube over the head is gone.

### 13.5 Proposed numbers, and what is not done

Proposed: every time and size in 13.2 and 13.3; 900 units for a name; the colours.

Not done, and worth saying: **no ability has a picture or a sound of its own yet** (the
effects are by verb: a swing, a bolt, an area); a cast shows nothing before its bolt or
its area (a glow at the hands, a mark where an area will fall); a hit on a stranger
whose health the wire does not carry shows nothing (PROTOCOL.md 5 sends health for the
own party and creatures only: the open question of SOUND.md 9, bytes on the wire); a
replay draws no wedges; the stances are still six poses blended, not animation (Phase 16
is the body's look); nothing was measured on the real GPU (13.6).

### 13.6 Measured, and found by running it

- The acting ability costs **one byte with a script's stance**: a delta that starts a
  swing is at most 5 bytes over the same delta without it (the test of the codec).
- Sizes: WebGPU wasm **1,136,246 bytes (383,589 packed)**, +24,059; WebGL2 3,128,065
  (956,743); the native client **9,798,240 bytes**, +40,528 (baseline updated). **393
  tests** in the workspace (385 before). The gates of the look, the screens, the items,
  the party and the sound pass with their desktop clients, the look's and the web's with
  the browser; the published page was opened in headless Chromium at 1920 × 1080 on the
  restarted play stack (60 fps, WebGPU, a fight by script).
- Not measured: what the effects cost a frame on the iGPU (no display was logged in; they
  are one draw call of a few hundred triangles).
- By eye, on the software GPU at 1920 × 1080 and 1280 × 720, on a private stack: the
  wedge, the slash and the names in third person; the swing across the view in first
  person; a firebolt's head at its end. A bolt in flight was not caught by the captures
  (a tenth of a second apart); the unit tests hold its geometry.
- **A horizontal ribbon is nearly invisible from behind**: the first slash was a flat
  sector at chest height, edge-on to a camera behind the shoulder. The band standing at
  the reach is what reads, from every side.
- **A bolt seen end-on is a dot**: the first streak was a few pixels to its own shooter.
  The head got its halo and the tracer from the hand.
- The view model at 14 units filled the lower half of the frame once it moved.

### 13.7 Code review (Gemini 3.1 Pro, 2026-10-03, over the change's diff and `fx.rs`)

| # | Finding | Verdict |
|---|---|---|
| 1 | A projectile's `def` is looked up in the own kit, and should be looked up in the pack | **Rejected.** A projectile's `def` is the index into its owner's kit (PROTOCOL.md 5; the zone writes the kit slot), unlike `acting`, which is the pack's; the lookup is made only for the own bolts, whose kit the client has |
| 2 | The hit flash's tint is vector arithmetic that does not compile | **Rejected.** The tint is three floats; it compiles and the frame shows it |

No design review was asked for before the code this time (the director was waiting on a
playable build): the design is this section, written with the code.

### 13.8 Numbers over bodies, and where a bolt begins (2026-10-06, the director's second look)

"Let's make damage dealt appear over characters, something like it does in Tales of
Pirates. Also RMB animations start from the camera or behind the character (in third
person view) and not from the character."

**The numbers.** What a blow did floats up from over the body's head in the title face
and fades: 1.1 s, quick at first and slowing (the ease of `1 - (1 - t)²` over 44 scale
units of screen), gone over the last 0.35 s, a shade behind for any wall.

| Which | Where it is known | Colour, size |
|---|---|---|
| what the own hand dealt | the zone's word of it: `FromZone::Hit { target, amount, absorbed }` to the attacker (PROTOCOL.md 22) | gold, the largest (1.4 × the UI scale) |
| what the own body lost | the own health, which every snapshot carries | red `-n`, the scale |
| what the own body got back | the own health going up from a living body (a respawn is not a healing) | green `+n`, the scale |
| what the own hand gave another body back (a Regen's pulse) | the zone's word of it: `FromZone::Healed { target, amount }` to the healer (PROTOCOL.md 22) | green `+n`, the largest |
| a blow of the own hand all taken by a block | `amount` 0 with `absorbed` over 0 | grey `blocked`, small |

A blow by somebody else on somebody else shows its spark and nothing more: what is dealt
is told only to the one who dealt it, and the snapshot's health of a party member or a
creature is not sent for the others, so a number from it would show for some bodies and
not for others. The message goes to the attacker because the health of a stranger (an
enemy team, a sparring bot) is not on the wire at all (13.5): without it, no number would
ever show over the body one is fighting. A healing is told the same way and for the same
reason: a mender's dart lands on a teammate who is in no party, whose health the wire does
not carry; the number is one a pulse (four a second) for the Regen's time, and a Regen the
own hand put on the own body (a sanctuary stood in) is not told, the own health says it. The own hurts come from the own health and not
from a message, so that what is lost to a burn or a fall shows the same as a blow.

In the first person the own head is not in the frame: the own numbers rise from under the
aim instead (100 scale units under the dot, up 40). In the third person the own numbers
start at the head itself; another's, over its name.

**Where a bolt begins.** A bolt's streak was drawn 96 units long behind its head from the
first frame, whatever it had flown: a bolt just let go from the hand was a line running
back through the shooter, in the third person to the camera behind the shoulder, in the
first from the eye. It was the streak, not the spawn: the zone spawned at the weapon's
offset all along. Now the streak is never longer than the bolt has flown since it was
first seen (`Effects::bolts`, by the entity's id; the own tracer from where it was let
go), so it starts as a head at the hand and grows a tail. The own tracer also leaves from
where the zone will spawn the bolt (`Origin::Weapon`'s offset in the body's yaw frame, as
`resolve_origin`), not from a point in front of the eye.

Seen on the software GPU at 1920 × 1080 on a private stack (`look.sh` in the session's
scratchpad: Xvfb, the fight script, two sparring bots): `50` and `67` in gold over
the bodies hit, `-26` in red over the own head and under the aim in the first person,
and in the same frames another's crossbow bolt trailing its streak from in front of the
shooter. The own ice shard's launch was not caught by a capture (a sixth of a second);
the unit test holds that its streak begins at the hand.

### 13.9 A body runs the way it goes (2026-10-06, the same evening)

"Check if A/S/D can rotate the model so it appears to run to a side in the non-FPS view,
since otherwise it looks like walking forward but moving sideways."

The body was drawn facing where it looked in every stance, so a side-step was a body
running ahead and sliding across. There are no side-step poses (Phase 16 is the body's
look), and none are needed for this: **a body running or in the air is drawn facing its
way of travel** (`app::facing`), turned toward it at 720°/s rather than snapped, and in
every other stance — winding up, swinging, casting, guarding, standing — facing where it
looks, which is where its blow lands. Backing off keeps the eyes on whom it backs off
from: the travel turns the body only within 100° of the look; further back is a
backpedal. The rule is the same for every body: the own from its prediction's velocity,
the others from the velocity the two snapshots around the render time give. The facing is
the drawing's alone — the wedge, the slash, the aim and the ring of aspects stay on the
look, as before.

Seen on the software GPU at 1920 × 1080 (`strafe.sh` in the session's scratchpad: a key
held with `xdotool`): A in profile to the left, D to the right, W away, S a backpedal,
idle on the look.

The RPG mode is the exception (MODES.md 5.1, 10.3; 2026-10-08): its camera orbits a body
that does not turn with it, so an RPG body (snapshot flag `RPG`, v15) is drawn standing as
it was left, facing its last travel or the target it last turned to, and runs facing its
travel whichever way, S included; it turns to its look only for an action, where the zone
fires.

### 13.10 The gun in the hand (2026-10-07, the director played the gun mode)

"FPS mode, for example when I take musket, musket is oriented wrong and gun has no model
at all, both need reload animation of some kind. Knife also needs model, maybe then it
will be and feel a bit more like CS 1.6."

Three faults, one cause each:

- **The musket lay across the view.** The view model put the prop's +X along the look
  (6.4), which is where a blade's tip is after its fit; the musket's fit lays it along the
  arm for the third person (6.3), so its muzzle was +Y, to the left. The `fit_view` the
  manifest had carried since Phase 14 is now applied (6.4): the crossbow and the musket
  turned back, the pistol nearer.
- **The pistol and the knife showed the musket.** A body's `Look` was the worn weapon's
  model, else the primary's prop, and nothing changed it when the gun mode's hand changed
  (MODES.md 3.7: the mover's `held`). The zone now reads the hand (`look_of`: in the gun
  mode the ability in hand, whatever is worn) and says a `Look` when it changes, once a
  switch (`crates/gm-server/tests/items.rs`: the watcher hears the pistol, the knife, the
  musket again, once each); the own view model reads the hand from the predicted mover
  the same frame. The pistol has a model of its own (`gm-tools content synth pistol`:
  a flintlock, 17 KB of glb, 1.0 KB of gmm); the knife is the dagger the content already
  had. The carbine is still drawn as the musket.
- **No reload to see.** The view model is lowered, rolled over and worked while the own
  reload runs (6.4); the stranger had the `RELOAD` stance already.

Seen on the software GPU at 1920 × 1080, offline and on a private play stack with a
musketeer driven by `xdotool` (the session's scratchpad `vm/`): the musket along the look
with its lock under the eye, the pistol on `2`, the dagger on `3`, "reloading" with the
pistol dropped to the frame's corner and the musket dipped and rolled; the captures are in
`~/.local/share/gamengine/play/shots/2026-10-07-gun/`. Sizes and counts: WebGPU wasm
1,237,363 (+9,801 over 15c's 1,227,562; the cap 2 MiB), the bundle 7 props and 23 baked
icons (the pistol's among them), `content check` reproduces it; the two tests of
`items.rs` pass against the test database, the workspace's unit tests as before.

### 13.11 Where the blow landed, and the bullet's mark (2026-10-07, the director played the gun)

The director: "our hit animation highlight is misleading since it's always centering no
matter where we hit", and "black marks on hit, bullet marks are a must". Since protocol
v13 the zone says **where** a blow landed (`FromZone::Hit.at`: the point of the hull a bolt
or a blade met, the body's centre for an area or a pulse), and the gold number of 13.8 and
a spark are drawn there, not over the head: a headshot shows at the head, a shot in the
leg at the leg. The own hurts stay where they were (the frame's red edge, the number from
the own health). A **bullet's mark**: a bolt at 10,000 u/s or more that meets the world
leaves a dark disc of 3.5 units on the surface, 0.6 off it, for twenty seconds, the last
four fading; 160 at most. Nothing marks a body yet: blood is the body's look (Phase 17).
The effects table of 13 gains two rows by this.


### 13.12 The font's character set (2026-10-08, the co-owner played in the browser)

The inventory read `kit ?3` and `kit ?3 of 5`. The game writes `kit ×3 of 5` (the item
rows of `gm-content`, the bag), but the faces were rasterised for printable ASCII and
Gaj's ten letters only, so the `×` was not in the atlas and the HUD drew its `?`. The
decision: the set is generic, not Croatian (2.3): ASCII, the Latin-1 Supplement's letters
and signs, Latin Extended-A whole, and the typographic marks, 308 characters in each face;
a character the font lacks is left out rather than baked as its box; and a test of the
tool holds both faces to that and to the `×`, the dashes, the quotes and an accented
letter of another tongue (`the_faces_hold_the_signs_the_game_writes_and_nothing_the_font_lacks`).
The atlases grew with the glyphs, 82,847 / 207,092 / 332,624 / 470,976 bytes at one to
four texels a dot, and `max_atlas_bytes` is 512 KiB (8). The five-by-seven face
(CLIENT.md 3) is unchanged: it is what is drawn before the bundle loads, and keeps its `?`.
