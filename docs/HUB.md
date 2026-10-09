# Hub: accounts, characters, zones, handoff

Status: v1.9 (Phase 14: a gear reading names the templates worn, so a zone can say what a body holds, LOOK.md 6.2; v1.8 of Phase 12: parties, lines relayed between zones, a trade opened by the zone both play in, a hire that names its price, section 3.10 and PARTY.md; Phase 11: what is worn, buying through a zone, the limit on the economy's requests and the operator's hand, section 3.9 and ITEMS.md; Phase 10: what the client's screens lean on, section 3.8; Phase 9: conduct, section 3.7; Phase 8: the web listener, section 3.6; Phase 4; the economy requests of Phase 5; models, stalls in the world and the
saved position's zone of Phase 6; squads, trials and gated zones of Phase 7, section 3.5). This document is the contract between `gm-hub`, `gm-server` and the
clients for everything that outlives a zone process: accounts, characters, where a character is,
and how it moves between zones. PLAN.md 2.1 (Postgres via sqlx, in-memory session state), 11.3
(hub-directed handoff with a ghost until the ack), 11.4 (data model) and 11.7 (one zone process
per map, one hub) are binding. When the code and this document disagree, the document wins.
Section 8 records the independent design review this version went through.

## 1. Principles

1. **The hub owns identity and persistence; zones own simulation.** A zone never touches the
   database. Everything a zone needs about a character arrives in one message when the character
   is claimed, and everything it changed goes back in one message when the character leaves.
2. **Zones verify, the hub issues.** Entry to a zone is a short-lived, single-use, zone-bound
   token signed by the hub (ed25519). A zone can verify it offline and asks the hub only to
   *claim* the character, so a stolen token is worthless after one use and never valid elsewhere.
3. **One transport.** The hub speaks the same QUIC the zones speak (`quinn` natively, WebTransport
   in the browser), with `bitcode` messages on bidirectional streams, one request per stream. The
   client binary gains no HTTP stack; the browser build needs nothing new.
4. **Every state change is one transaction.** A character is in exactly one place at a time
   (`characters.location`), moved only by the hub inside a transaction; the ledger of Phase 5
   builds on the same rule.
5. **Measured.** 200 bots on one zone must leave half the tick free; the login → zone → handoff
   → logout round trip is timed and gated.

## 2. Processes

- `gm-hub`: one process per deployment. Listens on one QUIC endpoint for clients and zones,
  holds the zone registry in memory, talks to Postgres through `sqlx`. Migrations ship with the
  binary and run at start (`gm-hub --database-url ... [--migrate-only]`).
- `gm-server`: one process per zone. At start it connects to the hub (`--hub ADDR --hub-cert
  PATH --zone-secret S --zone-id NAME --public-addr ADDR`), registers, heartbeats every 5 s, and
  keeps the connection as its control channel to the hub. Without `--hub` it runs **open** as in
  Phases 2 and 3 (empty tokens accepted; builds live for the session).
- `gm-client` / `gm-bot`: `--hub ADDR --hub-cert PATH --user EMAIL --password PW
  [--character NAME] [--zone ID]`; they log in, pick a character and a zone, receive a ticket,
  and connect to the zone with it. `--connect` without a hub still works for development.

## 3. Hub protocol

Transport: QUIC with the idle timeout and keep-alive of PROTOCOL.md 1 (10 s, 2 s), **1,024**
concurrent bidirectional streams per connection (a zone saving thirty characters at once must
never block on a stream limit) and, since Phase 6, QUIC's loss-based congestion control (Cubic)
instead of the zones' fixed 64 KiB window: that window is right for a few KB/s of datagrams and
would cap a model download at 640 KB/s on a 100 ms path (MODELS.md 6.2).
Certificates: the hub presents a self-signed certificate written at start (`--cert-out`);
clients and zones trust exactly that file. Every request is one bidirectional stream: the
requester writes one framed `HubRequest` and finishes; the hub writes one framed `HubResponse`
and finishes. Framing is PROTOCOL.md 8 (big-endian u16 length + `bitcode`). Messages over
65,535 bytes are protocol errors. Since v1.6 a stream **begins with the version** of these
messages in a frame of one byte (`HUB_VERSION`, 9 since Phase 14, 11 since the stacks of MODES.md 11), and the hub answers with its own in the
same way before anything else, going on to the response only when the two are equal: a
zone, a tool or a bot of another build is told so ("the hub speaks version N") instead of
failing to decode. A stream that begins with an empty frame speaks the players' messages
(3.8), with a version of their own. A stream that asks nothing within 15 s is dropped. Two requests carry bytes that are not messages (MODELS.md
6.2): `ModelUpload` is followed on the same stream by `len` raw bytes, checked against the
limit before one of them is read, and the answer to `ModelGet` and `Mod(Preview)` is
`Blob { len }` followed by `len` raw bytes. A zone's connection also carries hub-initiated
notices on unidirectional streams opened by the hub (`HubNotice`, section 3.4).

```
enum HubRequest {
    // anyone
    Register { email: String, password: String },
    Login { email: String, password: String },
    // accounts (session: the Login response's session id, 24 h, in-memory)
    Characters { session: SessionId },
    CreateCharacter { session: SessionId, name: String, build: BuildChoice },  // preset name or full build
    SetBuild { session: SessionId, character: CharacterId, build: BuildChoice },  // an offline character's (NotFound while it plays);
                                                                                // no screen calls it: a build worn in a zone comes with that zone's Save (3.2)
    ListZones { session: SessionId },
    Content { session: SessionId },                              // the pack and the presets' blurbs (3.8)
    Trials { session: SessionId, character: CharacterId },       // what it has passed (3.5)
    Enter { session: SessionId, character: CharacterId, zone: ZoneId },   // "" = where it was, else the start zone (3.8)
    Logout { session: SessionId },
    // zones (authenticated by the zone secret in ZoneHello, once per connection)
    ZoneHello { secret: String, zone: ZoneId, map: String, map_hash: u64, addr: SocketAddr, cert_der: Vec<u8>,
                requires: Vec<String>,           // trials that open the zone; empty = open to all (3.5)
                max_players: u32 },              // how many clients it takes (3.8)
    Heartbeat { players: u32, tick_mean_us: f32 },
    Claim { token: SessionToken },
    Save { character: CharacterId, state: CharacterState, leaving: bool },
    Release { character: CharacterId },          // claimed, and no body for it here (3.8)
    Handoff { character: CharacterId, state: CharacterState, to_zone: ZoneId,
              web: bool },                       // the traveller's client is a browser (3.8)
    Trial { character: CharacterId, trial: String, secs: u32 },  // a character here passed it (3.5)
    // the economy (ECONOMY.md): a session for one of its own characters; a zone for itself
    Econ { session: SessionId, character: CharacterId, op: EconOp },
    ZoneEcon(ZoneEconOp),
    // parties and lines (PARTY.md 3; 3.10): a zone, for a character that plays in it
    ZoneParty(ZonePartyOp),                      // Invite, Answer, Leave, Remove, Say, Read
    // avatar models (MODELS.md 6.2, 10)
    ModelUpload { session: SessionId, frame: u8, tos_version: u16, len: u32 },   // + len bytes
    ModelList { session: SessionId },
    ModelDrop { session: SessionId, model: ModelId },
    SetModel { session: SessionId, character: CharacterId, model: Option<ModelId> },
    ModelGet { session: SessionId, model: ModelId },
    Mod { session: SessionId, op: ModOp },      // moderators: Queue, Preview, Decide, Takedown,
                                                // Reinstate, SetUpload, SetTrust, ClearStrikes
}

enum HubNotice {                   // hub → zone, unidirectional streams
    Claimed { character: CharacterId },
    Kick { character: CharacterId, reason: String },
    ModelRevoked { model: ModelId },            // to every zone (MODELS.md 6.3)
    StallClosed { stall: i64 },                 // to the stall's zone (ECONOMY.md 7)
    HireEnded { hirer: CharacterId, hire: i64 },// to the hirer's zone (3.5)
    // 3.10: to the zone of each character it concerns, the asking zone among them
    Party(PartyNews),                           // a party changed: its number, what it is now, who left
    Invited { to: CharacterId, from: String },
    Declined { to: CharacterId, by: String },
    Heard { to: Vec<CharacterId>, channel: u8, from: String, text: String },  // everybody of the zone who hears it
}

enum HubResponse {
    Ok,
    Err(HubError),                 // typed: Credentials, Taken, NotFound, Busy, Unauthorized,
                                   // Invalid(String), Internal, Insufficient, Full, Cooldown,
                                   // Gone (taken down, not served), Locked(trials) (3.5)
    Session { session: SessionId, account: AccountId },
    Characters(Vec<CharacterSummary>),
    Character(CharacterSummary),
    Trials(Vec<(String, u32)>),    // trial key, best time in seconds
    Zones(Vec<ZoneSummary>),       // id, map, players, max_players, web, min_trust, requires, address,
                                   // cert hash, up for ms
    Content { pack: ContentPack, blurbs: Vec<String> },   // 3.8
    Ticket(ZoneTicket),            // addr, cert_der, token
    Claimed { character: CharacterId, name: String, state: CharacterState, team: u8,
              model: Option<ModelRef>,    // the avatar model it wears, while it is active
              squad: Vec<HiredAvatar>,    // its active hires (3.5)
              gear: GearReading,          // what its worn items do to damage, numbered (3.9)
              party: PartyReading },      // the party it is of, numbered (3.10)
    Registered { public_key: [u8; 32] },  // the hub's token verification key
    Econ(EconReply),               // Done, Id, Ids, Holder, TradeView, Trade, Decided, Tavern,
                                   // Squad, Stall, Stalls, Listings, Gear
    Party(PartyReply),             // Invited, Declined, News, Said, Reading (3.10)
    Saved { party: u64 },          // to a Save that does not leave: the number of the character's
                                   // last change of party (3.10)
    Models(Vec<ModelSummary>),
    ModelAccepted { model: ModelId, status: ModelStatus },
    Blob { len: u32 },             // + len bytes
    ModQueue(Vec<ModEntry>),
}
```

`SessionId` is 16 random bytes; sessions live in hub memory until 24 h after their last use
or until `Logout` (3.8).
`CharacterId`, `AccountId` are database ids (i64). `ZoneId` is the zone's configured name.
Character names follow 3.8 (2..=24 bytes, letters first, unique by what they look like); an
account holds at most **10** characters. Password hashing runs on the
blocking pool behind a semaphore of 8 permits; a ninth concurrent `Register`/`Login` answers
`Busy` rather than stalling the executor that carries the zones' heartbeats. `Register` and
`Login` are rate limited per source address (a token bucket of 10 per minute).

`EconOp` and `ZoneEconOp` are listed in `gm-hub-proto::protocol` and specified by ECONOMY.md:
every op is one database transaction. `Econ` is refused with `NotFound` unless the character
belongs to the session's account, and with `Busy` past five requests a second for one account
(3.9). `ZoneEcon` is refused with `Unauthorized` on any connection
that has not said `ZoneHello`; a zone grants only to characters playing in it and decides only
contracts whose instance it is.

`Logout` of an account's **last** session while a character is in a zone also tells that zone
to drop the player (`HubNotice::Kick`): nobody is left playing for an account that has logged
out everywhere. Logging out of one client leaves the account's other client alone (3.8).

### 3.1 Tokens

```
SessionToken {
    payload: TokenPayload { account, character, zone: ZoneId, issued_at, expires_at, nonce: [u8; 16] },
    signature: [u8; 64],            // ed25519 over the bitcode-encoded payload
}
```

- Valid for **60 s** from issue; bound to one zone; the nonce is single-use (the zone keeps the
  nonces it accepted until their expiry). Zones allow **5 s** of clock skew on the expiry check;
  NTP on every host is a deployment requirement, not a protocol feature.
- The hub's signing key is persisted (`--key PATH`, created on first start) so a hub restart
  does not invalidate tickets issued a moment before it; zones re-read the public key from
  `Registered` on every reconnect.
- The zone verifies the signature against the hub's public key (from `Registered`), the zone id,
  and the expiry **before** it answers `Hello`; then it sends `Claim` to the hub. The hub moves
  the character to the zone in one transaction and returns the state. A character that is in
  another zone cannot be claimed; a character `in_transit` can be claimed only by the zone the
  ticket names. The zone ignores `Hello.name` when a token is present: the name comes from
  `Claimed`.
- A zone without a hub (`--open`) accepts the empty token as in Phase 2.

### 3.2 Character state

```
CharacterState {
    build: Build,                   // MATRIX.md 9, validated at every write
    zone: Option<ZoneId>,           // where the position belongs; None = spawn
    position: [f32; 3], yaw: f32,
    viewport: u8,                   // last viewport preference
    play_seconds: u32,
}
```

The zone saves every **30 s**, on `Bye`, on disconnect, on handoff, when it stops, and at once
when it takes a respec (MATRIX.md 9.1), telling the player only after the hub answered; `build`
is the one the character chose, a build pending its respawn in a team zone included. The hub
accepts a `Save` or `Handoff` only from the zone the character is in (`location.zone` equals the
zone authenticated by `ZoneHello`); a late save from an origin zone after a handoff, or a rogue
zone writing someone else's character, is refused with `NotFound`. Streams are unordered, so this
rule, not arrival order, is what keeps the location right.

`zone` is the zone whose map the position is on (the column `pos_zone`, written with every
save), not where the character is: a claim hands the position to a zone only when it is that
zone's own, and the zone uses it only when it is still a place to stand on its map (otherwise
the character spawns). **[CORRECTED in Phase 6]** Until then the hub compared against
`location_zone`, which is null while offline and names the origin during a transit, and a zone
trusted whatever it was given: a first entry and every arrival from another zone were placed at
the saved coordinates of the previous map (the origin for a new character), usually inside a
wall. The Phase 4 round trip did not notice because its assertions were about the database;
it now also requires the bots to cover ground in both zones.

### 3.3 Handoff

1. Client → zone: `FromClient::Travel(zone)`.
2. Zone → hub: `Handoff { character, state, to_zone }`. The hub checks the target is registered,
   marks the character `in_transit(from, to)` in one transaction, and answers `Ticket` for the
   target zone.
3. Zone → client: `FromZone::TravelTicket { .. }`. The player's body becomes a **ghost**: it stays in
   the zone (visible, can be hit, cannot act) for at most **10 s**.
4. Client: `Bye` to the origin zone, connect to the target with the ticket's token, `Hello`.
5. Target zone: verifies, `Claim`s; the hub moves the character `in_transit → in target zone` and
   tells the origin zone (`HubNotice::Claimed { character }` on the zone's hub link) to drop
   the ghost. If no claim arrives within 10 s the origin zone keeps the player
   where it was (the hub reverts `in_transit` to the origin zone when the origin zone saves next).

A character in transit cannot `Enter` any zone except with the ticket it was issued, unless the
transit is older than **15 s**: then the hub treats it as abandoned (the origin zone died or the
client never arrived) and `Enter` moves the character directly to the requested zone, at its
spawn. Nothing can wedge a character forever.

### 3.4 Hub notices

The hub opens a unidirectional stream to a zone for each notice: `HubNotice::Claimed {
character }` (drop the ghost), `HubNotice::Kick { character, reason }` (the account logged
out, or an operator removed it), `HubNotice::ModelRevoked { model }` (to every zone: a takedown,
MODELS.md 6.3), `HubNotice::StallClosed { stall }` (to the stall's zone: its owner closed it
or its 48 h ran out, ECONOMY.md 7), `HubNotice::HireEnded { hirer, hire }` (to the zone the
hirer plays in: the avatar's owner took it back, or the hirer dismissed it) and the four of
3.10 (`Party`, `Invited`, `Declined`, `Heard`). Notices are advisory for the zone's
bookkeeping; the database is already updated when they are sent. A zone that reads a notice
it does not know goes on to the next: one stream, one notice.

### 3.5 Squads, trials and gated zones (COMPANIONS.md 3.3, 10, 11)

- **The squad at the claim.** `Claimed.squad` lists the character's active hires, oldest
  first, at most the squad capacity of its build (three, five with a leadership ability):
  `HiredAvatar { hire, character, name, build, model, expires_at }`. The zone spawns a
  companion for each; a hire whose stored build no longer validates against the content is
  left out. The same claim ends every active hire *of* the claimed character as an avatar
  (its owner is playing it now, ECONOMY.md 11) and tells the hirers' zones.
- **Trials.** `Trial { character, trial, secs }` is accepted from a zone only for a
  character playing in it and only for a trial the content gives to that zone's map; it is
  stored once per character and trial with the fastest time (`trials`). `Trials` answers a
  session what one of its characters has passed.
- **Gated zones.** A zone that registers with `requires` (trial keys of the content; at most
  16) is entered only by characters that have passed one of them: `Enter` and `Handoff`
  answer `Locked` with the trials' names otherwise.
- **A kill** is reported with `ZoneEconOp::GrantKill` (ECONOMY.md 9): one transaction, once
  per `(zone, reference)`.

### 3.6 Browsers (WEB.md 2)

- `gm-hub --web-listen ADDR [--web-cert PEM --web-key PEM] [--web-url URL] [--web-origin
  ORIGIN]... [--web-info-out FILE]` opens a WebTransport listener beside the QUIC endpoint.
  A session's bidirectional streams carry the same requests with the same framing, one per
  stream; uploads and downloads write their bytes raw on the stream as before. Zones keep
  talking QUIC.
- `ZoneHello` gains `web: Option<WebAddr>`: the zone's own WebTransport listener. `ZoneTicket`
  carries it on, so a ticket names both ways into a zone; a browser needs `web`, a native
  client `addr` and `cert_der`.
- Rate limits and sessions do not know the transport. The `Origin` allow-list of the web
  listener is not authentication (WEB.md 2.1).

### 3.7 Conduct (ANTICHEAT.md)

- Zones: `ZoneAim { nonce, character, stats }` (a client's aim numbers since its last
  report, added to its account's week; the nonce makes a repeat count once),
  `ZoneReplay { summary, len }` followed by the file's bytes (answered `ReplayStored { id }`;
  stored by hash under `<models-dir>/replays/`, the same bytes twice being one replay; the
  summary names the reports the file was written for and each is given the replay),
  `ZoneReport { reporter, target, reason }` (answered `ReportOpened { id }` within the
  reporter's limits). `ZoneHello` gains `min_trust`. A zone speaks for any existing
  character here: a leaver's numbers arrive after it went.
- Everyone: `Login` answers `Err(Banned { until, reason })` for a banned account, after the
  password. `Enter`, `Claim` and `Handoff` refuse a banned account and one below the
  destination's `min_trust` (`Locked("trust tier N")`).
- Moderators (`Mod`): `AimReport`, `Replays`, `ReplayGet` (a blob), `Reports`,
  `ReportVerdict`, `Ban`, `Unban`, `Reputation`, `Adjust`; each is a `mod_log` row. A ban
  ends the account's sessions, kicks its characters from their zones (the `Kick` notice)
  and closes its stalls.
- Once a minute the hub's sweeper also deletes replays past their retention, closes flags
  older than 90 days, and removes files in the replay store that no row names.
- An account is promoted from trust tier 0 to 1 (ten hours played, a reputation that is not
  negative, no report upheld in 30 days) when a character's leaving save arrives, when aim
  numbers arrive, and at the door of a zone that asks for a tier.
- Migration 0006 (`conduct`): `replays`, `replay_participants`, `aim_weeks`, `aim_reports`,
  `flags`, `reports`, `reputation`, `bans`, `mod_log`, `accounts.reputation`.

### 3.8 What the client's screens lean on (CLIENT.md 7)

- **The players' messages** (`gm_hub_proto::player`). `PlayerRequest` is the requests a
  player's client makes (`Register`, `Login`, `Characters`, `CreateCharacter`, `ListZones`,
  `Content`, `Enter`, `Logout`, `ModelGet`, and since Phase 11 `Econ` with the few economy
  requests that have a screen, 3.9) and `PlayerResponse` their answers (`Ok`, `Err`,
  `Session`, `Characters`, `Character`, `Zones`, `Content`, `Ticket`, `Blob`, `Econ`), as enums of
  their own: the same requests, handled by the same code, in an encoding that does not carry
  what zones, moderators and the economy say. (A browser client that spoke `HubRequest` paid
  89 KB of its megabyte for codecs it never uses.) On the wire a stream that speaks them
  begins with an **empty frame** (`00 00`: no `HubRequest` encodes to nothing) and a frame
  of one byte, the **version** of the players' messages (`PLAYER_VERSION`, 3 since Phase 12); the hub
  answers with its own version in a frame of one byte, and then, if the two are the same,
  with the framed `PlayerResponse`. A client of another version is told so in words it can
  show, whatever else changed between the builds. The two versions are apart on purpose:
  the hub's messages change with every phase, and a player's installed client need not
  care unless the ones it speaks did.
- `Content` answers the content pack the hub loaded and one line or two of plain words for
  each of its preset builds (`blurb` in `assets/content/builds.toml`, at most 160
  characters, required), in the pack's order. The blurbs are not part of the pack.
- `CharacterSummary.last_zone` is the zone the saved position belongs to (3.2).
- `Enter` with an empty zone name tries, in order: the zone the character was last in; the
  hub's `--start-zone ID`; the zone called `town`; every other zone by name. It takes the
  first that is up, has room, asks for no trial or trust tier the character lacks, and has
  a web listener if the request came from a browser. With none: `NotFound`. A zone named
  outright is refused for what is wrong with it (`NotFound`, `Full`, `Locked`, `Invalid`
  for a browser and a zone without a web listener).
- **Room.** A zone says how many clients it takes (`ZoneHello.max_players`, one at least);
  the hub counts the characters whose location is that zone, in the database, and answers
  `Full` to `Enter` and to `Handoff` when there are that many. Characters in transit are
  **not** counted: a ticket costs nothing to ask for, and counting them would let a
  handful of accounts hold every seat with tickets they never use. Two that race for the
  last seat both get a ticket, and the zone's own count decides at its door.
- **`Release { character }`** (zones): the zone claimed the character and has no body for
  it (it is full after all, the build is not valid there, the client went away before the
  handshake ended). The character is offline again and nothing else about it changes: it
  never stood there, so where it last stood is what it was. A claim that fails at the hub
  after the character was moved gives it back the same way. A zone also answers a `Kick`
  for a character it has no body for with `Release`, unless that character's leaving save
  is on its way (a logout races the save of the character it logs out); and a zone drops a
  body whose periodic save the hub refuses as not being there.
- **Transits end.** A character in transit whose ticket has run out (65 s) is offline
  again, by the sweeper once a minute; so are a banned account's transits, at the ban.
- `Enter` refuses a character whose stored build the content no longer takes, in words,
  before any ticket is cut. `Handoff` says whether the traveller is a browser, and a zone
  without a web listener is refused to one (`Invalid`).
- `ZoneSummary` gains `max_players`, `web` (a browser can reach it), `min_trust` and
  `requires`: the travel screen says why a zone cannot be gone to before anybody asks.
- **Sessions slide**: every request that names a session renews it. The client says
  something every ten minutes while it plays (a zone is played without a word to the hub).
  A session ends 24 h after its last use, 30 days after its login whatever its use
  (`SESSION_LIFETIMES`), at its `Logout`, and at a ban; an account has eight at once, and
  a ninth login ends the one used longest ago. Sessions that have run out are forgotten
  once a minute.
- **Login timing.** An email no account has costs the same password hash as one that has
  (a fixed decoy), so the time to `Credentials` does not say which emails exist.
- **Names** (`gm_hub_proto::names`): trimmed; two characters or more, 24 bytes at most; a
  letter first (ASCII or one of `č ć đ š ž` and their capitals); then letters, digits,
  and single spaces, hyphens and apostrophes between them. Not a reserved word (`zone`,
  `system`, `server`, `admin`, `admins`, `administrator`, `moderator`, `moderators`, `mod`,
  `mods`, `gm`, `gms`, `gamemaster`, `staff`, `support`, `official`, `hub`, `gamengine`,
  `nobody`), nor one as a word of its own or with a number behind it (`Zone 2`, `GM Bob`,
  `Moderator7`). Unique by **skeleton**: small letters without their marks, `i I l L 1` as
  `l`, `0 O o` as `o`, joiners removed. The skeleton is stored in `characters.name_key`
  (migration 0007, which keys the names made before it the same way and keeps both owners
  of two that are alike; they are not judged by the new rules). A name that is refused is
  refused with the rule it broke, in words (`Invalid`). A password is 8 characters or
  more and 256 bytes at most.

### 3.9 Possessions (ITEMS.md)

- **The item bar** (LOOK.md 3.2, ITEMS.md 4; `HUB_VERSION` 12, `PLAYER_VERSION` 7,
  2026-10-08): a small record a character, `bars (character_id, cell_1..cell_4 text)`,
  one row once the character has arranged it (migration 0013). `EconOp::Bar` reads it
  (the default without a row: the first stack carried that heals, on the first cell);
  `EconOp::SetBar { cells }` writes it, as a session's own request, since it is not part
  of the save a zone makes (HUB.md 3.2 keeps the zone the only writer of the character
  while it plays, and this changes nothing of that): the character's zone is then told
  with the gear, as after a storage move (`tell_zone_of_items`, `HubNotice::Gear`), and
  `GearReading.bar` carries the four cells to it in every reading.

- **What is worn changes through a zone.** `ZoneEconOp::Wear { character, item }` and
  `TakeOff { character, item }` are answered `EconReply::Gear(GearReading { seq, gear,
  templates })` (`templates`: the keys of the templates worn by place, weapon and armour,
  empty for nothing; v1.9, LOOK.md 6.2: keys, not indices, so a hub and a zone on
  different content disagree about nothing but what to draw):
  a reading of what the character's worn items do, made after the change was committed.
  The transaction holds the character's row (shared) against a change of where it is, and
  refuses (`Unauthorized`) a character that does not play in the asking zone: offline, in
  transit, or another zone's. A claim, a handoff and a save take that row whole, so a
  claim either reads the change or comes first. `Claimed.gear` is a reading too, made
  after the character became the claiming zone's. Readings are numbered from one sequence,
  the number drawn before the rows are read: of two readings a zone has for a character,
  the one with the larger number is true (ITEMS.md 3.3). No session request and no notice
  touches what is worn.
- **Buying is a zone's request.** `ZoneEconOp::StallBuy { character, stall, listing, price }`:
  the zone saw the character standing at that stall; the hub checks everything else (the
  character plays in that zone, the stall stands in it, the listing is that stall's, the
  price, not the keeper, coin, room). `EconOp::StallBuy` is gone.
- **Looking, listing, taking back.** `EconOp::StallView { stall }` from anywhere answers
  `Listings { owner, mine, listings }`. `StallList` and the new `StallUnlist { listing }`
  need the keeper to be playing in the stall's zone.
- **An item says itself.** `ItemSummary` carries `place` (0 none, 1 weapon, 2 armour), `edge`
  (per mille per damage type), `worn`, and the words `what` and `does`, composed by
  `gm-content` from the hub's item content: a client shows them and works nothing out.
- **The players' economy.** `PlayerEcon`: `Inventory`, `Storage`, `StorageDeposit`,
  `StorageWithdraw`, `StallView`, `StallList`, `StallUnlist`; `PlayerEconReply`: `Done`,
  `Id`, `Holder`, `Listings`. The same requests as their `EconOp`s; an answer none of them
  has is the hub's mistake.
- **A craft** names at most six parts (an item has no more places); more is refused before
  the hub looks at any of them.
- **A limit.** A session is all it takes to ask the economy something, every answer is a
  transaction, and some read a row per item: one **account** (whatever its sessions and
  connections) may make five `Econ` requests a second with twenty in hand
  (`--econ-per-second`); past that the answer is `Busy`, before the database is asked
  anything. A zone's own requests are not counted: a zone gates its players itself
  (PROTOCOL.md 8).
- **An operator's hand** (`gm-hub --database-url URL ...`, then exit; the hub may be
  running): `--grant-coin CHARACTER SILVER`, `--grant-item CHARACTER TEMPLATE
  MATERIAL,...` (ledger reason `grant`), `--place CHARACTER ZONE X,Y,Z YAW` (an offline
  character's saved position), `--audit` (the books in one line; exit status 1 when they
  are not sound).

### 3.10 People together (PARTY.md)

- **A party is the hub's**: `parties`, `party_members`, `party_invites` (PARTY.md 3.1). A
  zone asks for a character that plays in it (`ZoneParty`; `Unauthorized` otherwise, as a
  zone's economy requests are) and is answered `Party(PartyReply)` or `Err(Invalid(words))`
  with the words the player is to read.
- **One writer at a time.** Every change of any party is made by one writer: a lock in the
  process and an advisory lock in the transaction (a second hub on one database, a tool).
  Parties change at the pace people click; nothing in the path of a tick waits for this.
- **Numbered.** Every change draws a number from the sequence `party_seq` inside its
  transaction and stamps it on the party's row and on the row of each character that
  joined or left by it (`characters.party_seq`); the news of it carries that number
  (`PartyNews { seq, party, left }`), and so does a reading (`PartyReading { seq, party }`:
  `Claimed.party`, and the answer to `Read`). Of two things a zone holds about a
  character, the one with the larger number is true, whatever order they arrived in.
- **Told without waiting.** After the commit the hub looks up where each concerned
  character plays and opens a notice to each of those zones, the asking one among them (an
  answer and a notice are then the same news, and a zone applies it once). Nothing waits
  for a zone to read: each notice is sent by a task of its own, with five seconds of
  patience.
- **The repair.** A notice can be lost (a zone stalled past the patience; a character
  claimed between the commit and the look-up). A `Save` that does not leave is answered
  `Saved { party }`, the number of the character's last change; a zone that holds a
  smaller one asks `Read`. A character is saved every thirty seconds: that is how long a
  zone can be wrong.
- **The sweep** (every ten seconds; a task of the hub's): invitations older than a minute
  go; a party whose leader is offline is led by its longest-standing member in the game
  (any change of a party does the same when its leader is offline); a member offline for
  longer than `--party-away SECS` (120, from `characters.offline_since`, which a trigger
  sets when a character goes offline) is taken out, with the party's rows locked in
  ascending order; a party of one is no party.
- **Lines.** `Say { from, to: Party | Whisper(name), text }`: the hub looks up who hears
  (the party's members; the one character of that name, as names are compared, 3.8) and
  sends `Heard` to the zone each plays in, or to both zones of one on its way. The hub
  keeps no line. The zone has checked the text and the sender's limit (PARTY.md 5).
- **A trade is opened by the zone both play in**: `ZoneEconOp::TradeOpen { a, b }`, refused
  for anybody who does not play there; `EconOp::TradeOpen` is gone. A character has one
  open trade: opening another cancels it. `TradeView` says whether both still play in one
  zone, and how long until an accept is taken; an accept that finds them apart cancels the
  trade. A trade also ends at a claim of either character, and after ten idle minutes (the
  minute's sweep).
- **A hire names its price and buys what was listed**: `EconOp::Hire { avatar, price }` is
  refused when the listing shows another price now (`the price changed: look again`);
  a listing keeps the build the character had when it was listed (`hire_listings.build`),
  the tavern shows that, and the hire keeps it (`hires.build`). A listing whose build the
  content no longer has is neither shown nor sold (`that character is not for hire now:
  its owner must list it again`). `HireListed` answers what one's own listing is;
  `HireUnlist` withdraws it. `TavernEntry` and `HiredAvatar` say `what` and `role` in
  words, of a build that validates.
- **The players' messages are v3**: `PlayerEcon` gains the trade's requests (`TradeView`,
  `TradeOffer`, `TradeRetract`, `TradeCoin`, `TradeAccept`, `TradeCancel`) and the
  tavern's (`Tavern`, `Hires`, `HireListed`, `Hire`, `Dismiss`, `HireList`, `HireUnlist`);
  `PlayerEconReply` gains `TradeView`, `Trade`, `Tavern` and `Hires`.

## 4. Database (PLAN.md 11.4)

```
accounts   (id bigserial, email text unique, password_hash text, created timestamptz,
            trust_tier smallint default 0, upload_privileges bool default false)
characters (id bigserial, account_id → accounts, name text unique, build jsonb, location jsonb,
            viewport smallint, play_seconds int, created, updated)
zones_log  (id bigserial, zone text, event text, at timestamptz)        -- registry events, ops
bars       (character_id → characters, cell_1..cell_4 text)            -- the item bar (3.9), 0013
```

`location` is typed, not free JSON, because "exactly one place" is an invariant the database
must hold: columns `location_kind` (`offline` | `zone` | `transit`), `location_zone` (the zone,
or the transit origin), `transit_to`, `transit_since`, `position` (three reals), `yaw`, with a
check constraint that the zone columns are null exactly when offline. Builds are stored as JSON
for inspection; the hub validates them against the content pack it loads at start (the hub and
every zone of a deployment load the same `assets/content`, and the hub refuses a zone whose
`map_hash`/content differ from what it knows for that zone id). Items, stalls, escrow and the
ledger arrive in Phase 5 as separate tables (ECONOMY.md); Phase 6 adds `models`,
`model_holders`, `model_events`, `characters.model` and three account columns (MODELS.md 6.1),
and `characters.pos_zone` (section 3.2); Phase 7 adds `hires.ended`, `trials (character_id,
trial, zone, secs, passed_at)` and `kills (zone, ref)` (migration 0005); Phase 10 adds
`characters.name_key`, unique (migration 0007, section 3.8) and an index for a zone's
room count (0008); Phase 11 adds `worn (character_id, slot, item_id)`, the trigger that
keeps a worn item with its wearer and the sequence its readings are numbered from (0009,
ITEMS.md 2 and 3.3); Phase 12 adds `parties`, `party_members`, `party_invites`,
`characters.party_seq`, `characters.offline_since` with its trigger, the sequence
`party_seq`, `hires.build`, `hire_listings.build` and partial indexes on open trades
(0010, PARTY.md 3.1). Migrations are embedded in the binary and run at
start in order; the hub refuses to start on an unknown newer schema.

Passwords: argon2id with the crate defaults (19 MiB, 2 iterations, parallelism 1), one hash per
account, never logged. Emails are stored lowercase and trimmed. A `Register` with an existing
email answers `Taken`: an address is not a secret here (nothing is ever mailed to it), and
`Login` no longer tells which exist by its timing (3.8).

## 5. Budgets and acceptance (PLAN.md 11.8 Phase 4)

- **200-bot swarm** (`scripts/check-swarm.sh`, real UDP on loopback, one zone on the arena, 200
  duelist bots for 20 s, everyone in one hall so nothing is culled, the worst case): server tick
  mean under `[server].max_tick_mean_us` and p99 under `max_tick_p99_us` from `budgets.toml`,
  server RSS under `max_rss_bytes`, per-player bytes under the Phase 2 budget. Numbers are set
  from the first measurement with margin, like the netcode gate. The gate runs on the reference
  machine and the self-hosted runner; CI runs a 32-bot smoke with the same assertions.
- **Round trip** (`crates/gm-hub/tests/handoff.rs`, loopback: one hub on a test database, two
  zones): register → login → create character → enter zone A → travel to zone B → logout, with
  the character's location checked in the database at every step; the whole trip under
  `[hub].round_trip_ms`. The test is event-driven (it waits for `Travel`, `Welcome`, `Claimed`),
  never sleeps. It needs `GM_TEST_DATABASE_URL`; without it the test is skipped with a message
  (CI provides a Postgres service).
- Every zone state change is a transaction: a `Claim` for a character already in a zone fails,
  and a `Save` from the wrong zone fails.

## 6. Deliberately absent

No HTTP API: the tools speak the same protocol (`gm-tools model`, `gm-tools mod`,
`gm-tools hub`; the first moderator is made with `gm-hub --grant-moderator EMAIL`). No OAuth, no email
verification, no password reset (Phase ∞, with the website). No zone-to-zone direct links: every
move goes through the hub. No sharding of the hub.

## 7. Open

Whether zone secrets become per-zone certificates signed by the hub. (Resolved in Phase 6: the
asset ingestion API lives on the same endpoint, bytes raw on the request's own stream.)

## 8. Implementation notes (Phase 4)

`gm-hub-proto` (GPL, what clients link): `protocol` (the messages), `token` (ed25519 issue and
verify with a nonce memory), `client` (the connection zones, bots and the client use). `gm-hub`
(AGPL): `db` (sqlx, embedded migrations, every move one transaction) and `hub` (the process).
`CreateCharacter` and `SetBuild` take a preset name or a full build, so clients carry no content
parser. `gm-server` gains `hub_link` and the zone loop
does saves every 30 s (staggered), ghosts, travel and notices; `--hub` makes tokens mandatory.
`gm-bot --hub ...` and `gm-client --hub ...` log in, enter, travel and log out through the hub;
the client reloads the map of a zone it travels to from `--maps-dir`.

Three things the implementation settled that the draft left implicit:
- A ghost's `Bye` keeps its body and the hub's transit; the origin zone drops the ghost on
  `HubNotice::Claimed`, or after 10 s: back to play if the client is still connected, otherwise
  the body leaves and the character goes offline (the origin may save a transit it started).
- A zone that disconnects from the hub takes its characters offline at the hub; the players
  keep playing but their next saves are refused until they re-enter. A zone restart does the
  same (`ZoneHello` orphans). This is the conservative choice; a network blip costs a re-login.
- The hub verifies tokens too (its own key, its own nonce memory per zone), so a compromised
  zone cannot claim with a token it did not receive from a client.

Measured 2026-10-01 (loopback, reference machine): the hub's share of login → enter → travel →
logout is **24 + 110 + 82 + 2 ms**; a character is claimed in the first zone 55 ms after the
flow starts and in the second zone 160 ms after the travel request; argon2id costs ~60 ms per
hash. The 200-bot swarm gate (`scripts/check-swarm.sh`) measured **6.6 ms mean / 9.1 ms p99**
per tick with 200 duelists in the arena hall, 272 MB server RSS, 20 KB/s down per player; the
snapshot pass runs on a `rayon` pool, the simulation step is single-threaded (3 ms of the 6.6).

**Phase 6** added `gm-hub::models` (the store, quotas, moderation; MODELS.md 6 and 10) and the
`ingest-worker` subcommand of the hub binary, which is the same executable started as a child
with resource limits. Measured 2026-10-01: 100 uploads of 1.17 MB each through the worker,
verification, approval and `SetModel` take 37 to 50 s on loopback (0.4 to 0.5 s per avatar);
the two model tests against Postgres with the real worker take 3 to 5 s.

## 9. Design review log

**2026-10-01, v1 draft reviewed by Gemini 3.1 Pro** (independent review before implementation;
verdicts are ours):
- Accepted: the 4-stream transport limit would deadlock a zone saving several characters (hub
  connections allow 1,024); argon2 on the executor would starve the heartbeats (blocking pool
  plus a semaphore); a late `Save` from the origin zone could overwrite a handoff, and a rogue
  zone could write any character (saves are scoped to the character's current zone); a zone
  crash mid-transit wedged the character (transits older than 15 s are abandoned); `Logout`
  left the player in the zone (`HubNotice::Kick`); an ephemeral signing key broke tickets across
  a hub restart (persisted key); clock skew (5 s tolerance, NTP required); unbounded characters
  per account (10) and names (PROTOCOL.md 8 rule); `Hello.name` could be spoofed when a token
  names the character (ignored); the round-trip test must be event-driven.
- Accepted with a different reading: "200 bots in one hall exceed the budget" is the worst case
  the gate is for; measured before this document was written, 200 duelists in the arena cost
  18 KB/s per player on the server side with 29 KB/s peaks per bot, under the 30 KB/s budget.
  The far band stays at 1/6 rate; if a real map ever breaks the budget the lever is the band
  rates, not the test.
- Correct as drafted: nonce memory, single-transaction claims, the 10 s ghost, message sizes,
  30 s crash loss, keep-alives.

**2026-10-01, implementation reviewed by Gemini 3.1 Pro** (after the round trip passed):
- Accepted, all six: a periodic `Save` sent just before a `Handoff` could arrive after it on
  another stream and revert the transit (only transits older than 5 s can be reverted by a
  save); `Logout` took the character offline before the zone's final save, losing up to 30 s
  (the kick now lets the zone save and offline it; only transits go offline at the hub); the
  10-character cap raced without a lock on the account row; the rate limiter swept its map on
  every attempt under the global lock (now once a minute); every character joined at once saved
  at once (saves are staggered by entity id); heartbeats were recorded but never read (zones
  silent for 15 s get no tickets and are not listed).
