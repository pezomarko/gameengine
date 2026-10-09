//! What a player's client says to the hub and hears from it (HUB.md 3.8): the handful of
//! `HubRequest`s a client makes and the answers to them, as enums of their own. They are
//! the same requests and the hub handles them as such; what is separate is the encoding. A
//! client that speaks `HubRequest` carries the codecs of everything a zone and a moderator
//! can say as well, and in a browser that is paid for by everybody who downloads the page.
//!
//! On the wire a stream that speaks these begins with an empty frame (two zero bytes: no
//! `HubRequest` is empty, so the hub tells the two apart by that) and a frame of one byte,
//! the version of these messages. The hub answers with its own version in a frame of one
//! byte and, when the two are the same, with the response. A client of another build is
//! told so in a way no change of the messages can garble.

use bitcode::{Decode, Encode};
use gm_core::build::ContentPack;

use crate::protocol::{
    BuildChoice, CharacterId, CharacterSummary, EconOp, EconReply, HubError, HubRequest,
    HubResponse, ItemId, ItemSummary, ListingSummary, ModelId, SessionId, TradeOffer, ZoneId,
    ZoneSummary, ZoneTicket,
};

/// The version of the players' messages; any change to them, or to a type they carry, is
/// a new one.
pub const PLAYER_VERSION: u8 = 7;

/// What a stream that speaks the players' messages begins with: the empty frame, then
/// the version in a frame of its own.
pub const PREAMBLE: [u8; 5] = [0, 0, 0, 1, PLAYER_VERSION];

/// The hub's side of it: its version, in a frame of its own, before any answer.
pub const HUB_PREAMBLE: [u8; 3] = [0, 1, PLAYER_VERSION];

/// What to tell a person whose client and hub are of different builds.
pub fn version_words(hub: u8) -> String {
    if hub > PLAYER_VERSION {
        "this client is older than the hub it talks to: it needs an update".into()
    } else {
        "this client is newer than the hub it talks to".into()
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum PlayerRequest {
    Register {
        email: String,
        password: String,
    },
    Login {
        email: String,
        password: String,
    },
    Characters {
        session: SessionId,
    },
    CreateCharacter {
        session: SessionId,
        name: String,
        build: BuildChoice,
    },
    ListZones {
        session: SessionId,
    },
    Content {
        session: SessionId,
    },
    Enter {
        session: SessionId,
        character: CharacterId,
        zone: ZoneId,
    },
    Logout {
        session: SessionId,
    },
    /// Answered with `Blob { len }` followed by `len` raw bytes.
    ModelGet {
        session: SessionId,
        model: ModelId,
    },
    /// About what one of the session's characters owns (ITEMS.md 4).
    Econ {
        session: SessionId,
        character: CharacterId,
        op: PlayerEcon,
    },
}

/// The economy requests that have a screen (ITEMS.md 6): a few of `EconOp`, and the same
/// requests to the hub. Buying at a stall and wearing are not among them: those are said
/// to the zone, which knows where the body stands and whether it is in a fight.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum PlayerEcon {
    Inventory,
    /// The item bar (LOOK.md 3.2): the four cells' templates. v7.
    Bar,
    /// Arrange the item bar: four cells, a stack template or nothing each. v7.
    SetBar {
        cells: Vec<Option<String>>,
    },
    /// The account's storage, and an item into it or out of it.
    Storage,
    StorageDeposit {
        item: ItemId,
    },
    StorageWithdraw {
        item: ItemId,
    },
    StallView {
        stall: i64,
    },
    /// Into the character's own open stall, at a price in silver; and out of it again.
    StallList {
        item: ItemId,
        price: i64,
    },
    StallUnlist {
        listing: i64,
    },
    /// A trade the zone opened (PARTY.md 6): what it looks like, and the window's
    /// requests (ECONOMY.md 6). An accept names the version it saw.
    TradeView {
        trade: i64,
    },
    TradeOffer {
        trade: i64,
        item: ItemId,
    },
    TradeRetract {
        trade: i64,
        item: ItemId,
    },
    TradeCoin {
        trade: i64,
        coin: i64,
    },
    TradeAccept {
        trade: i64,
        version: i32,
    },
    TradeCancel {
        trade: i64,
    },
    /// The tavern (PARTY.md 7): who is for hire, the character's own hires and its own
    /// listing; hiring at the price shown, sending a hire away, listing and unlisting.
    Tavern,
    Hires,
    HireListed,
    Hire {
        avatar: CharacterId,
        price: i64,
    },
    Dismiss {
        hire: i64,
    },
    HireList {
        price: i64,
    },
    HireUnlist,
}

/// Somebody for hire, as the tavern page shows it: in the hub's words.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TavernRow {
    pub character: CharacterId,
    pub name: String,
    /// What it is (`ironclad: colossus in plate`), and the role a mind plays it in.
    pub what: String,
    pub role: String,
    pub price: i64,
    /// Hires in the last 12 h.
    pub hires: i64,
}

/// One of the character's hires: until when it runs, in unix seconds.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct HireRow {
    pub hire: i64,
    pub name: String,
    pub what: String,
    pub role: String,
    pub ends_at: u64,
}

/// The answers to them: the few of `EconReply` those requests have.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum PlayerEconReply {
    Done,
    Id(i64),
    /// The item bar's four cells. v7.
    Bar(Vec<Option<String>>),
    Holder {
        coin: i64,
        items: Vec<ItemSummary>,
    },
    Listings {
        owner: String,
        mine: bool,
        listings: Vec<ListingSummary>,
    },
    /// As `EconReply::TradeView`.
    TradeView {
        state: u8,
        version: i32,
        wait_ms: u32,
        with: String,
        together: bool,
        mine: TradeOffer,
        theirs: TradeOffer,
    },
    /// An accept was taken; `committed`: the other had accepted too, and it is done.
    Trade {
        committed: bool,
    },
    Tavern(Vec<TavernRow>),
    Hires(Vec<HireRow>),
}

impl From<PlayerEcon> for EconOp {
    fn from(op: PlayerEcon) -> EconOp {
        match op {
            PlayerEcon::Inventory => EconOp::Inventory,
            PlayerEcon::Bar => EconOp::Bar,
            PlayerEcon::SetBar { cells } => EconOp::SetBar { cells },
            PlayerEcon::Storage => EconOp::Storage,
            PlayerEcon::StorageDeposit { item } => EconOp::StorageDeposit { item },
            PlayerEcon::StorageWithdraw { item } => EconOp::StorageWithdraw { item },
            PlayerEcon::StallView { stall } => EconOp::StallView { stall },
            PlayerEcon::StallList { item, price } => EconOp::StallList { item, price },
            PlayerEcon::StallUnlist { listing } => EconOp::StallUnlist { listing },
            PlayerEcon::TradeView { trade } => EconOp::TradeView { trade },
            PlayerEcon::TradeOffer { trade, item } => EconOp::TradeOfferItem { trade, item },
            PlayerEcon::TradeRetract { trade, item } => EconOp::TradeRetractItem { trade, item },
            PlayerEcon::TradeCoin { trade, coin } => EconOp::TradeSetCoin { trade, coin },
            PlayerEcon::TradeAccept { trade, version } => EconOp::TradeAccept { trade, version },
            PlayerEcon::TradeCancel { trade } => EconOp::TradeCancel { trade },
            PlayerEcon::Tavern => EconOp::Tavern,
            PlayerEcon::Hires => EconOp::Squad,
            PlayerEcon::HireListed => EconOp::HireListed,
            PlayerEcon::Hire { avatar, price } => EconOp::Hire { avatar, price },
            PlayerEcon::Dismiss { hire } => EconOp::Dismiss { hire },
            PlayerEcon::HireList { price } => EconOp::HireList { price },
            PlayerEcon::HireUnlist => EconOp::HireUnlist,
        }
    }
}

impl TryFrom<EconReply> for PlayerEconReply {
    type Error = ();

    fn try_from(reply: EconReply) -> Result<PlayerEconReply, ()> {
        Ok(match reply {
            EconReply::Done => PlayerEconReply::Done,
            EconReply::Id(id) => PlayerEconReply::Id(id),
            EconReply::Bar(cells) => PlayerEconReply::Bar(cells),
            EconReply::Holder { coin, items } => PlayerEconReply::Holder { coin, items },
            EconReply::Listings {
                owner,
                mine,
                listings,
            } => PlayerEconReply::Listings {
                owner,
                mine,
                listings,
            },
            EconReply::TradeView {
                state,
                version,
                wait_ms,
                with,
                together,
                mine,
                theirs,
            } => PlayerEconReply::TradeView {
                state,
                version,
                wait_ms,
                with,
                together,
                mine,
                theirs,
            },
            EconReply::Trade { committed } => PlayerEconReply::Trade { committed },
            // What a screen shows of somebody for hire is the hub's words: the build
            // itself is the zone's to have.
            EconReply::Tavern(list) => PlayerEconReply::Tavern(
                list.into_iter()
                    .map(|t| TavernRow {
                        character: t.character,
                        name: t.name,
                        what: t.what,
                        role: t.role,
                        price: t.price,
                        hires: t.hires,
                    })
                    .collect(),
            ),
            EconReply::Squad(list) => PlayerEconReply::Hires(
                list.into_iter()
                    .map(|h| HireRow {
                        hire: h.hire,
                        name: h.name,
                        what: h.what,
                        role: h.role,
                        ends_at: h.expires_at,
                    })
                    .collect(),
            ),
            _ => return Err(()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum PlayerResponse {
    Ok,
    Err(HubError),
    Session {
        session: SessionId,
        account: i64,
    },
    Characters(Vec<CharacterSummary>),
    Character(CharacterSummary),
    Zones(Vec<ZoneSummary>),
    Content {
        pack: ContentPack,
        blurbs: Vec<String>,
    },
    Ticket(ZoneTicket),
    /// `len` raw bytes follow on the stream.
    Blob {
        len: u32,
    },
    Econ(PlayerEconReply),
}

impl From<PlayerRequest> for HubRequest {
    fn from(req: PlayerRequest) -> HubRequest {
        match req {
            PlayerRequest::Register { email, password } => HubRequest::Register { email, password },
            PlayerRequest::Login { email, password } => HubRequest::Login { email, password },
            PlayerRequest::Characters { session } => HubRequest::Characters { session },
            PlayerRequest::CreateCharacter {
                session,
                name,
                build,
            } => HubRequest::CreateCharacter {
                session,
                name,
                build,
            },
            PlayerRequest::ListZones { session } => HubRequest::ListZones { session },
            PlayerRequest::Content { session } => HubRequest::Content { session },
            PlayerRequest::Enter {
                session,
                character,
                zone,
            } => HubRequest::Enter {
                session,
                character,
                zone,
            },
            PlayerRequest::Logout { session } => HubRequest::Logout { session },
            PlayerRequest::ModelGet { session, model } => HubRequest::ModelGet { session, model },
            PlayerRequest::Econ {
                session,
                character,
                op,
            } => HubRequest::Econ {
                session,
                character,
                op: op.into(),
            },
        }
    }
}

impl TryFrom<HubResponse> for PlayerResponse {
    /// An answer no player's request has: the hub's mistake, not the client's.
    type Error = ();

    fn try_from(resp: HubResponse) -> Result<PlayerResponse, ()> {
        Ok(match resp {
            HubResponse::Ok => PlayerResponse::Ok,
            HubResponse::Err(e) => PlayerResponse::Err(e),
            HubResponse::Session { session, account } => {
                PlayerResponse::Session { session, account }
            }
            HubResponse::Characters(list) => PlayerResponse::Characters(list),
            HubResponse::Character(c) => PlayerResponse::Character(c),
            HubResponse::Zones(list) => PlayerResponse::Zones(list),
            HubResponse::Content { pack, blurbs } => PlayerResponse::Content { pack, blurbs },
            HubResponse::Ticket(t) => PlayerResponse::Ticket(t),
            HubResponse::Blob { len } => PlayerResponse::Blob { len },
            HubResponse::Econ(reply) => PlayerResponse::Econ(reply.try_into()?),
            _ => return Err(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_player_s_request_is_the_hub_s_request_and_no_hub_request_is_an_empty_frame() {
        let session = SessionId([3; 16]);
        let req = PlayerRequest::Enter {
            session,
            character: 9,
            zone: "town".into(),
        };
        assert_eq!(
            HubRequest::from(req),
            HubRequest::Enter {
                session,
                character: 9,
                zone: "town".into()
            }
        );
        // The marker of a player's stream is an empty frame: nothing the hub's own
        // messages can encode to.
        for req in [
            HubRequest::Logout { session },
            HubRequest::Characters { session },
            HubRequest::Register {
                email: String::new(),
                password: String::new(),
            },
        ] {
            assert!(!bitcode::encode(&req).is_empty());
        }
        assert_eq!(
            PlayerResponse::try_from(HubResponse::Blob { len: 7 }),
            Ok(PlayerResponse::Blob { len: 7 })
        );
        // The preambles are frames as the hub reads them: a length, then that many bytes.
        assert_eq!(PREAMBLE, [0, 0, 0, 1, PLAYER_VERSION]);
        assert_eq!(HUB_PREAMBLE, [0, 1, PLAYER_VERSION]);
        assert!(version_words(PLAYER_VERSION + 1).contains("older"));
        assert!(version_words(PLAYER_VERSION - 1).contains("newer"));
        // What only a zone or a moderator is ever told has no place here.
        assert_eq!(
            PlayerResponse::try_from(HubResponse::ReplayStored { id: 1 }),
            Err(())
        );
        // The economy requests with a screen are the hub's own; an answer none of them has
        // (a zone's, a tavern's) is not a player's.
        assert_eq!(
            HubRequest::from(PlayerRequest::Econ {
                session,
                character: 9,
                op: PlayerEcon::StorageDeposit { item: 4 },
            }),
            HubRequest::Econ {
                session,
                character: 9,
                op: EconOp::StorageDeposit { item: 4 },
            }
        );
        assert_eq!(
            PlayerResponse::try_from(HubResponse::Econ(EconReply::Id(3))),
            Ok(PlayerResponse::Econ(PlayerEconReply::Id(3)))
        );
        assert_eq!(
            PlayerResponse::try_from(HubResponse::Econ(EconReply::Stalls(Vec::new()))),
            Err(())
        );
        // The tavern's and the trade window's requests are the hub's own, and a hire's
        // price goes with it.
        assert_eq!(
            EconOp::from(PlayerEcon::Hire {
                avatar: 5,
                price: 120
            }),
            EconOp::Hire {
                avatar: 5,
                price: 120
            }
        );
        assert_eq!(
            EconOp::from(PlayerEcon::TradeAccept {
                trade: 3,
                version: 9
            }),
            EconOp::TradeAccept {
                trade: 3,
                version: 9
            }
        );
        assert_eq!(EconOp::from(PlayerEcon::Hires), EconOp::Squad);
    }
}
