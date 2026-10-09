# Items in play

Status: v1 (Phase 11), as built. This document is the contract for what a character
**wears**, what worn gear **does** in the simulation, how a person **sees** what a character
owns, and how a person **buys** at a stall. ECONOMY.md is the contract for what items, coin,
holders and stalls *are*. PLAN.md 0 (gear is an edge, never a tier), 3.4 (gear is unbound and
un-classed), 5.3 (what each layer of a craft is for), 5.5 (stalls stand in the world) and 6
(nothing a scammer can dress up) are binding. When the code and this document disagree, the
document wins. Section 10 records the reviews, section 11 what was measured.

Until Phase 11 an item was a row at the hub that nothing read: no character wore one, the
simulation knew of none, and no screen showed one.

## 1. Principles

1. **An edge, typed.** Gear moves damage by a little, and each part of an item moves its own
   kinds of damage and no other: an item is right for one build and of no use to another,
   which is what makes it worth selling to "someone with the opposite problem" (PLAN.md 3.4).
2. **One edge for a character, not one for each thing it wears.** PLAN.md 0 gives top gear
   15–25% over standard gear. A character wears two things, so each counts for half (3.1).
3. **Three owners of three things.** What is worn is the hub's (a row beside the item). What
   it does is the zone's (sixteen small numbers per body, read by the damage pipeline). What
   a person sees is the hub's word, numbers and words both: a client works nothing out.
4. **The client predicts none of it.** Gear changes damage, and damage is the zone's alone:
   no number a client predicts with (speed, stamina, timings) depends on gear in v1, so
   nothing about prediction or its wire changes.
5. **What happens to a body is asked of the zone the body is in.** Wearing and buying go
   client → zone → hub, as opening a stall does (ECONOMY.md 7): only the zone knows where
   the body stands and whether it is in a fight, and what the hub answers the zone applies
   at once. Nothing about gear travels on a notice that could be late or lost, and what
   the hub says of a character's gear is numbered, so that no order of arrival can leave a
   zone with an old answer (3.3).
6. **Worn is not for sale.** A worn item cannot be offered, listed, stored, dropped or
   decomposed until it is taken off. (Whether a death drops what is worn is PLAN.md 5.4's
   open lever, and stays open.)

## 2. What is worn

A character has two places: **weapon** and **armour**. A template's `kind` says which one an
item goes into (`items.toml`: `weapon` or `armour`; a component is neither and cannot be
worn). One item in each, or none. "Worn" in this document is about items; the avatar model a
character wears is MODELS.md's.

- **Wear** an item: it must be in the character's own inventory and of a kind that is worn.
  It takes its place; whatever was there is taken off. Wearing what is worn already changes
  nothing. An item on offer in a trade leaves the offer (both accepts are cleared and the
  offer's version moves on, as when an offered item is destroyed).
- **Take off** an item: its place is empty again. Taking off what is not worn changes nothing
  and is no error; an item whose template the content no longer knows comes off like any
  other.
- A worn item **stays in the inventory** and takes its slot there.
- Gear is **unbound and un-classed** (PLAN.md 3.4): any character may wear any armour, and
  what an item is good for is decided by what it is made of (3.2), not by who holds it.
  **A weapon is worn only by hands that hold one** (the director, 2026-10-08: "my musket
  can equip a staff? make it say it can't and disable it"): an item of a weapon template
  is worn only when an ability of the character's build has the template's `model` as
  its `prop` (CONTENT.md 3; `gm_hub::hub::hands`, `economy::fits`). The musketeer's hands
  are for the musket, the pistol and the dagger; the blade's for the sword; a caster's for
  the staff. A weapon nobody in the build swings would sharpen a kind of blow the body
  never deals and put the wrong prop in its hand (LOOK.md 6.1 prefers the worn weapon's
  model), so the hub refuses it in words, "this build's hands are for the musket, the
  pistol and the dagger: not a staff", and says the same of the item wherever it is shown
  (`ItemSummary.fits` false, the words last in `does`: the inventory, the storage, a stall's
  listing, a trade), so the bag offers no Wear for it and takes no drop of it on the weapon
  slot. Armour, parts and stacks fit everyone. The rule is the build's, not the item's: the
  item stays unbound, sells to whoever holds one, and the same sword fits every build whose
  abilities hold a sword. A respec that leaves a worn weapon behind keeps it on until it is taken off (an
  open point; the trainer could take it off at the `Respawned`).
- A companion (a hired avatar, a recruit) fights in **nothing** in v1 (9).

Stored as `worn (character_id, slot, item_id)`, one row per filled place, the item unique
(migration 0009). What holds the rule that a worn item stays with its wearer:

| What | How |
|---|---|
| destroyed (decompose) | refused in words; and the worn row refers to the item, so the delete would fail |
| moved (storage, stall, ground, guild chest, a trade's commit) | `move_item`, the one mover, refuses in words; and a trigger on `items.holder_id` refuses whatever else would move it (`GM001`, said as the same words) |
| offered in a trade | refused; the offer takes the holder's lock first, so offering and wearing cannot both go through. Wearing takes an item out of every **open** offer (what finished trades wrote down of it stays written) |
| the audit | `Economy::audit` counts worn items that are not in their wearer's own inventory, are listed, or are on an open offer; ECONOMY.md's storm puts things on while it offers and trades them, and ends on the audit |

Lock order, everywhere: the character's row (shared), holders (ascending), trade rows, the
item, the worn row.

## 3. What gear does

### 3.1 The term

MATRIX.md 7 ends `damage = max(1, round(base × type × layer × guard × gear))`. This is
`gear`:

```
gear = (2000 + A.dealt[t]) / (2000 + D.taken[t])
```

`t` is the packet's damage type (Slash, Pierce, Blunt, Fire, Water, Grass, Electric, Ground, Air; nine since MATRIX.md v3);
`dealt` is the edge of the attacker's worn weapon and `taken` the edge of the defender's worn
armour on that type, per mille, each at most 250 (an item's whole edge is capped at 250 by
`gm-content`). **A place counts for half of what its item's edge says** (principle 2): an
edge of 220 is 11% more dealt, or 9.9% less taken (2000/2220).

- `gear` lies between 1/1.125 and 1.125 for one packet. What PLAN.md 0 bounds is the
  exchange: the best weapon *and* the best armour the content allows, against a body in
  nothing, win a like-for-like exchange by **23.2%** (1.11²); against iron and oak (an edge
  of 40 on each side, "standard gear") by **18.4%**; at the very cap (250 and 250 against
  nothing) by 26.6%.
- The factor multiplies **before the rounding**: its bounds are on the unrounded number, and
  a result of 1 can become 2.
- The attacker's `dealt` is taken **when the packet is made**, with the rest of the
  attacker's side (a swing in its windup, a projectile in flight and an area keep the edge
  they were made with); the defender's `taken` when it lands.
- **The pulses of Bleed and Burn take no gear on either side.** A pulse is a point or two,
  rounded alone: a factor on it is a step of a third or of nothing, not an edge (design
  review 5). The blow that set the status is moved; the status is the same for everybody.
- What does not go through the pipeline is not moved: healing, a zero packet (MATRIX.md 7),
  knockback, stagger build-up, stamina costs, a block's stamina cost.
- A body with nothing worn has all sixteen at 0, and `gear` is 1: every result measured
  before Phase 11 stands.

### 3.2 From materials to the sixteen numbers

An item's edge is the sum of its materials' edges (ECONOMY.md 4). Each **layer** puts its
material's edge into the kinds of damage the item is for, and into no other:

| Layer | In a weapon (`dealt`) | In an armour (`taken`) |
|---|---|---|
| core | the physical kind the template **strikes** with | what the template **guards** against: `physical` (Slash, Pierce, Blunt) or `elements` (the six) |
| catalyst | its element | its element |
| shard, frame, gem | every kind the core and the catalyst reach | the same |

| Template | Kind | | Layers |
|---|---|---|---|
| `sword`, `dagger` | weapon | strikes slash | shard, core, catalyst, frame, gem |
| `hammer`, `staff` | weapon | strikes blunt | shard, core, catalyst, frame, gem |
| `crossbow` | weapon | strikes pierce | shard, core, frame, gem |
| `cuirass` | armour | guards physical | shard, core, frame, gem |
| `robe` | armour | guards elements | shard, core, catalyst, frame, gem |

(`dagger` is new in Phase 11: the dagger ability had no weapon.) Nothing in the rule names
an ability or a build. A hammer does nothing for a sword's blows; a staff with an ember
catalyst sharpens the staff's own blow (blunt) and every fire the wearer casts; a cuirass
turns blows and a robe turns the elements, and choosing between them is a choice.

The best the content allows (boss scale 55, dragonbone 60, a catalyst 30, whalebone 45, two
opals 60):

| Item | Whole edge | Does |
|---|---|---|
| sword, ember catalyst | 250 | slash +11.0%, fire +9.5% |
| crossbow | 220 | pierce +11.0% |
| cuirass | 220 | physical −9.9% |
| robe, rime catalyst | 250 | water −11.1%, other elements −9.9% |
| sword of iron and oak ("standard") | 40 | slash +2.0% |

A craft, and an operator's grant, take only parts of layers the template has room for ("a
cuirass takes no catalyst"); `craft` checked only the counts before. A part an older item
has of a layer its template has no room for counts for nothing.

The hub says an item in words as well as in numbers (`ItemSummary.what`, `.does`:
`a weapon, 250 of 250`; `slash +11.0%`, `fire +9.5%`), composed by `gm-content` from the
same numbers the zone is given, with the scale of 3.1: a weapon's edge `e` is said as
`e / 20` per cent more, an armour's as `100 × e / (2000 + e)` per cent less, to a tenth.
Kinds of one group that share an edge are said together (`physical`, `elements`, or `other
elements` beside one that stands out). The `N of 250` is the item's whole edge: its budget,
whatever it is called and however it is drawn (PLAN.md 6).

**Deviations to confirm** (9): PLAN.md 5.3 gives the frame "speed, stamina, crit", the shard
"a passive trait" and gems "counter-meta modifiers"; in v1 all three are plain edge on the
item's own kinds. PLAN.md 3.4 has gear "balanced by weight/cast/stamina penalties"; v1 gear
has no penalty at all. Both because speed, stamina and cast times are numbers a client
predicts with (principle 4). The notes in `items.toml` say what the materials are meant to
become, not what they do.

### 3.3 Where the numbers travel

- `Gear { dealt: [u16; 8], taken: [u16; 8] }` (`gm_core::matrix`), indexed in the order of
  `DamageType`. The zone clamps each to 250 whatever it is told.
- **At the claim**: the hub reads what the character wears *after* it made the character
  that zone's, and `Claimed` carries the reading.
- **While the character plays**: its gear changes only through its zone (5). The hub's
  transaction holds the character's row against a change of where it is, checks that it
  plays in the asking zone (not offline, not on its way anywhere), makes the change and
  commits; then it reads the gear and answers with that reading, which the zone applies
  **at once**. (A build change waits for the respawn because the client predicts with it;
  gear it does not.) A claim by another zone either comes after and reads the change, or
  comes first and the change is refused.
- **Readings are numbered.** A reading is `GearReading { seq, gear }`: the number is drawn
  from one sequence at the hub, *before* the worn rows are read, and every statement sees
  what was committed before it began. So the reading a change makes of itself (after its
  commit) has a larger number than any reading that could have missed it, and a zone that
  keeps, for each character, the reading with the largest number holds what the hub holds,
  in whatever order the answers reach it. This matters where two readings race at one
  zone: a character that joins the zone it is in already (its claim's reading) while its
  last body's change is still on its way (that answer). The zone applies an answer to the
  body the *character* has now, not to the body that asked; a new body takes the larger of
  its claim's reading and what the zone already has for the character; and an answer that
  finds no body is kept for two minutes for the body a join is about to make.
- **In transit** (a ghost waiting for the other zone's claim) nothing changes: the zone
  refuses a ghost and the hub refuses a character that is not in a zone. A ghost that
  returns to play fights in what it wore, and the zone saves it at once, so that the hub
  has it in the zone again and its requests are taken (it used to wait for the next
  periodic save, up to half a minute).
- A session cannot change what its character wears: there is no such request. A zone
  without a hub has no gear, and never reads `items.toml`.
- A zone that lost its hub can change nothing (the request fails): its bodies fight on in
  what they wore. The hub has taken its characters offline (HUB.md 5), and nothing that
  zone does reaches the books any more.

## 4. The hub (v1.7; v11 adds stacks, MODES.md 11.7; v12 the item bar, LOOK.md 3.2)

`HUB_VERSION` is 7 and `PLAYER_VERSION` 2 (11 and 4 since 2026-10-07: `items.quantity`,
`GearReading.stacks`, `ZoneEconOp::Consume`, `HubNotice::Gear`, `ItemSummary.quantity`
and `cap`; `StallBuy` answers `Gear`; `PLAYER_VERSION` 5 since 2026-10-08:
`ItemSummary.fits`, section 2; `PLAYER_VERSION` 6 since 2026-10-09: `ItemSummary.edge` is
nine wide, MATRIX.md 14; **12 and 7** the same day: the item bar, below). The hub loads
the content's looks beside the pack
(`HubConfig.looks`) for the props the abilities hold. The requests of Phase 11:

| Request | From | Answer | |
|---|---|---|---|
| `EconOp::Inventory`, `Storage` | a session | `Holder { coin, items }` | an item now says its `place`, its `edge` per type, whether it is `worn`, and itself in words (`what`, `does`) |
| `EconOp::Bar` | a session | `Bar(cells)` | the item bar (LOOK.md 3.2): four cells, a stack template or nothing each, as the character arranged it; never arranged, the first stack it carries that heals on the first cell |
| `EconOp::SetBar { cells }` | a session | `Done` | arranges it: four cells, each a stack template of the content or nothing (gear is refused in words); kept whether or not the stacks are carried; the character's zone is told with its gear (`HubNotice::Gear`: `GearReading.bar`) |
| `EconOp::StallView { stall }` | a session, from anywhere | `Listings { owner, mine, listings }` | each listing: its id, the item, the price |
| `EconOp::StallList { item, price }` | a session | `Id` | into the caller's own stall, **which stands in the zone the character plays in** |
| `EconOp::StallUnlist { listing }` | a session | `Done` | out of it again, under the same rule; the inventory must have room |
| `ZoneEconOp::StallBuy { character, stall, listing, price }` | a zone | `Done` | 5 |
| `ZoneEconOp::Wear { character, item }`, `TakeOff { character, item }` | a zone | `Gear(GearReading { seq, gear })` | 2, 3.3; a weapon the build's hands do not hold is `Invalid` with the words of 2 |

- **`EconOp::StallBuy` is gone**, as `EconOp::StallOpen` went in Phase 6: a session could
  buy from anywhere, and a program with no body at any stall could buy every underpriced
  listing in the world. The hub's own checks for a zone's `StallBuy`: the character plays
  in the asking zone; the stall stands in that zone; the listing is that stall's; the
  price is the listing's; the buyer is not the keeper; the coin is there; the inventory has
  room. Coin and item move in one transaction (ECONOMY.md 7).
- A stall is kept where it stands: listing and unlisting need its keeper to be playing in
  the stall's zone. (Otherwise a stall is twelve slots reached from the other end of the
  world. The account's storage is reached from anywhere, as ECONOMY.md 3 left it: 9.)
- The players' messages (HUB.md 3.8) gain `PlayerRequest::Econ { session, character, op }`
  with `PlayerEcon`, the requests that have a screen: `Inventory`, `Storage`,
  `StorageDeposit`, `StorageWithdraw`, `StallView`, `StallList`, `StallUnlist`; answered
  `PlayerResponse::Econ(PlayerEconReply)`: `Done`, `Id`, `Holder`, `Listings`.
- **A limit on the economy's requests**: every `Econ` request of a session is a transaction
  and some read a row per item, and a session is all it takes to ask. One account may make
  **5 a second, with 20 in hand** (`gm-hub --econ-per-second N`); past it the answer is
  `Busy`. Zones' requests are not counted: a zone gates its own players (5).
- An operator's hand, with the hub's own program as for a moderator, on a database that
  may be in use:
  - `gm-hub --grant-coin CHARACTER SILVER` and `--grant-item CHARACTER TEMPLATE
    MATERIAL,...`: out of the source like every drop, under the reason `grant` in the
    ledger and the item log; what is made obeys the craft's rule and the template's room,
    and a full inventory refuses (nothing goes to the ground).
  - `--place CHARACTER ZONE X,Y,Z YAW`: where an **offline** character stands when it next
    enters (its saved position); refused for one that plays.
  - `--audit`: one line: coin created, in the world and destroyed, how many balances
    disagree with the ledger or worn items are astray (0: sound; -1: the coin created is
    not the coin there and destroyed; the exit status says whether it is sound), how many
    items are worn, and the coin that moved under each reason.
- A craft is refused before the hub looks at anything when it names more parts than an
  item has places for (six): each part costs statements, and a frame holds thirty thousand
  ids.
- Migration 0009: `worn`, the trigger of 2, and the sequence the readings are numbered
  from.

## 5. The zone and the wire (protocol v6)

| Message | | Answer |
|---|---|---|
| `FromClient::StallBuy { stall, listing, price }` | client → zone | `BuyResult { listing, result }`, always |
| `FromClient::Wear { item }`, `TakeOff { item }` | client → zone | `WearResult { item, result }`, always |

- **Buying.** The zone checks that the buyer is alive and in person (not a ghost), that the
  stall is one of its open stalls, and that the buyer stands at it: its feet within
  **120 units** of the middle of the stall's tile along the ground and within 96 above or
  below (`gm_net::control::stall_in_reach`: a tile is 128 wide, a body in front of the
  counter is about 80 from its middle, the next stall's middle is 160 away). Then it asks
  the hub (4).
- **Wearing.** The zone checks that the body is alive and in person, and **not in a
  fight**: it has neither dealt nor taken damage for **10 s** (`GEAR_AFTER_FIGHT`; a pulse
  of a status counts for both). Then it asks the hub, applies the answer at once, and logs
  it (`gear character=… dealt=[…] taken=[…]`).
- **Gates** (PROTOCOL.md 8): one request at a time and one a second per player, for the
  stall requests (open, close, buy) and, apart from them, for wearing. A request the gate
  stops is **answered** ("one thing at a time: try again in a moment"): somebody is looking
  at a screen. (A stopped `StallOpen` or `StallClose` is dropped, as before.) The second
  is what a request to the hub costs: one the zone refuses by itself (not at the counter,
  in a fight) costs none, and the next may follow at once.
- **A flood** of these messages never reaches the tick loop: a connection hands on four a
  second with eight in hand and drops the rest unread.
- **Patience**: the zone waits ten seconds for the hub. Then the player is told ("the hub
  did not answer: try again") and may ask again; what the hub answers to a change of gear
  in the end is still applied when it comes.
- An answer names what it is about (the listing, the item): a screen takes only the answer
  it is waiting for.
- The five messages are appended (to the one enum, `Control`, that both directions were
  until protocol v7; PARTY.md 4): the ones before them keep their numbers, so
  a client and a zone of different versions can still read each other's `Hello`, `Reject`
  and `Kick` (the first build of this phase put them in the middle, and a v5 client could
  not read why a v6 zone turned it away).
- A refusal is in words for the person:

| What the zone or the hub found | Words |
|---|---|
| the gate | one thing at a time: try again in a moment |
| dead, or a ghost | you cannot buy now / not now |
| not an open stall of this zone | that stall has closed |
| not standing at it | walk up to the stall to buy |
| the listing is gone, or is another stall's | it is no longer for sale here |
| the price is not the listing's | the price changed |
| the keeper's own | that is your own stall |
| the purse | not enough coin |
| no room | the inventory is full |
| in a fight | not in a fight: wait a moment |
| not the wearer's item | that is not in the inventory |
| a part, or what the content does not know | that cannot be worn |
| no such item | there is no such item |
| the hub does not have the character in this zone (a ghost just back, a handoff under way) | you are between zones: try again in a moment |
| the hub is past its limit | the hub is busy: try again |
| the hub did not answer in ten seconds | the hub did not answer: try again |
| anything else the hub said, or its connection | the hub could not be asked: try again (the reason goes to the zone's log) |
| a zone without a hub | this zone has no market / nothing is worn in this zone |

  (A listing's price never changes today: the price check is for a client that shows a
  wrong one.)
- Nothing else is new on the wire: a client learns what it owns from the hub.

## 6. Screens (CLIENT.md's toolkit)

Four pages (`bag.rs`), named `inventory`, `storage`, `price` and `stall` for UI scripts.
The body stands while one is up, as with the menu. Since Phase 14 (LOOK.md 4) the lists
are grids of pictures with tooltips and drag, and the inventory has an equip panel with a
paperdoll; the rules below hold as they did, and a cell is found by a script by the words
its row had. Three rules hold on all of them:

- A list is asked for when its page opens and after something the person did, and at no
  other time: what is on the screen does not move under the pointer.
- What is picked is picked by what it is (an item's id, a listing's id), not by its row.
  When that thing is gone, **nothing is picked**, and no button acts until the person picks
  again: a keeper who takes a sword back and puts junk in its row does not have the junk
  bought by a second click. A row is acted on by its buttons, never by Enter or a double
  click.
- Nothing is said that the hub did not say. A list that has not been answered is not an
  empty list ("nothing is for sale here" is said when the hub said so). A list that could
  not be asked for again after a change is called old, and no button acts on it. A screen
  takes only the zone's answer it is waiting for; any other is said where the game says
  things, and moves nothing.

Under the lists are two lines for what is being waited for or was last said. A price in a
row is whole when it fits and otherwise its largest units and a `+` (`1,234 g 56 s+`); the
line under the list has all of it, and that is the price a purchase is made at.

### 6.1 Inventory

`I` in the game, and **Inventory** in the menu. The purse (ECONOMY.md 2: `2 g 15 s 40 c`,
gold in threes, a colour for each unit, never a row of digits) and the items, one row each:
what it is called, the strongest thing it does, and `worn`. Under the list the picked item
in full: what it is and its budget (`a weapon, 250 of 250`), everything it does, what is
worn in its place now (`you wear: slash +2.0%`, or that nothing is), and what it is made
of. Buttons: **Wear** / **Take off** (asked of the zone; one a second), **Sell** (when the
character keeps a stall in this zone and the item is not worn), **Store** (into the
account's storage), **Storage**, **Close**.

Under the grid, **the item bar** (LOOK.md 3.2): the four cells as drop slots named `bar
F`, `bar 8`, `bar 9`, `bar 0`, each with the stack set on it (dim, `×0`, while none is
carried) and its key beneath. A stack dragged from the grid onto a cell sets it (gear
sets nothing; a kind sits on one cell, so it leaves the cell it was on), a cell dragged
onto another swaps the two, a cell dragged into the grid empties it; each is `SetBar` to the hub (section 4), said as `the bar is set`, and the HUD's
cells follow at once.

### 6.2 Storage

The account's storage, one row an item, and the picked one in full as above. **Take** brings
it into the inventory. It is where what a closed stall could not hand back is (ECONOMY.md 7),
and where a full inventory is emptied into.

### 6.3 Price

**Sell** asks for a price in two fields (gold, silver: digits only, since Phase 14 dropped copper; an amount
pasted with anything else in it is not taken: `12.50` is not 1250) and says the whole back
in words (`for 1 g 20 s`) before **List it** sends it, for the item it was opened for and
no other.

### 6.4 A stall

Standing at a stall (in reach by the zone's own rule, 5), the corner of the screen says
whose it is and `E look`; of several in reach it is the nearest. `E` opens it (not in the
tactical view, which has its own use for the keys around it): the keeper's name, the
buyer's purse, and what is for sale, one row each (what it is called, the strongest thing
it does, its price). The picked one in full as in 6.1, beside what the buyer wears, and its
price. **Buy** asks the zone for that listing at the price shown; the answer is said on the
screen in the words of 5, and the stall and the purse are asked for again. At one's own
stall the button is **Take back**.

### 6.5 What is not a screen yet

Crafting and decomposing, dropping and picking up, buy orders, the town board (a search
over every stall), the tavern, a trade between two players, guild chests: the hub has every
one of them as a request (ECONOMY.md) and none has a screen. Trade and the tavern come with
parties of people (Phase 12).

## 7. Budgets and acceptance (PLAN.md 11.8 Phase 11)

`budgets.toml`: `[items]` (the time from the stall to the sword being worn); the browser
builds stay inside WEB.md's budgets; the desktop client inside its cap.

`scripts/check-items.sh`:

1. **The term** (`gm-core`): a packet of every type between geared and ungeared bodies comes
   out moved by the factor of 3.1, to the point; the bounds and the three exchange numbers;
   a real swing in nothing, with a sword, against a cuirass, with both; an edge on another
   type; the clamp; a bolt whose weapon is taken off in flight; a burn with the best of
   everything and with nothing; gear through a death, a respawn and a change of build.
2. **The content** (`gm-content`): each layer into its own kinds; the words; the cap; a
   weapon strikes, an armour guards.
3. **The screens** (`gm-client`): the four pages against a scripted hub and zone, and every
   page whole at five window sizes with the longest rows there are.
4. **The hub** (a database): wearing and taking off through the zone only; the numbers and
   the words; a worn item refused to the storage, a trade, a stall, the ground and
   decomposition, and by the database itself; what cannot be worn and whose; an item
   offered and then worn; transit; the claim; the players' messages; the limit; the
   audit, and that it is not blind. The storm (six characters, 1,176 operations at once)
   with wearing, offering and decomposing in it.
5. **A zone** (a hub, the town, three clients driven by hand): a buy from across the square
   refused, at the counter every refusal of 5 and then the purchase; a swing in nothing,
   the fight lock, the sword worn and a swing, the cuirass worn and a swing, the buyer gone
   and back and a swing, the sword off and a swing.
6. **By somebody who is not a person** (`--desktop`; `--browser` for both browser builds): a
   bot keeps a stall in the town and lists what an operator hands it; a new account and a
   character are made through the screens; the operator gives it coin and stands it at the
   counter; by UI script it looks at the stall, buys a sword at the price shown, finds its
   purse lighter, wears the sword, and finds it worn through the menu as well; the hub's
   audit and the zone's log say the same. On the desktop then `E`, Escape and `I` from a
   real keyboard.
7. Every earlier gate green.

What the proposal in PLAN.md 11.8 had and this phase does not: the **tavern** and **trade**
screens (Phase 12, with parties of people); and the buyer's coin is an operator's grant,
not earned in play. "The zone's hits show the edge" is step 5, by hand-driven clients, not
by clicking: a UI script presses the keys of screens, not of a fight.

## 8. Deliberately absent, and known gaps

- Speed, stamina, weight, traits, resistances as distinct effects (3.2); durability and
  repair (ECONOMY.md 1: no chore sinks); item levels, bindings, class restrictions.
- The armour **class** from worn gear (MATRIX.md 4 planned it for Phase 5): the class stays
  a choice of the build, paid for in its budget (9).
- Gear on companions; a look for worn items (a body is drawn as before); seeing what
  somebody else wears; more places than two.
- **A replay does not say what its bodies wore**: a moderator sees hits a tenth larger than
  the builds explain. (Gear cannot change in a fight, so one line in the roster would do.)
- **The striker trial** asks for a share of the damage, and a weapon makes that easier
  while companions fight in nothing.
- **Buy orders are still filled from anywhere** (`EconOp::BuyOrderFill`): they have no
  screen, and get their place when they get one.
- A zone does not come back from a hub that restarted (HUB.md 5), and its notice reader
  stops at a notice it cannot read: old weaknesses that gear no longer leans on.
- **The fight lock looks back only.** A body that sees a bolt coming can still put a robe
  on before it lands (the hub answers in milliseconds), and a blow that was parried or
  dodged marks nobody. A lock that begins when a blow is *made* is a decision (9).
- **An answer the zone never gets** (its connection to the hub reset between the hub's
  commit and its answer) leaves the body in what it wore while the hub holds the change;
  asking again mends it, and so does the next claim.
- A stall is bought from through a wall, if the body is within reach of it.
- **A stack never splits** (MODES.md 11.1): one drag moves it whole, and a stack that
  would pass the cap of what it lands on is refused whole.

## 9. Proposed numbers and open decisions

Proposed, for the director: two places; **a place counts for half** (the scale of 2000);
the layer table of 3.2, what each weapon strikes with and each armour guards against; that
pulses take no gear; 120 units of reach; the fight lock of 10 s; one change a second; five
economy requests a second per account; that worn items are locked out of every movement.

Open:

- **What the 25% is.** As built a character's whole edge is 23% over nothing and 18% over
  iron and oak. If instead each *item* may be a full 25% (the scale 1000), the same two
  items win an exchange by 49% over nothing and 38% over iron and oak.
- **Whether frame, shard and gem stay plain edge** or become what PLAN.md 5.3 names them;
  and whether gear ever has the penalties of PLAN.md 3.4 (both touch what a client
  predicts).
- **A caster's edge is smaller than a fighter's**: an element is reached by the catalyst
  (30) where a blow is reached by the core (60): 190 against 220 at best.
- **Whether the armour class moves from the build to the armour.** It would refund the 0–12
  points the class costs today, and every stored build (which spends exactly 100) would
  have to be rebuilt.
- **Whether a hired avatar wears its owner's gear.**
- **When a fight begins** for the lock: at the first damage, as built, or when a blow is
  made (and for both sides of a parried one).
- **Where the storage is reached from** (anywhere, today) and whether a death drops gear.

## 10. Review log

**Design review, 2026-10-02** (an independent agent; the Gemini account still answers HTTP
402). It read the draft against PLAN.md, ECONOMY.md, MATRIX.md, HUB.md and the code. All
thirteen findings were accepted; they changed the design before most of it was written.

| # | Finding | What was done |
|---|---|---|
| 1 | A ghost could take its weapon off unheard, return to play and keep the edge while the sword was sold | Gear changes only through the zone a character plays in; the hub checks that inside the transaction; a ghost is refused (3.3) |
| 2 | Gear rode on notices, which are advisory, unordered and unacknowledged | No notice carries gear. (The first build had numbered readings and an early-notice store; it was removed for the simpler rule) |
| 3 | "The database refuses it too" was true only of deletes; a trade's commit moves items with its own statement | The trigger, the detaching at Wear, the offer's lock, the audit (2) |
| 4 | 25% a packet is 49% for a character in two items | A place counts for half (3.1); the alternative is an open decision with its numbers |
| 5 | On a pulse of 1 or 2 points the factor is a step of 0 or 33%, and a pulse read the wearer's gear at each pulse | Pulses take no gear on either side |
| 6 | Wear and take off were unthrottled, needed no body and applied at once: a load on the zone, a helper program swapping per ability | Through the zone: one a second, never in a fight; and a limit on every account's economy requests |
| 7 | "Buying is not among them" must mean `EconOp::StallBuy` is deleted | Deleted; the hub's checks listed (4) |
| 8 | A buy stopped by the gate was dropped unanswered; one untagged answer for open, close and buy; the hub's words passed through raw | `BuyResult { listing }`, always answered; the table of words (5) |
| 9 | Dead ends: what a closed stall sends to the storage vanished from every screen; no unlisting; a price typed as a row of digits; listing from anywhere | The storage page; Take back; three fields and the price said back; listing where the stall stands |
| 10 | Rows too long; the pick was an index (a click could land on another row's listing after a refresh); the client had to compute; reach as large as the pitch; `E` in the tactical view | A row and a detail pane; picks pinned by id and lists refreshed only after the person's own action; the hub's words; 120 units, the nearest stall, not in the tactical view; beside what is worn, with the budget |
| 11 | 160 of 250 was plain edge on everything; the robe was the cuirass and more; a craft ignored the template's layers | Shard, frame and gem only into the item's own kinds; a robe guards the elements; the craft and the grant obey the template |
| 12 | Deviations the draft did not admit | 3.2, 7 and 9 say them |
| 13 | The acceptance passed with these broken, and its step by script could not be written (no game keys, no way to stand a buyer at a stall) | Steps 1 to 6; `key E` and `key I`; `gm-hub --place`; a bot that keeps a stall |

Smaller points taken: `dagger` named as new; the reason `grant`; "worn" told apart from
MODELS.md's; the replay gap and the trial written down (8); `TakeOff` by item and without
error. The one-hit threshold the review feared (by its numbers an ice shard of 86 becoming
103 against a body of 95) does not appear at the halved scale: 86 × 1.095 is 94.

**Code review, 2026-10-02** (three independent agents, each on a part: the hub and the
economy; the simulation, the zone and the wire; the client, the bot and the gate. Gemini
still answered HTTP 402). Two findings were High, one for the zone and one for the client;
the hub's reviewer found none. 35 findings in all; 32 were acted on, 3 are written down as
open or as gaps (the fight lock that looks back only, the audit's -1, an answer the zone
never gets).

*The zone and the simulation (1 High, 3 Medium, 7 Low):*

| Finding | What was done |
|---|---|
| **High.** A gear answer was applied by body, and dropped when the character had joined again meanwhile: a character whose ghost returned could join its own zone a second time and take its sword off in the same moment; the new body kept the claim's older reading while the hub held the sword as free | Readings are numbered at the hub and made after the commit (3.3); the zone applies an answer to the body the character has now, a new body takes the larger of its claim's reading and what the zone has, an answer without a body is kept for the next one. A hub test races a claim against a change 32 times and asks that the largest number is what the hub holds |
| The test that pulses take no gear could not fail (the content's burn is 3 a pulse whatever is done to it) | A burn of 40 a pulse: 62 with and without gear; with the rule broken on purpose the test reads 69 and fails |
| The zone test depended on wall-clock time in three places | It asks again until the zone stops saying "in a fight", as a person does; the lock is 3 s; two requests at once are sent at once |
| A ghost that returned was "in transit" at the hub for up to half a minute, and refused everything | Saved at once |
| Refusals reached the player raw ("unauthorized", "unexpected hub answer …") | Words for each (5); the hub's reason goes to the zone's log |
| No patience on the hub: a held request held the player's gate | Ten seconds, then told; a late answer to a change of gear is still applied |
| A flood of these messages was handled one by one in the tick loop | Dropped in the connection's own task past four a second |
| The new messages were put in the middle of `Control`: a v5 client could not read a v6 zone's `Reject` | Appended; the handshake's bytes are pinned by a test against the v5 build's |
| Answers could not be matched to requests; a refusal of the zone's own spent the player's second | `WearResult` names its item; the client takes only the answer it waits for; the gate's `cancel` |
| Promises no test guarded (a swing's edge taken at its windup, an area, a riposte, the lock's edges, what marks a fight) | Tests for each (7 step 1); the reach rule at its edges |
| *Seen in passing, older than this phase:* a status pulsed every 16 ticks whatever the zone's rate, so in a 20 Hz zone (the town) Burn, Bleed and Regen did a third of what they say; a body replaced by its character joining again was never said to have left | Four pulses a second at any rate; `PlayerLeft` for the replaced body |
| The fight lock looks back only (a robe put on while the bolt is in the air) | Not changed: written down (8) and open (9) |

*The client, the bot and the gate (1 High, 6 Medium, 12 Low):*

| Finding | What was done |
|---|---|
| **High.** When the picked thing was gone, the pick fell to whatever stood in its row and the button armed on it: a refused Buy followed by a second click bought another listing; a price typed for one item could be sent for another | Nothing is picked when the picked thing is gone (6); the price page belongs to the item it was opened for. Breaking the rule on purpose fails four tests |
| The zone's answers were not matched to requests | Matched by listing and by item; any other answer is said in the game's own lines and moves no list |
| "The session has ended" was said for a thing that had merely moved (from another client) | "It is not there any more", and the lists asked again |
| A refresh that failed overwrote the word of what was done ("bought" became "the hub is busy", with the bought listing still shown and Buy armed) | The word stays; the list is called old; nothing acts on an old list |
| The screens said things the hub had not said ("nothing is stored" after a refused request, "you wear nothing" before the inventory was known) | A list is unknown until answered |
| Cells of rows were cut silently and the five-sizes test could not see it (a price drawn as `99,999,999 g 9`) | Cut cells are recorded and tested for; a price in a row is whole or says there is more |
| Tests leaned on 20 ms and 80 ms of wall clock | The screens are handed the time; the tests step it |
| Smaller: an answer with the screen closed was only in the window's title; a refused Take did not ask the storage again; answers in another order than asked; a notice cut to one line; Enter wore the first row; a page changing inside a frame; a pasted `12.50` read as 1250 | Each fixed (6) |
| The keeper bot retried a refused listing twice a second for ever; its task outlived a failed run | Backs off ten seconds; ends with its flow |
| The gate: `--place` checked by a word its refusal also contains; a grant's failure taken for the right failure; minimums below the number of tests; no look at the desktop's logs | The exit status; the refusal's own words; the real counts; every log looked at |
| The browser gates saw only what the client logged, not what the browser said (a WebGPU validation error) | The runner prints the browser's rendering errors as errors, and an uncaught exception fails a gate too |
| *Older:* two lines of the Keys page were cut at every size | Shortened; the menu's test now asks that nothing is cut |

*The hub (0 High, 1 Medium, 4 Low):*

| Finding | What was done |
|---|---|
| A craft naming thirty thousand parts cost the hub a statement or two for each before it looked at any | Refused above six, before anything |
| Wearing an item removed what finished trades had written down of it | Only open offers (2) |
| `--audit` says -1, not a count, when the supply does not add up | Written down (4) |
| An answer the zone never gets leaves the body behind the hub | Written down (8); a late answer is applied |
| No test raced a claim against a change, or granted into a full inventory | Both added |

Not acted on, with reasons: the fight lock beginning when a blow is made (a rule change for
the director: 9); a line-of-sight test for buying (8); zone-level tests of a dead buyer and
of the same-zone re-join (the hub-level race and the unit tests cover the rule; the zone's
part is a dozen lines).

**Found by running it:**

- The **WebGL build drew no town**: the map black, bodies and HUD in place. wgpu's GL
  backend cannot be told what a texture array will be viewed as and guesses from the layer
  count; the town has twelve textures and was taken for a cube array (the arena has seven).
  It had been so since Phase 8 and no gate had looked at the town in that build. The array
  now never has such a count, and the browser gates fail when the client logs an error.
- A body put down exactly on the floor is in the floor, as far as a zone that asks whether
  a saved place is still a place to stand can tell: `--place` wants a hair above.
- The first storm with wearing in it "decomposed a worn hammer": two tasks of one character
  were fighting over the weapon's place, and the sword's wearer had taken the hammer off.
  The test was wrong, the hub right.
- In a 20 Hz zone a burn did a third of what it says (the review saw the constant; a test
  at both rates now holds it).

**Review by Gemini 3.1 Pro, after the fact (2026-10-03).** The Gemini account had no credit
when this phase was written (the reviews above are independent agents'); with credit back,
Gemini 3.1 Pro read this document and the whole commit, asked for what the earlier reviews
missed. 4 findings, all Low.

1. *With the menu and the inventory both open, neither is drawn*: **rejected, unreachable**.
   While a page is up the toolkit takes every key (Escape closes the page, it does not open
   the menu), and the menu's Inventory button closes the menu; the two guards are each
   other's belt and braces.
2. *A `pasted` check in the number field is redundant*: **noted**, left (it says why the
   pasted text is dropped).
3. *The zone's per-player ask bucket adds single-precision gains: a flood of asks nanoseconds
   apart could add nothing and the bucket never refill under it*: **accepted, fixed** (double
   precision, as the hub's bucket).
4. *`grant_item` does not lock the `source` holder before writing its `item_moves` row*:
   **rejected**. Source and sink keep no balance and no transaction ever locks them (ECONOMY.md
   4); a lock order is a cycle between two lockers, and there is none here.

Its verdict on the earlier reviews: sound overall.

## 11. What was measured (2026-10-02, the reference machine)

- **The term**, in a simulated swing (35 slash on cloth, 37.19 unrounded): 37 in nothing,
  41 with the best sword (220), 34 against the best cuirass, 37 with both, 42 at the clamp.
- **A zone's hits** (a real zone, clients by hand, a sword on plate): 15 in nothing, 16
  with the best sword, 15 against the best cuirass too, 15 after the buyer left and came
  back, 13 with the cuirass alone.
- **The storm**: 1,176 operations in 0.65–0.82 s (1,432–1,815 a second), eleven runs, no
  deadlock, the audit sound.
- **By script, software GPU**: 0.5–0.6 s from `E` at the stall to the sword being worn
  (four runs of the gate).
- **A frame with a page up** (the integrated GPU, 1280×720, uncapped, in the town at a
  stall, eight items in the inventory and eight listings; three runs): the game alone
  0.39–0.45 ms, the stall's page 0.47–0.50, the inventory 0.48–0.49, the storage
  0.47–0.49. A page costs under a tenth of a millisecond.
- **Sizes**: the WebGPU build 1,005,086 bytes (335,607 packed) of its 1,048,576, 38,034
  more than Phase 10; the WebGL build 2,993,885 (908,530 packed). The desktop client
  9,547,168 bytes, 72,784 more.
- **Tests**: 329 in the workspace (310 before), the hub's test of what is worn eight
  times out of eight, the zone's by-hand test in 19.8 s.
