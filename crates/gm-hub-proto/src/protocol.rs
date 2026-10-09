//! Hub protocol (HUB.md 3): `bitcode` messages, one request per bidirectional stream, framed
//! like PROTOCOL.md 8.

use std::net::SocketAddr;

use bitcode::{Decode, Encode};
use gm_core::build::Build;
use gm_core::matrix::Gear;
pub use gm_net::control::BuildChoice;

pub type AccountId = i64;
pub type CharacterId = i64;
pub type ZoneId = String;
/// SHA-256 of an ingested model file (MODELS.md 5).
pub type ModelId = [u8; 32];

/// Sixteen random bytes; lives in hub memory for 24 h or until `Logout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct SessionId(pub [u8; 16]);

/// What outlives a zone (HUB.md 3.2).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct CharacterState {
    pub build: Build,
    /// Where the position belongs; `None` = spawn at the next zone.
    pub zone: Option<ZoneId>,
    pub position: [f32; 3],
    pub yaw: f32,
    pub viewport: u8,
    pub play_seconds: u32,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct CharacterSummary {
    pub id: CharacterId,
    pub name: String,
    pub build: Build,
    pub location: LocationSummary,
    /// The zone its saved position belongs to: where `Enter` with no zone named takes it
    /// (CLIENT.md 7). `None` for a character that has never been in one.
    pub last_zone: Option<ZoneId>,
    pub play_seconds: u32,
    /// The model the character wears (MODELS.md 6), whatever its status.
    pub model: Option<ModelId>,
}

/// What a zone is told about a character's avatar: the id clients fetch by and the frame the
/// model was ingested for (MODELS.md 6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ModelRef {
    pub id: ModelId,
    pub frame: u8,
}

/// A hired avatar as the hirer's zone is told about it (COMPANIONS.md 3.3): a copy of the
/// listed character, driven by a mind until the hire ends.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct HiredAvatar {
    pub hire: i64,
    pub character: CharacterId,
    pub name: String,
    /// What it is, in the hub's words: its build's name, frame and armour
    /// (`ironclad: colossus in plate`), and the role a mind plays it in (`tank`).
    pub what: String,
    pub role: String,
    pub build: Build,
    pub model: Option<ModelRef>,
    /// Unix seconds.
    pub expires_at: u64,
}

/// A character listed for hire, as the tavern shows it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TavernEntry {
    pub character: CharacterId,
    pub name: String,
    pub price: i64,
    /// Hires in the last 12 h (three or more sort last).
    pub hires: i64,
    /// What it brings, in the hub's words (as `HiredAvatar::what` and `role`), and as a
    /// build.
    pub what: String,
    pub role: String,
    pub build: Build,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ModelStatus {
    Pending,
    Active,
    Rejected,
    Takedown,
}

impl ModelStatus {
    pub const fn name(self) -> &'static str {
        match self {
            ModelStatus::Pending => "pending",
            ModelStatus::Active => "active",
            ModelStatus::Rejected => "rejected",
            ModelStatus::Takedown => "takedown",
        }
    }

    pub fn from_name(s: &str) -> Option<ModelStatus> {
        [
            ModelStatus::Pending,
            ModelStatus::Active,
            ModelStatus::Rejected,
            ModelStatus::Takedown,
        ]
        .into_iter()
        .find(|m| m.name() == s)
    }
}

/// Why a model was refused or removed (MODELS.md 10). Every code but `Other` is a strike.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ReasonCode {
    None,
    Copyright,
    Likeness,
    Sexual,
    Hateful,
    Other,
}

impl ReasonCode {
    pub const fn name(self) -> &'static str {
        match self {
            ReasonCode::None => "",
            ReasonCode::Copyright => "copyright",
            ReasonCode::Likeness => "likeness",
            ReasonCode::Sexual => "sexual",
            ReasonCode::Hateful => "hateful",
            ReasonCode::Other => "other",
        }
    }

    pub fn from_name(s: &str) -> Option<ReasonCode> {
        [
            ReasonCode::None,
            ReasonCode::Copyright,
            ReasonCode::Likeness,
            ReasonCode::Sexual,
            ReasonCode::Hateful,
            ReasonCode::Other,
        ]
        .into_iter()
        .find(|c| c.name() == s)
    }

    pub const fn is_strike(self) -> bool {
        !matches!(self, ReasonCode::None | ReasonCode::Other)
    }
}

/// A model as its holder (or a moderator) sees it.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ModelSummary {
    pub id: ModelId,
    pub frame: u8,
    pub status: ModelStatus,
    pub bytes: u32,
    pub triangles: u32,
    pub texture: [u16; 2],
    /// The statement of reasons for a rejection or a takedown.
    pub code: ReasonCode,
    pub reason: String,
}

/// One entry of the moderation queue.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ModEntry {
    pub model: ModelSummary,
    pub uploader: String,
    pub holders: u32,
    pub waiting_secs: u64,
}

/// What a moderator may ask (MODELS.md 10).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ModOp {
    /// Pending models, oldest first.
    Queue {
        limit: u32,
    },
    /// The preview image of a model (answered with a blob).
    Preview {
        model: ModelId,
    },
    /// `pending → active | rejected`.
    Decide {
        model: ModelId,
        approve: bool,
        code: ReasonCode,
        reason: String,
    },
    /// `active → takedown`; `reference` names the notice.
    Takedown {
        model: ModelId,
        code: ReasonCode,
        reason: String,
        reference: String,
    },
    /// `takedown → active` (a counter-notice).
    Reinstate {
        model: ModelId,
        reason: String,
    },
    SetUpload {
        email: String,
        allow: bool,
    },
    SetTrust {
        email: String,
        tier: u8,
    },
    ClearStrikes {
        email: String,
    },
    // Conduct (ANTICHEAT.md 7). Every one of these is a row in `mod_log`.
    /// Who stands out by aim over the last `weeks`, most suspicious first.
    AimReport {
        weeks: u8,
        min_shots: u32,
    },
    /// Replays, newest first: of one account, or the reported or flagged ones.
    Replays {
        email: Option<String>,
        reported: bool,
        flagged: bool,
        limit: u32,
    },
    /// The bytes of a replay (answered with a blob).
    ReplayGet {
        id: i64,
    },
    Reports {
        open_only: bool,
    },
    /// `open → upheld | not_proven | abusive`.
    ReportVerdict {
        id: i64,
        verdict: Verdict,
        note: String,
    },
    /// Ban the account for `days`; `cheat` writes `cheat_confirmed` to its reputation.
    Ban {
        email: String,
        days: u32,
        reason: String,
        cheat: bool,
    },
    Unban {
        email: String,
        note: String,
    },
    /// The account's reputation ledger, its tier, its flags and its ban.
    Reputation {
        email: String,
    },
    /// A reputation row by hand.
    Adjust {
        email: String,
        delta: i32,
        note: String,
    },
}

/// A moderator's word on a report (ANTICHEAT.md 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Verdict {
    /// The report was right.
    Upheld,
    /// Nothing to see either way: no consequence for anybody.
    NotProven,
    /// The report was made to harm.
    Abusive,
}

pub use gm_net::control::ReportReason;
pub use gm_replay::aim::AimStats;

/// What a zone tells the hub about a replay it wrote (ANTICHEAT.md 3.3).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ReplaySummary {
    pub started_unix: u64,
    pub seconds: f32,
    /// Written for a report rather than as a fight.
    pub reported: bool,
    /// The reports this file was written for.
    pub reports: Vec<i64>,
    pub kills: u32,
    /// Damage between clients' parties.
    pub damage: u64,
    /// Every character in it and its aim numbers over the file.
    pub participants: Vec<(CharacterId, AimStats)>,
}

/// One account of the aim report (ANTICHEAT.md 4.4).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct AimRow {
    pub account: AccountId,
    pub email: String,
    pub stats: AimStats,
    /// Rules broken: `lock`, `flick`, `laser`, `reaction`, `outlier`.
    pub rules: Vec<String>,
    /// Robust z-scores against the accounts of the report: hit rate on hard shots, lock
    /// rate, flick rate.
    pub z_hard_hits: f32,
    pub z_lock: f32,
    pub z_flick: f32,
    /// The account's most recent replays.
    pub replays: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ReplayRow {
    pub id: i64,
    pub zone: String,
    pub started_unix: u64,
    pub seconds: f32,
    pub reported: bool,
    pub kills: u32,
    pub damage: u64,
    pub bytes: u32,
    /// `(character name, rules its numbers in this file break)`.
    pub participants: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ReportRow {
    pub id: i64,
    pub reporter: String,
    pub target: String,
    pub zone: String,
    pub reason: String,
    pub state: String,
    pub replay: Option<i64>,
    pub created_unix: u64,
    pub note: String,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ReputationRow {
    pub kind: String,
    pub delta: i32,
    pub reference: String,
    pub note: String,
    pub at_unix: u64,
}

/// An account as a moderator sees it.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Standing {
    pub reputation: i32,
    pub trust_tier: i16,
    /// `(until, reason)` of the ban in force.
    pub ban: Option<(u64, String)>,
    /// `(rule, week, detail)` of the open flags.
    pub flags: Vec<(String, String, String)>,
    pub ledger: Vec<ReputationRow>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum LocationSummary {
    Offline,
    Zone(ZoneId),
    Transit { from: ZoneId, to: ZoneId },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ZoneSummary {
    pub id: ZoneId,
    pub map: String,
    pub players: u32,
    /// How many clients it takes; whether a browser can reach it; what it asks of a
    /// character (a trust tier, one of these trials).
    pub max_players: u32,
    pub web: bool,
    pub min_trust: i16,
    pub requires: Vec<String>,
    pub addr: SocketAddr,
    /// FNV-1a 64 of the zone's certificate DER.
    pub cert_hash: u64,
    pub up_secs: u64,
}

/// Entry to a zone (HUB.md 3.1).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct TokenPayload {
    pub account: AccountId,
    pub character: CharacterId,
    pub zone: ZoneId,
    /// Unix seconds.
    pub issued_at: u64,
    pub expires_at: u64,
    pub nonce: [u8; 16],
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct SessionToken {
    pub payload: TokenPayload,
    /// ed25519 over the bitcode-encoded payload.
    pub signature: [u8; 64],
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ZoneTicket {
    pub zone: ZoneId,
    pub addr: SocketAddr,
    pub cert_der: Vec<u8>,
    pub token: SessionToken,
    /// Where a browser reaches the zone (WEB.md 2.3); `None`: the zone has no web listener.
    pub web: Option<WebAddr>,
}

pub use gm_net::control::WebAddr;

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum HubRequest {
    // anyone
    Register {
        email: String,
        password: String,
    },
    Login {
        email: String,
        password: String,
    },
    // accounts
    Characters {
        session: SessionId,
    },
    CreateCharacter {
        session: SessionId,
        name: String,
        /// A preset of the hub's content, or a full build (validated either way).
        build: BuildChoice,
    },
    SetBuild {
        session: SessionId,
        character: CharacterId,
        build: BuildChoice,
    },
    ListZones {
        session: SessionId,
    },
    /// The content pack a zone would send (abilities and preset builds): what a client
    /// shows when a character is made, before any zone is entered (CLIENT.md 4.3).
    Content {
        session: SessionId,
    },
    /// The trials a character of the session's account has passed (COMPANIONS.md 11).
    Trials {
        session: SessionId,
        character: CharacterId,
    },
    /// A ticket for `zone`; an empty name asks for the zone the character was last in
    /// if it is up, else the hub's start zone (CLIENT.md 7).
    Enter {
        session: SessionId,
        character: CharacterId,
        zone: ZoneId,
    },
    Logout {
        session: SessionId,
    },
    // zones (the secret authenticates the connection once; later requests ride on it)
    ZoneHello {
        secret: String,
        zone: ZoneId,
        map: String,
        map_hash: u64,
        addr: SocketAddr,
        cert_der: Vec<u8>,
        /// The zone's WebTransport listener, if it has one (WEB.md 2.3).
        web: Option<WebAddr>,
        /// The least trust tier the zone admits (ANTICHEAT.md 6); 0 = everybody.
        min_trust: i16,
        /// Trials that open this zone: a character must have passed one of them to be let
        /// in (COMPANIONS.md 11); empty = open to all.
        requires: Vec<String>,
        /// How many clients the zone takes: the hub sends nobody to a full one.
        max_players: u32,
    },
    Heartbeat {
        players: u32,
        tick_mean_us: f32,
    },
    Claim {
        token: SessionToken,
    },
    Save {
        character: CharacterId,
        state: CharacterState,
        leaving: bool,
    },
    Handoff {
        character: CharacterId,
        state: CharacterState,
        to_zone: ZoneId,
        /// The traveller's client is a browser: a zone without a web listener is not
        /// somewhere it can follow its character to.
        web: bool,
    },
    /// A character playing in this zone passed a trial (COMPANIONS.md 11).
    Trial {
        character: CharacterId,
        trial: String,
        secs: u32,
    },
    // a session, about one of its characters (ECONOMY.md)
    Econ {
        session: SessionId,
        character: CharacterId,
        op: EconOp,
    },
    // a registered zone connection (ECONOMY.md 8, 9)
    ZoneEcon(ZoneEconOp),
    // a registered zone connection, about the parties of its characters (PARTY.md 3.2)
    ZoneParty(ZonePartyOp),
    // models (MODELS.md 6.2). `ModelUpload` is followed on the same stream by `len` raw bytes.
    ModelUpload {
        session: SessionId,
        /// Archetype frame index the model is for.
        frame: u8,
        /// The upload terms the account certifies (MODELS.md 10).
        tos_version: u16,
        len: u32,
    },
    ModelList {
        session: SessionId,
    },
    /// Give up holding a model; the account's characters stop wearing it.
    ModelDrop {
        session: SessionId,
        model: ModelId,
    },
    /// What an offline character wears.
    SetModel {
        session: SessionId,
        character: CharacterId,
        model: Option<ModelId>,
    },
    /// Answered with `Blob { len }` followed by `len` raw bytes.
    ModelGet {
        session: SessionId,
        model: ModelId,
    },
    Mod {
        session: SessionId,
        op: ModOp,
    },
    // zones: conduct (ANTICHEAT.md 8)
    /// A client's aim numbers since the last report: added to its account's week. `nonce`
    /// makes a repeated report count once.
    ZoneAim {
        nonce: u64,
        character: CharacterId,
        stats: AimStats,
    },
    /// A replay: the summary, then `len` raw bytes on the stream. Answered `ReplayStored`.
    ZoneReplay {
        summary: ReplaySummary,
        len: u32,
    },
    /// A client's report of another body. Answered `ReportOpened` when it is within the
    /// reporter's limits; the zone then records and uploads the replay that belongs to it.
    ZoneReport {
        reporter: CharacterId,
        target: CharacterId,
        reason: ReportReason,
    },
    /// This zone has no body for a character it claimed (it is full after all, the build
    /// is not valid here, the client went away): the character is offline again, as it
    /// came, and nothing else about it is written (HUB.md 3.8).
    Release {
        character: CharacterId,
    },
}

pub type ItemId = i64;

/// What a character may ask of the economy (ECONOMY.md). Every op is one transaction.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum EconOp {
    Inventory,
    /// The item bar (LOOK.md 3.2, ITEMS.md 4): the template on each of the four cells, as
    /// the character arranged it, or the default. v12.
    Bar,
    /// Arrange it: four cells, a stack template or nothing each; the character's zone is
    /// told with its gear. v12.
    SetBar {
        cells: Vec<Option<String>>,
    },
    Storage,
    StorageDeposit {
        item: ItemId,
    },
    StorageWithdraw {
        item: ItemId,
    },
    Craft {
        template: String,
        components: Vec<ItemId>,
    },
    Decompose {
        item: ItemId,
    },
    TradeOfferItem {
        trade: i64,
        item: ItemId,
    },
    TradeRetractItem {
        trade: i64,
        item: ItemId,
    },
    TradeSetCoin {
        trade: i64,
        coin: i64,
    },
    /// `version` is the offer version the client is showing.
    TradeAccept {
        trade: i64,
        version: i32,
    },
    TradeCancel {
        trade: i64,
    },
    /// The offers as the hub holds them, with the version an accept must name.
    TradeView {
        trade: i64,
    },
    /// Into the caller's own open stall, which stands in the zone the character plays in.
    StallList {
        item: ItemId,
        price: i64,
    },
    /// Out of it again, back into the inventory.
    StallUnlist {
        listing: i64,
    },
    StallClose,
    BuyOrderPost {
        material: String,
        price: i64,
        quantity: i32,
    },
    BuyOrderFill {
        order: i64,
        item: ItemId,
    },
    BuyOrderCancel {
        order: i64,
    },
    ContractPost {
        instance: String,
        price: i64,
        collateral: i64,
    },
    ContractCancel {
        contract: i64,
    },
    /// By the party leader; `sellers` includes the leader.
    ContractAccept {
        contract: i64,
        sellers: Vec<CharacterId>,
    },
    ChestDeposit {
        chest: i64,
        item: ItemId,
    },
    ChestWithdraw {
        chest: i64,
        item: ItemId,
    },
    HireList {
        price: i64,
    },
    /// `price`: the price the hirer was shown; the hub refuses when it is another now.
    Hire {
        avatar: CharacterId,
        price: i64,
    },
    /// Take the character off the tavern's list.
    HireUnlist,
    /// The price the character is listed at; answered `Id` (0: it is not listed).
    HireListed,
    Tavern,
    /// The character's active hires (COMPANIONS.md 3.3).
    Squad,
    /// End one of them early; nothing is refunded.
    Dismiss {
        hire: i64,
    },
    /// What a stall has for sale and whose it is, from anywhere. Buying is not here: it is
    /// done standing at the stall, through the zone (`ZoneEconOp::StallBuy`); nor is
    /// wearing, which the zone asks for too (`ZoneEconOp::Wear`).
    StallView {
        stall: i64,
    },
}

/// What a zone reports (ECONOMY.md 8, 9): only a registered zone connection may send these.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ZoneEconOp {
    /// The boss split's result: one component per entry.
    GrantComponents {
        grants: Vec<(CharacterId, String)>,
        reference: i64,
    },
    GrantCoin {
        character: CharacterId,
        amount: i64,
        reference: i64,
    },
    /// Everything one kill gives (ECONOMY.md 9, COMPANIONS.md 10): one transaction, and
    /// one only per `reference`, so the zone may repeat it until it is answered. A
    /// recipient who is no longer playing in the zone gets no coin, and its components lie
    /// on the zone's ground. Answered `Ids`, or `Done` when the kill was paid before.
    GrantKill {
        reference: i64,
        components: Vec<(CharacterId, String)>,
        coin: Vec<(CharacterId, i64)>,
    },
    ContractReport {
        contract: i64,
        outcome: ContractOutcome,
    },
    /// A character drops an item onto this zone's ground, or picks one up from it. The zone
    /// asks, because only the zone knows where the character stands.
    Drop {
        character: CharacterId,
        item: ItemId,
    },
    Pickup {
        character: CharacterId,
        item: ItemId,
    },
    /// A character opens a stall on a tile of this zone's market. The zone asks, because only
    /// the zone knows that the character stands on that tile and that the tile exists
    /// (ECONOMY.md 7).
    StallOpen {
        character: CharacterId,
        tile_x: i32,
        tile_y: i32,
    },
    /// The owner, standing in this zone, closes the stall.
    StallClose { character: CharacterId },
    /// Every open stall of this zone (asked once, when the zone starts).
    Stalls,
    /// A character standing at a stall of this zone buys one of its listings (ITEMS.md 5):
    /// the zone vouches for the place, the hub for everything else. `price` is the price
    /// the buyer was shown.
    StallBuy {
        character: CharacterId,
        stall: i64,
        listing: i64,
        price: i64,
    },
    /// A character playing in this zone puts an item of its inventory on, or takes one
    /// off (ITEMS.md 2). The zone asks, because the zone is where gear takes effect and
    /// where it is known whether the body is in a fight; answered `Gear`, what the
    /// character's worn items do from now on.
    Wear {
        character: CharacterId,
        item: ItemId,
    },
    TakeOff {
        character: CharacterId,
        item: ItemId,
    },
    /// A character playing in this zone used `quantity` of a stack it carries (MODES.md
    /// 11.2: a reload's rounds, a kit): the zone has already spent them in the
    /// simulation and tells the hub; answered `Gear`, the reading after. v11.
    Consume {
        character: CharacterId,
        item: ItemId,
        quantity: u32,
    },
    /// Two characters playing in this zone, standing together, both asked to trade with
    /// each other (PARTY.md 6): the zone vouches for that, and the hub opens the trade
    /// (calling off any trade either still had open). Answered `Id`.
    TradeOpen { a: CharacterId, b: CharacterId },
}

/// An open stall as its zone shows it: where it is and who keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct StallSummary {
    pub id: i64,
    pub tile_x: i32,
    pub tile_y: i32,
    pub owner: CharacterId,
    pub owner_name: String,
    /// The keeper's frame and armour class, and its avatar model while that is active.
    pub frame: u8,
    pub armour: u8,
    pub model: Option<ModelRef>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ContractOutcome {
    Completed,
    Wipe,
    Abandon,
}

/// Where an item is worn (ITEMS.md 2); `PLACE_NONE` for what is not (a component).
pub const PLACE_NONE: u8 = 0;
pub const PLACE_WEAPON: u8 = 1;
pub const PLACE_ARMOUR: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ItemSummary {
    pub id: ItemId,
    pub template: String,
    /// `(layer, material)` in layer order.
    pub components: Vec<(String, String)>,
    /// Where it is worn: `PLACE_WEAPON`, `PLACE_ARMOUR`, or `PLACE_NONE`.
    pub place: u8,
    /// Its edge there, per mille per damage type in `DamageType`'s order (ITEMS.md 3.2).
    pub edge: [u16; 9],
    /// Its holder wears it now.
    pub worn: bool,
    /// What it is, in words: `a weapon, 220 of 250`; `a core, for crafting`.
    pub what: String,
    /// What it does, in words, the strongest first: `slash +11.0%`. A screen shows these
    /// and works nothing out.
    pub does: Vec<String>,
    /// A stack (MODES.md 11.1): how many, and the most its holder may carry; `cap` 0 for
    /// what is not a stack (gear, a part). v11.
    pub quantity: u32,
    pub cap: u32,
    /// Whether the asking character's build holds a weapon like it (ITEMS.md 2: an
    /// ability of the build has the template's model as its prop); true for anything that
    /// is not a weapon. False, `does` ends with the hub's words for it, and the hub
    /// refuses to wear it. Player protocol v5.
    pub fits: bool,
}

/// A reading of what a character's worn items do to damage (ITEMS.md 3.3). `seq` orders
/// the hub's readings: of two a zone has for one character, the one with the larger number
/// is true, whichever arrived first.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct GearReading {
    pub seq: u64,
    pub gear: Gear,
    /// The keys of the templates worn, by place (weapon, armour); empty for nothing (LOOK.md
    /// 6.2: keys, so that a hub and a zone on different content versions disagree about
    /// nothing but what to draw). v1.9.
    pub templates: [String; 2],
    /// The stacks in the inventory (MODES.md 11.2): a firearm's reserve is the quantity of
    /// the one its `ammo` names, the kits are the ones that heal. v11.
    pub stacks: Vec<StackReading>,
    /// The item bar (LOOK.md 3.2): the template key set on each of the four cells, or
    /// nothing. The zone counts a cell's stack from `stacks`. v12.
    pub bar: Vec<Option<String>>,
}

/// One stack of a character's inventory, as the zone reads it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct StackReading {
    pub item: ItemId,
    pub template: String,
    pub quantity: u32,
    /// What one of it heals, when it is a kit.
    pub heals: Option<i32>,
}

/// The states of a trade in `EconReply::TradeView`.
pub const TRADE_OPEN: u8 = 0;
pub const TRADE_COMMITTED: u8 = 1;
pub const TRADE_CANCELLED: u8 = 2;

/// The most characters in a party of people (PARTY.md 2).
pub const PARTY_MAX: usize = 5;
/// How long an invitation waits for its answer, seconds, and how many may wait for one
/// character at once.
pub const INVITE_SECS: u64 = 60;
pub const INVITES_WAITING: i64 = 5;

/// A party of people as the hub holds it (PARTY.md 3): its members in the order they
/// joined, the leader among them.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PartyState {
    pub id: i64,
    pub leader: CharacterId,
    pub members: Vec<(CharacterId, String)>,
}

/// What the hub says of one character's party, numbered: of two readings a zone has for a
/// character, the one with the larger number is true, whichever arrived first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct PartyReading {
    pub seq: u64,
    pub party: Option<PartyState>,
}

/// A change of a party: the party after it (no members: it was dissolved) and who is out
/// of it by this change. For each character it names, it is that character's reading
/// with the number `seq`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PartyNews {
    pub seq: u64,
    pub party: PartyState,
    pub left: Vec<CharacterId>,
}

/// Whom a line that goes through the hub is for (PARTY.md 5).
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum SayTo {
    Party,
    /// One character, by name, wherever in the game it is.
    Whisper(String),
}

/// The channels of `HubNotice::Heard` and of the zone's `Heard` (PROTOCOL.md 19).
pub const CHANNEL_PARTY: u8 = 1;
pub const CHANNEL_WHISPER: u8 = 2;
pub const CHANNEL_WHISPERED: u8 = 3;

/// What a zone asks about the parties of the characters that play in it (PARTY.md 3.2):
/// only a registered zone connection may send these, and the character each names must
/// be playing in that zone.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ZonePartyOp {
    /// Answered `Invited`.
    Invite { from: CharacterId, to: String },
    /// Answered `News` when it joined, `Declined` when it did not.
    Answer {
        character: CharacterId,
        from: String,
        join: bool,
    },
    /// Answered `News`.
    Leave { character: CharacterId },
    /// Answered `News`.
    Remove { leader: CharacterId, name: String },
    /// Answered `Said`.
    Say {
        from: CharacterId,
        to: SayTo,
        text: String,
    },
    /// What the hub holds of a character's party now: asked by a zone whose own reading
    /// is behind the number a save was answered with. Answered `Reading`.
    Read { character: CharacterId },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum PartyReply {
    /// The invitation waits; the name as the hub has it.
    Invited {
        name: String,
    },
    Declined,
    News(PartyNews),
    /// The line was passed on; for a whisper, the name of whom it went to as the hub
    /// has it.
    Said {
        to: String,
    },
    Reading(PartyReading),
}

/// One thing a stall has for sale.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ListingSummary {
    pub id: i64,
    pub item: ItemSummary,
    /// In silver (ECONOMY.md 2).
    pub price: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TradeOffer {
    pub coin: i64,
    pub accepted: bool,
    pub items: Vec<ItemSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum EconReply {
    Done,
    Id(i64),
    /// The item bar's four cells (`EconOp::Bar`). v12.
    Bar(Vec<Option<String>>),
    Ids(Vec<i64>),
    Holder {
        coin: i64,
        items: Vec<ItemSummary>,
    },
    /// A trade as one of its two sees it (PARTY.md 6): whether it is still open
    /// (`TRADE_*`), the version an accept must name, the milliseconds until an accept
    /// is taken, who the other is and whether both still play in one zone, and the two
    /// offers.
    TradeView {
        state: u8,
        version: i32,
        wait_ms: u32,
        with: String,
        together: bool,
        mine: TradeOffer,
        theirs: TradeOffer,
    },
    /// `committed` is false while the other side has yet to accept.
    Trade {
        committed: bool,
    },
    /// The report decided the contract (false: it was already decided).
    Decided(bool),
    Tavern(Vec<TavernEntry>),
    Squad(Vec<HiredAvatar>),
    Stall(StallSummary),
    Stalls(Vec<StallSummary>),
    /// A stall's keeper and what it sells; `mine` when the asker keeps it.
    Listings {
        owner: String,
        mine: bool,
        listings: Vec<ListingSummary>,
    },
    /// What a character's worn items do to damage, read after the change (the answer to
    /// a zone's `Wear` and `TakeOff`).
    Gear(GearReading),
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum HubError {
    Credentials,
    Taken,
    NotFound,
    Busy,
    Unauthorized,
    Invalid(String),
    Internal,
    /// Not enough coin.
    Insufficient,
    /// No room in the inventory, the storage or the stall.
    Full,
    /// Too soon after the last change of a trade.
    Cooldown,
    /// The content was removed and is not served (MODELS.md 6.2).
    Gone,
    /// The zone opens only to characters that passed one of these trials.
    Locked(String),
    /// The account is banned until this unix second, for this reason (ANTICHEAT.md 6).
    Banned {
        until: u64,
        reason: String,
    },
}

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HubError::Credentials => write!(f, "wrong email or password"),
            HubError::Taken => write!(f, "already taken"),
            HubError::NotFound => write!(f, "not found"),
            HubError::Busy => write!(f, "busy, try again"),
            HubError::Unauthorized => write!(f, "unauthorized"),
            HubError::Invalid(s) => write!(f, "invalid: {s}"),
            HubError::Internal => write!(f, "internal error"),
            HubError::Insufficient => write!(f, "not enough coin"),
            HubError::Full => write!(f, "no room"),
            HubError::Cooldown => write!(f, "too soon after the last change"),
            HubError::Gone => write!(f, "removed"),
            HubError::Locked(trials) => write!(f, "locked: pass one of {trials} first"),
            HubError::Banned { until, reason } => {
                write!(f, "this account is banned until unix {until}: {reason}")
            }
        }
    }
}

impl std::error::Error for HubError {}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum HubResponse {
    Ok,
    Err(HubError),
    Session {
        session: SessionId,
        account: AccountId,
    },
    Characters(Vec<CharacterSummary>),
    Character(CharacterSummary),
    /// `(trial key, best time in seconds)`.
    Trials(Vec<(String, u32)>),
    Zones(Vec<ZoneSummary>),
    /// The content pack, and what each of its preset builds is in plain words, in the
    /// pack's order (CLIENT.md 4.3).
    Content {
        pack: gm_core::build::ContentPack,
        blurbs: Vec<String>,
    },
    Ticket(ZoneTicket),
    ReplayStored {
        id: i64,
    },
    ReportOpened {
        id: i64,
    },
    AimReport(Vec<AimRow>),
    Replays(Vec<ReplayRow>),
    Reports(Vec<ReportRow>),
    Standing(Standing),
    Claimed {
        character: CharacterId,
        name: String,
        state: CharacterState,
        team: u8,
        /// The avatar model, present only while it is active (MODELS.md 6.3).
        model: Option<ModelRef>,
        /// The character's active hires, at most its squad capacity (COMPANIONS.md 3.3).
        squad: Vec<HiredAvatar>,
        /// What its worn items do to damage (ITEMS.md 3), read after the character
        /// became this zone's.
        gear: GearReading,
        /// The party it is in (PARTY.md 3), read after the character became this zone's.
        party: PartyReading,
        /// v10: the account is a moderator, so the character is a game master in the zone
        /// (GM.md 1).
        gm: bool,
    },
    Registered {
        public_key: [u8; 32],
    },
    /// A save that was not the character's last was taken; `party`: the number of the
    /// hub's present reading of its party (PARTY.md 3.2). A zone that holds a smaller
    /// one asks for the reading.
    Saved {
        party: u64,
    },
    Econ(EconReply),
    Party(PartyReply),
    Models(Vec<ModelSummary>),
    /// The upload was ingested (or already known): its id and where it stands.
    ModelAccepted {
        model: ModelId,
        status: ModelStatus,
    },
    /// `len` raw bytes follow on the stream.
    Blob {
        len: u32,
    },
    ModQueue(Vec<ModEntry>),
}

/// Hub → zone, on unidirectional streams (HUB.md 3.4).
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum HubNotice {
    Claimed {
        character: CharacterId,
    },
    Kick {
        character: CharacterId,
        reason: String,
    },
    /// A model left `active`: nobody wears it any more (MODELS.md 6.3).
    ModelRevoked {
        model: ModelId,
    },
    /// A stall of this zone closed: its owner closed it, or its 48 h ran out (ECONOMY.md 7).
    StallClosed {
        stall: i64,
    },
    /// The inventory of a character playing in this zone changed by a request that did not
    /// come through the zone (the storage, a trade, a stall of its own): what it wears and
    /// carries now (MODES.md 11.2). v11.
    Gear {
        character: CharacterId,
        reading: GearReading,
    },
    /// A hire of a character playing in this zone ended early: the avatar's owner took it
    /// back, or the hirer dismissed it (COMPANIONS.md 3.3).
    HireEnded {
        hirer: CharacterId,
        hire: i64,
    },
    /// A party changed, and a character that plays in this zone, or is on its way to
    /// it, is in it or left it by this change (PARTY.md 3.2).
    Party(PartyNews),
    /// Somebody invites a character of this zone to a party.
    Invited {
        to: CharacterId,
        from: String,
    },
    /// An invitation a character of this zone made was refused.
    Declined {
        to: CharacterId,
        by: String,
    },
    /// A line for characters of this zone (`channel`: `CHANNEL_*`). `from` is the
    /// speaker's name; on `CHANNEL_WHISPERED`, the name of whom the line went to.
    Heard {
        to: Vec<CharacterId>,
        channel: u8,
        from: String,
        text: String,
    },
}

/// The largest replay the hub stores (ANTICHEAT.md 3.3).
pub const MAX_REPLAY_BYTES: u32 = 48 * 1024 * 1024;
/// Open reports one account may have, and how long a replay is kept, days.
pub const MAX_OPEN_REPORTS: i64 = 5;
pub const REPLAY_KEEP_DAYS: i32 = 14;
pub const CASE_KEEP_DAYS: i32 = 90;

/// Limits (HUB.md 3).
pub const MAX_CHARACTERS_PER_ACCOUNT: i64 = 10;
pub const TOKEN_VALID_SECS: u64 = 60;
pub const CLOCK_SKEW_SECS: u64 = 5;
pub const TRANSIT_ABANDON_SECS: u64 = 15;
pub const SESSION_SECS: u64 = 24 * 3600;
/// A session ends this many of its idle lifetimes after the login, however much it is
/// used (thirty days), and an account has at most this many at once.
pub const SESSION_LIFETIMES: u32 = 30;
pub const MAX_SESSIONS_PER_ACCOUNT: usize = 8;
/// The version of the hub's messages (HUB.md 3: `HubRequest`, `HubResponse` and what they
/// carry); any change to them is a new one. A stream that speaks them begins with it, in
/// a frame of one byte, and the hub answers with its own before anything else: zones,
/// tools and bots of another build are told so instead of being garbled at.
pub const HUB_VERSION: u8 = 12;
pub const HUB_PREAMBLE: [u8; 3] = [0, 1, HUB_VERSION];
pub const HUB_BIDI_STREAMS: u32 = 1024;
/// Password hashes running at once; more answer `Busy`.
pub const HASH_PERMITS: usize = 8;
/// Models (MODELS.md 3, 5, 6.2).
pub const MAX_MODEL_UPLOAD_BYTES: u32 = 8 * 1024 * 1024;
pub const MAX_MODEL_BYTES: u32 = 1_572_864;
/// The largest blob a hub answers with (a model or its preview).
pub const MAX_BLOB_BYTES: u32 = MAX_MODEL_BYTES;
/// The upload terms an account must certify (MODELS.md 10).
pub const TOS_VERSION: u16 = 1;
pub const DEFAULT_MODEL_SLOTS: i16 = 4;
pub const MAX_PENDING_MODELS: i64 = 3;
pub const UPLOAD_STRIKES: i16 = 3;
/// Accounts at this trust tier or above skip the moderation queue.
pub const TRUSTED_TIER: i16 = 2;
/// Ingestion workers running at once; more answer `Busy`.
pub const INGEST_PERMITS: usize = 2;

/// Unix seconds now. Hub and zones only: the browser client has no use for the wall clock,
/// and the std clock panics there.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
