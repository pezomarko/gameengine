//! What a character wears, over the hub protocol (ITEMS.md 2 to 4): wearing and taking off
//! through the zone the character plays in, what the zone is answered, a worn item refused
//! by everything that moves or destroys items, the claim, the players' messages and the
//! limit on them. Needs `GM_TEST_DATABASE_URL`; without it the test is skipped.

use std::path::Path;
use std::time::Duration;

use gm_core::matrix::Gear;
use gm_core::tick::TickRate;
use gm_core::vocab::DamageType;
use gm_hub::economy::{EconError, Economy, INVENTORY_SLOTS, MAX_PARTS, NOT_CARRIED, WORN};
use gm_hub::protocol::{
    BuildChoice, CharacterId, CharacterState, EconOp, EconReply, GearReading, HubError, HubRequest,
    HubResponse, ItemSummary, ListingSummary, PLACE_ARMOUR, PLACE_NONE, PLACE_WEAPON, SessionId,
    ZoneEconOp,
};
use gm_hub::{Db, HubClient, HubClientError, HubConfig, HubKey};
use gm_hub_proto::player::{PlayerEcon, PlayerEconReply, PlayerRequest, PlayerResponse};
use gm_net::transport::{Identity, hub_server_config};

const CONTENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/content");
const SECRET: &str = "items-test-secret";

async fn zone(addr: std::net::SocketAddr, cert: &[u8], id: &str) -> HubClient {
    let client = HubClient::connect_with_cert(addr, cert.to_vec())
        .await
        .unwrap();
    let r = client
        .request(&HubRequest::ZoneHello {
            secret: SECRET.into(),
            zone: id.into(),
            map: "test".into(),
            map_hash: 0,
            addr: "127.0.0.1:9".parse().unwrap(),
            cert_der: vec![1, 2, 3],
            web: None,
            min_trust: 0,
            requires: Vec::new(),
            max_players: 64,
        })
        .await
        .unwrap();
    assert!(matches!(r, HubResponse::Registered { .. }), "{r:?}");
    client
}

/// Enter `zone_id` and have the zone claim the character: what the claim said.
async fn enter(
    client: &HubClient,
    session: SessionId,
    character: CharacterId,
    zone_conn: &HubClient,
    zone_id: &str,
) -> (CharacterState, Gear) {
    let HubResponse::Ticket(ticket) = client
        .request(&HubRequest::Enter {
            session,
            character,
            zone: zone_id.into(),
        })
        .await
        .unwrap()
    else {
        panic!("enter")
    };
    match zone_conn
        .request(&HubRequest::Claim {
            token: ticket.token,
        })
        .await
        .unwrap()
    {
        HubResponse::Claimed { state, gear, .. } => (state, gear.gear),
        other => panic!("claim: {other:?}"),
    }
}

/// Register, create a character, enter `zone_id` and have the zone claim it.
async fn player(
    addr: std::net::SocketAddr,
    cert: &[u8],
    zone_conn: &HubClient,
    zone_id: &str,
    name: &str,
) -> (HubClient, SessionId, CharacterId, CharacterState) {
    let client = HubClient::connect_with_cert(addr, cert.to_vec())
        .await
        .unwrap();
    let HubResponse::Session { session, .. } = client
        .request(&HubRequest::Register {
            email: format!("{name}@example.test"),
            password: "correct horse battery".into(),
        })
        .await
        .unwrap()
    else {
        panic!("register")
    };
    let HubResponse::Character(c) = client
        .request(&HubRequest::CreateCharacter {
            session,
            name: name.into(),
            build: BuildChoice::Preset("blade".into()),
        })
        .await
        .unwrap()
    else {
        panic!("create")
    };
    let (state, gear) = enter(&client, session, c.id, zone_conn, zone_id).await;
    assert_eq!(gear, Gear::NONE, "a new character wears nothing");
    (client, session, c.id, state)
}

async fn econ(
    client: &HubClient,
    session: SessionId,
    character: CharacterId,
    op: EconOp,
) -> Result<EconReply, HubError> {
    match client
        .request(&HubRequest::Econ {
            session,
            character,
            op,
        })
        .await
    {
        Ok(HubResponse::Econ(r)) => Ok(r),
        Err(HubClientError::Refused(e)) => Err(e),
        other => panic!("unexpected {other:?}"),
    }
}

async fn zone_econ(client: &HubClient, op: ZoneEconOp) -> Result<EconReply, HubError> {
    match client.request(&HubRequest::ZoneEcon(op)).await {
        Ok(HubResponse::Econ(r)) => Ok(r),
        Err(HubClientError::Refused(e)) => Err(e),
        other => panic!("unexpected {other:?}"),
    }
}

async fn inventory(
    client: &HubClient,
    session: SessionId,
    character: CharacterId,
) -> Vec<ItemSummary> {
    match econ(client, session, character, EconOp::Inventory).await {
        Ok(EconReply::Holder { items, .. }) => items,
        other => panic!("inventory: {other:?}"),
    }
}

/// A zone puts an item on a character, or takes it off: the hub's reading of its gear
/// after that.
async fn change_gear(
    zone: &HubClient,
    character: CharacterId,
    item: i64,
    on: bool,
) -> Result<GearReading, HubError> {
    let op = if on {
        ZoneEconOp::Wear { character, item }
    } else {
        ZoneEconOp::TakeOff { character, item }
    };
    match zone_econ(zone, op).await? {
        EconReply::Gear(reading) => Ok(reading),
        other => panic!("wear: {other:?}"),
    }
}

async fn wear(zone: &HubClient, character: CharacterId, item: i64) -> Result<Gear, HubError> {
    change_gear(zone, character, item, true)
        .await
        .map(|r| r.gear)
}

async fn take_off(zone: &HubClient, character: CharacterId, item: i64) -> Result<Gear, HubError> {
    change_gear(zone, character, item, false)
        .await
        .map(|r| r.gear)
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

fn config(
    content: gm_core::build::ContentPack,
    items: &gm_content::items::ItemContent,
) -> HubConfig {
    HubConfig {
        zone_secret: SECRET.into(),
        content,
        key: HubKey::generate(),
        session_secs: 3600,
        auth_per_minute: 100.0,
        econ_per_second: 1000.0,
        party_sweep: std::time::Duration::from_millis(300),
        party_away: std::time::Duration::from_secs(2),
        items: items.clone(),
        looks: gm_content::looks::Looks::load_dir(Path::new(CONTENT)).expect("looks"),
        max_coin_grant: 500,
        models_dir: std::env::temp_dir().join(format!("gm-hub-items-{}", std::process::id())),
        ingest: gm_hub::IngestMode::InProcess,
        ingest_timeout: gm_hub::models::INGEST_TIMEOUT,
        start_zone: None,
        blurbs: Vec::new(),
    }
}

/// The two tests share one database: one at a time.
static DATABASE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_is_worn_is_the_hub_s_and_changes_through_the_zone() {
    let _database = DATABASE.lock().await;
    let Ok(url) = std::env::var("GM_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set GM_TEST_DATABASE_URL to a Postgres this test may wipe");
        return;
    };
    let db = Db::connect(&url).await.expect("database");
    db.migrate().await.expect("migrations");
    db.wipe().await.expect("wipe");
    let content = gm_content::load_dir(Path::new(CONTENT), TickRate::COMBAT).expect("content");
    let items = gm_content::items::load_items(Path::new(CONTENT)).expect("items");
    let identity = Identity::generate(&[gm_hub::HUB_SERVER_NAME, "localhost"]).unwrap();
    let cert = identity.cert_der().to_vec();
    let endpoint = quinn::Endpoint::server(
        hub_server_config(&identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let addr = endpoint.local_addr().unwrap();
    let _hub = tokio::spawn(gm_hub::run(
        config(content.clone(), &items),
        db.clone(),
        endpoint,
        std::future::pending(),
    ));
    let direct = Economy::new(db.pool().clone());

    let town = zone(addr, &cert, "town").await;
    let arena = zone(addr, &cert, "arena").await;
    let (smith_conn, smith_s, smith, smith_state) =
        player(addr, &cert, &town, "town", "Smith").await;
    let (other_conn, other_s, other, _) = player(addr, &cert, &town, "town", "Other").await;

    // An operator's hand (ITEMS.md 4): the best sword and the best cuirass the content
    // allows, a plain sword, and a sword for somebody else. A grant obeys a craft's rule
    // and the template's room.
    let best = strings(&[
        "shard/boss_scale",
        "core/dragonbone",
        "catalyst/ember",
        "frame/whalebone",
        "gem/opal",
        "gem/opal",
    ]);
    let iron = strings(&["core/iron", "frame/oak"]);
    let room = |template: &str| items.layers(template).map(|l| l.to_vec());
    let sword = direct
        .grant_item(smith, "sword", &best, room("sword").as_deref())
        .await
        .unwrap();
    let plain = direct
        .grant_item(smith, "sword", &iron, room("sword").as_deref())
        .await
        .unwrap();
    let cuirass = direct
        .grant_item(
            smith,
            "cuirass",
            &strings(&[
                "shard/boss_scale",
                "core/dragonbone",
                "frame/whalebone",
                "gem/opal",
                "gem/opal",
            ]),
            room("cuirass").as_deref(),
        )
        .await
        .unwrap();
    let theirs = direct
        .grant_item(other, "sword", &iron, room("sword").as_deref())
        .await
        .unwrap();
    assert!(matches!(
        direct
            .grant_item(smith, "sword", &strings(&["core/iron"]), None)
            .await,
        Err(EconError::Invalid(_))
    ));
    assert_eq!(
        direct
            .grant_item(smith, "cuirass", &best, room("cuirass").as_deref())
            .await,
        Err(EconError::Invalid("a cuirass takes no catalyst".into()))
    );
    assert_eq!(
        direct.grant_item(smith + 99, "sword", &iron, None).await,
        Err(EconError::NotFound)
    );
    let EconReply::Ids(parts) = zone_econ(
        &town,
        ZoneEconOp::GrantComponents {
            grants: vec![
                (smith, "catalyst/ember".to_string()),
                (smith, "core/tin".to_string()),
                (smith, "frame/oak".to_string()),
            ],
            reference: 1,
        },
    )
    .await
    .unwrap() else {
        panic!("grant")
    };
    let ember = parts[0];
    // A craft over the wire obeys the template's room too.
    assert_eq!(
        econ(
            &smith_conn,
            smith_s,
            smith,
            EconOp::Craft {
                template: "cuirass".into(),
                components: parts.clone()
            }
        )
        .await,
        Err(HubError::Invalid("a cuirass takes no catalyst".into()))
    );

    // More parts than an item has places for: refused before the hub looks at any of them
    // (each would cost it statements, and the ids need not even exist).
    assert_eq!(
        econ(
            &smith_conn,
            smith_s,
            smith,
            EconOp::Craft {
                template: "sword".into(),
                components: (1..=MAX_PARTS as i64 + 1).map(|i| 1_000_000 + i).collect()
            }
        )
        .await,
        Err(HubError::Invalid("too many components".into()))
    );

    // What an item is and does is the hub's to say (ITEMS.md 3.2), numbers and words: no
    // client works either out.
    let at = |t: DamageType| t as usize;
    let mut sword_edge = [0u16; 9];
    sword_edge[at(DamageType::Slash)] = 220;
    sword_edge[at(DamageType::Fire)] = 190;
    let cuirass_edge = [220u16, 220, 220, 0, 0, 0, 0, 0, 0];
    let plain_edge = [40u16, 0, 0, 0, 0, 0, 0, 0, 0];
    let have = inventory(&smith_conn, smith_s, smith).await;
    let find = |list: &[ItemSummary], id: i64| list.iter().find(|i| i.id == id).unwrap().clone();
    assert_eq!(have.len(), 6);
    let shown = find(&have, sword);
    assert_eq!((shown.place, shown.edge), (PLACE_WEAPON, sword_edge));
    assert_eq!(shown.what, "a weapon, 250 of 250");
    assert_eq!(shown.does, ["slash +11.0%", "fire +9.5%"]);
    let shown = find(&have, cuirass);
    assert_eq!((shown.place, shown.edge), (PLACE_ARMOUR, cuirass_edge));
    assert_eq!(shown.what, "an armour, 220 of 250");
    assert_eq!(shown.does, ["physical -9.9%"]);
    assert_eq!(find(&have, plain).edge, plain_edge);
    let shown = find(&have, ember);
    assert_eq!((shown.place, shown.edge), (PLACE_NONE, [0; 9]));
    assert_eq!(shown.what, "a catalyst, for crafting");
    assert!(shown.does.is_empty());
    assert!(have.iter().all(|i| !i.worn));

    // Wearing is asked for by the zone the character plays in (ITEMS.md 2), and answered
    // with what its gear does from then on.
    let gear = wear(&town, smith, sword).await.unwrap();
    assert_eq!((gear.dealt, gear.taken), (sword_edge, [0; 9]));
    let gear = wear(&town, smith, cuirass).await.unwrap();
    assert_eq!((gear.dealt, gear.taken), (sword_edge, cuirass_edge));
    // Twice is once.
    assert_eq!(wear(&town, smith, cuirass).await, Ok(gear));
    let have = inventory(&smith_conn, smith_s, smith).await;
    assert_eq!(have.len(), 6, "what is worn stays in the inventory");
    assert!(find(&have, sword).worn && find(&have, cuirass).worn);
    assert!(!find(&have, plain).worn && !find(&have, ember).worn);
    // Only that zone: another speaks for nobody here, and a session's connection is no zone.
    assert_eq!(
        wear(&arena, smith, plain).await,
        Err(HubError::Unauthorized)
    );
    assert_eq!(
        take_off(&arena, smith, sword).await,
        Err(HubError::Unauthorized)
    );
    assert_eq!(
        wear(&smith_conn, smith, plain).await,
        Err(HubError::Unauthorized)
    );

    // Worn is not for sale (ITEMS.md 1): everything that moves or destroys an item refuses.
    let worn = || Err(HubError::Invalid(WORN.into()));
    let smiths = |op: EconOp| econ(&smith_conn, smith_s, smith, op);
    assert_eq!(smiths(EconOp::StorageDeposit { item: sword }).await, worn());
    assert_eq!(smiths(EconOp::Decompose { item: sword }).await, worn());
    let open = ZoneEconOp::TradeOpen { a: smith, b: other };
    let EconReply::Id(trade) = zone_econ(&town, open).await.unwrap() else {
        panic!("trade")
    };
    assert_eq!(
        smiths(EconOp::TradeOfferItem { trade, item: sword }).await,
        worn()
    );
    let EconReply::Stall(stall) = zone_econ(
        &town,
        ZoneEconOp::StallOpen {
            character: smith,
            tile_x: 1,
            tile_y: 1,
        },
    )
    .await
    .unwrap() else {
        panic!("stall")
    };
    assert_eq!(
        smiths(EconOp::StallList {
            item: cuirass,
            price: 5
        })
        .await,
        worn()
    );
    assert_eq!(
        zone_econ(
            &town,
            ZoneEconOp::Drop {
                character: smith,
                item: sword
            }
        )
        .await,
        worn()
    );
    assert!(matches!(
        smiths(EconOp::Craft {
            template: "sword".into(),
            components: vec![sword, ember]
        })
        .await,
        Err(HubError::Invalid(_))
    ));
    // The database is the last line: whatever code moved or deleted it would be refused.
    let moved = sqlx::query(
        "update items set holder_id = (select id from holders where kind = 'sink') where id = $1",
    )
    .bind(sword)
    .execute(db.pool())
    .await;
    assert!(
        matches!(&moved, Err(sqlx::Error::Database(d)) if d.code().as_deref() == Some("GM001")),
        "{moved:?}"
    );
    let deleted = sqlx::query("delete from items where id = $1")
        .bind(sword)
        .execute(db.pool())
        .await;
    assert!(
        matches!(&deleted, Err(sqlx::Error::Database(d)) if d.is_foreign_key_violation()),
        "{deleted:?}"
    );

    // What cannot be worn, and whose.
    assert_eq!(
        wear(&town, smith, ember).await,
        Err(HubError::Invalid("that cannot be worn".into()))
    );
    assert_eq!(
        wear(&town, smith, theirs).await,
        Err(HubError::Invalid(NOT_CARRIED.into()))
    );
    assert_eq!(
        wear(&town, smith, sword + 1_000).await,
        Err(HubError::NotFound)
    );
    assert_eq!(
        wear(&town, other, sword).await,
        Err(HubError::Invalid(NOT_CARRIED.into()))
    );

    // A weapon the build's hands do not hold (ITEMS.md 2): the blade swings a sword, so a
    // staff is refused in words, and the inventory says so of it before anyone asks.
    let staff = direct
        .grant_item(smith, "staff", &iron, room("staff").as_deref())
        .await
        .unwrap();
    let words = "this build's hands are for the sword: not a staff";
    assert_eq!(
        wear(&town, smith, staff).await,
        Err(HubError::Invalid(words.into()))
    );
    let have = inventory(&smith_conn, smith_s, smith).await;
    let shown = find(&have, staff);
    assert!(!shown.fits && !shown.worn, "{shown:?}");
    assert_eq!(shown.does.last().map(String::as_str), Some(words));
    assert!(find(&have, plain).fits && find(&have, cuirass).fits);

    // One weapon: the plain sword takes the best one's place, and the best one is an
    // item like any other again.
    let gear = wear(&town, smith, plain).await.unwrap();
    assert_eq!((gear.dealt, gear.taken), (plain_edge, cuirass_edge));
    let have = inventory(&smith_conn, smith_s, smith).await;
    assert!(find(&have, plain).worn && !find(&have, sword).worn);
    assert_eq!(
        smiths(EconOp::StorageDeposit { item: sword }).await,
        Ok(EconReply::Done)
    );
    assert_eq!(
        smiths(EconOp::StorageWithdraw { item: sword }).await,
        Ok(EconReply::Done)
    );

    // Offered and then worn: it leaves the offer, the offer says it changed, and an
    // accept of what was shown before is refused.
    assert_eq!(
        smiths(EconOp::TradeOfferItem { trade, item: sword }).await,
        Ok(EconReply::Done)
    );
    let view = || async {
        match smiths(EconOp::TradeView { trade }).await.unwrap() {
            EconReply::TradeView { version, mine, .. } => (version, mine.items.len()),
            other => panic!("{other:?}"),
        }
    };
    let (offered, n) = view().await;
    assert_eq!(n, 1);
    let gear = wear(&town, smith, sword).await.unwrap();
    assert_eq!(gear.dealt, sword_edge);
    let (now, n) = view().await;
    assert!(
        n == 0 && now > offered,
        "{n} items, version {offered} then {now}"
    );
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    assert_eq!(
        econ(
            &other_conn,
            other_s,
            other,
            EconOp::TradeAccept {
                trade,
                version: offered
            }
        )
        .await,
        Err(HubError::Invalid("the offer changed".into()))
    );

    // Taking off: by the item, and taking off what is not worn changes nothing.
    let gear = take_off(&town, smith, sword).await.unwrap();
    assert_eq!((gear.dealt, gear.taken), ([0; 9], cuirass_edge));
    assert_eq!(take_off(&town, smith, sword).await, Ok(gear));
    assert_eq!(take_off(&town, smith, ember).await, Ok(gear));
    assert_eq!(take_off(&town, smith, theirs).await, Ok(gear));
    let gear = wear(&town, smith, sword).await.unwrap();
    assert_eq!((gear.dealt, gear.taken), (sword_edge, cuirass_edge));

    // The players' messages (HUB.md 3.8): what the screens ask, in the client's own
    // encoding. Wearing is not among them.
    let players = |op: PlayerEcon| {
        let conn = smith_conn.clone();
        async move {
            match conn
                .player(&PlayerRequest::Econ {
                    session: smith_s,
                    character: smith,
                    op,
                })
                .await
            {
                Ok(PlayerResponse::Econ(reply)) => Ok(reply),
                Err(HubClientError::Refused(e)) => Err(e),
                other => panic!("unexpected {other:?}"),
            }
        }
    };
    match players(PlayerEcon::Inventory).await.unwrap() {
        PlayerEconReply::Holder { coin, items } => {
            assert_eq!(coin, 0);
            assert!(find(&items, sword).worn && find(&items, cuirass).worn);
        }
        other => panic!("{other:?}"),
    }
    // A listing, seen from anywhere, and taken back.
    let listed = |listing: i64| PlayerEconReply::Listings {
        owner: "Smith".into(),
        mine: true,
        listings: vec![ListingSummary {
            id: listing,
            item: ItemSummary {
                worn: false,
                ..find(&have, plain)
            },
            price: 12_345,
        }],
    };
    let PlayerEconReply::Id(listing) = players(PlayerEcon::StallList {
        item: plain,
        price: 12_345,
    })
    .await
    .unwrap() else {
        panic!("list")
    };
    assert_eq!(
        players(PlayerEcon::StallView { stall: stall.id }).await,
        Ok(listed(listing))
    );
    assert_eq!(
        players(PlayerEcon::StallUnlist { listing }).await,
        Ok(PlayerEconReply::Done)
    );
    assert_eq!(
        players(PlayerEcon::StallUnlist { listing }).await,
        Err(HubError::NotFound)
    );
    let have = inventory(&smith_conn, smith_s, smith).await;
    assert!(have.iter().any(|i| i.id == plain), "back in the inventory");
    // The storage: in, seen there, and out again.
    assert_eq!(
        players(PlayerEcon::StorageDeposit { item: plain }).await,
        Ok(PlayerEconReply::Done)
    );
    match players(PlayerEcon::Storage).await.unwrap() {
        PlayerEconReply::Holder { items, .. } => {
            assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![plain])
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        players(PlayerEcon::StorageWithdraw { item: plain }).await,
        Ok(PlayerEconReply::Done)
    );
    let PlayerEconReply::Id(listing) = players(PlayerEcon::StallList {
        item: plain,
        price: 12_345,
    })
    .await
    .unwrap() else {
        panic!("list again")
    };

    // On its way to another zone a character is nobody's to change (ITEMS.md 3.3): the
    // zone it leaves is refused, so what its ghost fights in is what the hub holds; and
    // the zone it arrives in is told what it wears, read after it became that zone's.
    let HubResponse::Ticket(ticket) = town
        .request(&HubRequest::Handoff {
            character: smith,
            state: smith_state.clone(),
            to_zone: "arena".into(),
            web: false,
        })
        .await
        .unwrap()
    else {
        panic!("handoff")
    };
    assert_eq!(
        take_off(&town, smith, sword).await,
        Err(HubError::Unauthorized)
    );
    assert_eq!(
        take_off(&arena, smith, sword).await,
        Err(HubError::Unauthorized),
        "not there yet"
    );
    let claimed = match arena
        .request(&HubRequest::Claim {
            token: ticket.token,
        })
        .await
        .unwrap()
    {
        HubResponse::Claimed { gear, .. } => gear,
        other => panic!("claim: {other:?}"),
    };
    assert_eq!(
        (claimed.gear.dealt, claimed.gear.taken),
        (sword_edge, cuirass_edge)
    );
    assert_eq!(
        take_off(&town, smith, sword).await,
        Err(HubError::Unauthorized)
    );
    let gear = take_off(&arena, smith, cuirass).await.unwrap();
    assert_eq!((gear.dealt, gear.taken), (sword_edge, [0; 9]));
    // Its stall stands in the town: from the arena it neither lists nor unlists there.
    assert_eq!(
        players(PlayerEcon::StallList {
            item: cuirass,
            price: 5
        })
        .await,
        Err(HubError::NotFound)
    );
    assert_eq!(
        players(PlayerEcon::StallUnlist { listing }).await,
        Err(HubError::NotFound)
    );

    // Readings are numbered, and the largest number is the truth (ITEMS.md 3.3). The
    // character joins the arena again and again (a claim by the zone it is in already)
    // while that same zone puts the cuirass on and takes it off: whatever order the hub
    // does them in, the reading with the largest number says what the hub holds.
    let mut both = 0;
    for round in 0..32u64 {
        let saved = arena
            .request(&HubRequest::Save {
                character: smith,
                state: smith_state.clone(),
                leaving: true,
            })
            .await
            .unwrap();
        assert_eq!(saved, HubResponse::Ok);
        let HubResponse::Ticket(ticket) = smith_conn
            .request(&HubRequest::Enter {
                session: smith_s,
                character: smith,
                zone: "arena".into(),
            })
            .await
            .unwrap()
        else {
            panic!("enter")
        };
        let claim = {
            let arena = arena.clone();
            tokio::spawn(async move {
                match arena
                    .request(&HubRequest::Claim {
                        token: ticket.token,
                    })
                    .await
                {
                    Ok(HubResponse::Claimed { gear, .. }) => gear,
                    other => panic!("claim: {other:?}"),
                }
            })
        };
        let change = {
            let arena = arena.clone();
            tokio::spawn(async move {
                // Sometimes before the claim, sometimes after, sometimes across it.
                tokio::time::sleep(Duration::from_micros(200 * (round % 8))).await;
                change_gear(&arena, smith, cuirass, round % 2 == 0).await
            })
        };
        let mut readings = vec![claim.await.unwrap()];
        // The change is refused while the character is still on its way (the claim had
        // not happened yet): then only the claim's reading exists.
        if let Ok(reading) = change.await.unwrap() {
            readings.push(reading);
            both += 1;
        }
        let latest = readings.iter().max_by_key(|r| r.seq).unwrap();
        let (_, held, _, _, _) = direct.gear(smith, &items).await.unwrap();
        assert_eq!(
            latest.gear, held,
            "round {round}: the readings {readings:?} against what the hub holds"
        );
    }
    assert!(both > 0, "no change ever went through beside a claim");

    // An operator's grant into a full inventory is refused: nothing goes to the ground.
    let have = inventory(&other_conn, other_s, other).await.len();
    for _ in have..INVENTORY_SLOTS as usize {
        direct
            .grant_item(other, "sword", &iron, None)
            .await
            .unwrap();
    }
    assert_eq!(
        direct.grant_item(other, "sword", &iron, None).await,
        Err(EconError::Full)
    );
    assert_eq!(
        inventory(&other_conn, other_s, other).await.len(),
        INVENTORY_SLOTS as usize
    );

    // The operator's coin is in the ledger as a grant, and the books are sound: every
    // balance, and everything worn in its wearer's own inventory and in no offer.
    direct.grant_coin_as(smith, 500, "grant", 0).await.unwrap();
    let grants: i64 = sqlx::query_scalar("select count(*) from coin_ledger where reason = 'grant'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let made: i64 = sqlx::query_scalar("select count(*) from item_moves where reason = 'grant'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    // (Four made for the test, and the swords that filled the other inventory.)
    assert_eq!(grants, 1);
    assert!(made >= 4, "{made}");
    assert_eq!(direct.audit().await, Ok(0));
    // The audit is not blind: a worn item put elsewhere behind the hub's back counts.
    sqlx::query("alter table items disable trigger worn_stays")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "update items set holder_id = (select id from holders where kind = 'storage' limit 1) where id = $1",
    )
    .bind(sword)
    .execute(db.pool())
    .await
    .unwrap();
    assert_eq!(direct.audit().await, Ok(1));
    sqlx::query(
        "update items set holder_id = (select id from holders where kind = 'character' and character_id = $2) where id = $1",
    )
    .bind(sword)
    .bind(smith)
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query("alter table items enable trigger worn_stays")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(direct.audit().await, Ok(0));

    // A session is all it takes to ask the economy something, so an account may ask only
    // so often (ITEMS.md 4): a hub that allows two a second lets eight through at once
    // and then says it is busy.
    let identity = Identity::generate(&[gm_hub::HUB_SERVER_NAME, "localhost"]).unwrap();
    let cert2 = identity.cert_der().to_vec();
    let endpoint = quinn::Endpoint::server(
        hub_server_config(&identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let addr2 = endpoint.local_addr().unwrap();
    let _slow = tokio::spawn(gm_hub::run(
        HubConfig {
            econ_per_second: 2.0,
            party_sweep: std::time::Duration::from_millis(300),
            party_away: std::time::Duration::from_secs(2),
            ..config(content, &items)
        },
        db.clone(),
        endpoint,
        std::future::pending(),
    ));
    let client = HubClient::connect_with_cert(addr2, cert2).await.unwrap();
    let HubResponse::Session { session, .. } = client
        .request(&HubRequest::Login {
            email: "Other@example.test".into(),
            password: "correct horse battery".into(),
        })
        .await
        .unwrap()
    else {
        panic!("login")
    };
    let mut answers = Vec::new();
    for _ in 0..12 {
        answers.push(
            econ(&client, session, other, EconOp::Inventory)
                .await
                .is_ok(),
        );
    }
    assert!(answers[..8].iter().all(|ok| *ok), "{answers:?}");
    assert!(!answers[9] && !answers[11], "{answers:?}");
    assert_eq!(
        econ(&client, session, other, EconOp::Inventory).await,
        Err(HubError::Busy)
    );
}

/// Stacks (MODES.md 11): a grant lands on the stack carried and stops at the cap; the
/// inventory says how many; the hub's reading carries the stacks; a zone's `Consume` lowers
/// them and answers the reading; a stall sells a stack onto the buyer's, and past the
/// buyer's cap the sale is refused in words with nothing moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stacks_merge_to_the_cap_and_are_spent_through_the_zone() {
    let _database = DATABASE.lock().await;
    let Ok(url) = std::env::var("GM_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set GM_TEST_DATABASE_URL to a Postgres this test may wipe");
        return;
    };
    let db = Db::connect(&url).await.expect("database");
    db.migrate().await.expect("migrations");
    db.wipe().await.expect("wipe");
    let content = gm_content::load_dir(Path::new(CONTENT), TickRate::COMBAT).expect("content");
    let items = gm_content::items::load_items(Path::new(CONTENT)).expect("items");
    let identity = Identity::generate(&[gm_hub::HUB_SERVER_NAME, "localhost"]).unwrap();
    let cert = identity.cert_der().to_vec();
    let endpoint = quinn::Endpoint::server(
        hub_server_config(&identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let addr = endpoint.local_addr().unwrap();
    let _hub = tokio::spawn(gm_hub::run(
        config(content.clone(), &items),
        db.clone(),
        endpoint,
        std::future::pending(),
    ));
    let direct = Economy::new(db.pool().clone()).with_stacks(&items);

    let town = zone(addr, &cert, "town").await;
    let (smith_conn, smith_s, smith, _) = player(addr, &cert, &town, "town", "Smith").await;
    let (other_conn, other_s, other, _) = player(addr, &cert, &town, "town", "Other").await;

    // A grant lands on the stack carried, up to the cap of 30 balls.
    let balls = direct.grant_stack(smith, "ball", 10).await.unwrap();
    assert_eq!(direct.grant_stack(smith, "ball", 15).await.unwrap(), balls);
    assert_eq!(
        direct.grant_stack(smith, "ball", 10).await,
        Err(EconError::Invalid(gm_hub::economy::at_the_cap("ball")))
    );
    let kits = direct.grant_stack(smith, "kit", 2).await.unwrap();
    assert!(matches!(
        direct.grant_stack(smith, "sword", 1).await,
        Err(EconError::Invalid(_))
    ));
    let EconReply::Holder { items: carried, .. } =
        econ(&smith_conn, smith_s, smith, EconOp::Inventory)
            .await
            .unwrap()
    else {
        panic!("inventory")
    };
    assert_eq!(carried.len(), 2, "one row a stack: {carried:?}");
    let ball = carried.iter().find(|i| i.id == balls).unwrap();
    assert_eq!((ball.quantity, ball.cap), (25, 30));
    assert_eq!(ball.what, "ball ×25 of 30");
    assert_eq!(ball.place, PLACE_NONE);
    let kit = carried.iter().find(|i| i.id == kits).unwrap();
    assert_eq!((kit.quantity, kit.cap), (2, 5));
    assert_eq!(kit.does, vec!["heals 300, used with F".to_string()]);
    assert_eq!(
        ball.does,
        vec!["loads the Musket: carried is loaded, R reloads".to_string()],
        "a stack of rounds says which gun it loads (MODES.md 10.2)"
    );

    // The reading carries the stacks, as a zone takes them, and the item bar (LOOK.md
    // 3.2): never arranged, it is the kit on the first cell.
    let (_, _, _, stacks, bar) = direct.gear(smith, &items).await.unwrap();
    let find = |stacks: &[gm_hub::economy::Stack], t: &str| {
        stacks
            .iter()
            .find(|s| s.template == t)
            .map(|s| (s.quantity, s.heals))
    };
    assert_eq!(find(&stacks, "ball"), Some((25, None)));
    assert_eq!(find(&stacks, "kit"), Some((2, Some(300))));
    let kit_first = vec![Some("kit".to_string()), None, None, None];
    assert_eq!(bar, kit_first);
    assert_eq!(
        econ(&smith_conn, smith_s, smith, EconOp::Bar).await,
        Ok(EconReply::Bar(kit_first.clone()))
    );
    let (_, _, _, _, bar) = direct.gear(other, &items).await.unwrap();
    assert_eq!(bar, vec![None; 4], "nothing carried, nothing on the bar");
    // Arranged by the player: kept as set, even for a stack not carried; gear is refused,
    // and so is a bar of another size.
    let arranged = vec![
        None,
        Some("kit".to_string()),
        None,
        Some("ball".to_string()),
    ];
    assert_eq!(
        econ(
            &smith_conn,
            smith_s,
            smith,
            EconOp::SetBar {
                cells: arranged.clone()
            }
        )
        .await,
        Ok(EconReply::Done)
    );
    assert_eq!(
        econ(&smith_conn, smith_s, smith, EconOp::Bar).await,
        Ok(EconReply::Bar(arranged.clone()))
    );
    let (_, _, _, _, bar) = direct.gear(smith, &items).await.unwrap();
    assert_eq!(bar, arranged);
    assert!(matches!(
        econ(
            &smith_conn,
            smith_s,
            smith,
            EconOp::SetBar {
                cells: vec![Some("sword".to_string()), None, None, None]
            }
        )
        .await,
        Err(HubError::Invalid(_))
    ));
    assert!(matches!(
        econ(
            &smith_conn,
            smith_s,
            smith,
            EconOp::SetBar {
                cells: vec![None, None, None]
            }
        )
        .await,
        Err(HubError::Invalid(_))
    ));
    assert!(
        matches!(
            econ(
                &smith_conn,
                smith_s,
                smith,
                EconOp::SetBar {
                    cells: vec![Some("kit".to_string()), Some("kit".to_string()), None, None]
                }
            )
            .await,
            Err(HubError::Invalid(_))
        ),
        "a kind sits on one cell"
    );
    let cleared = vec![None; 4];
    assert_eq!(
        econ(
            &smith_conn,
            smith_s,
            smith,
            EconOp::SetBar {
                cells: cleared.clone()
            }
        )
        .await,
        Ok(EconReply::Done)
    );
    assert_eq!(
        econ(&smith_conn, smith_s, smith, EconOp::Bar).await,
        Ok(EconReply::Bar(cleared)),
        "an emptied bar stays empty: the default is for a bar never arranged"
    );
    assert_eq!(
        econ(
            &smith_conn,
            smith_s,
            smith,
            EconOp::SetBar {
                cells: kit_first.clone()
            }
        )
        .await,
        Ok(EconReply::Done)
    );

    // The zone spent rounds: the hub follows and answers the reading after. More than is
    // carried: refused (logged), and the reading says what is.
    let consume = |item: i64, quantity: u32| {
        zone_econ(
            &town,
            ZoneEconOp::Consume {
                character: smith,
                item,
                quantity,
            },
        )
    };
    let EconReply::Gear(reading) = consume(balls, 5).await.unwrap() else {
        panic!("consume")
    };
    let read = |r: &GearReading, t: &str| {
        r.stacks
            .iter()
            .find(|s| s.template == t)
            .map(|s| s.quantity)
    };
    assert_eq!(read(&reading, "ball"), Some(20));
    let EconReply::Gear(reading) = consume(balls, 25).await.unwrap() else {
        panic!("consume")
    };
    assert_eq!(
        read(&reading, "ball"),
        Some(20),
        "nothing moved past what is carried"
    );
    let EconReply::Gear(reading) = consume(balls, 20).await.unwrap() else {
        panic!("consume")
    };
    assert_eq!(read(&reading, "ball"), None, "a stack spent is gone");
    assert_eq!(read(&reading, "kit"), Some(2));

    // The quartermaster: a stack listed at a stall goes onto the buyer's stack; past the
    // buyer's cap the sale is refused before any coin moves.
    let EconReply::Stall(stall) = zone_econ(
        &town,
        ZoneEconOp::StallOpen {
            character: other,
            tile_x: 1,
            tile_y: 1,
        },
    )
    .await
    .unwrap() else {
        panic!("stall")
    };
    let first = direct.grant_stack(other, "pistol_round", 16).await.unwrap();
    let EconReply::Id(listing) = econ(
        &other_conn,
        other_s,
        other,
        EconOp::StallList {
            item: first,
            price: 1,
        },
    )
    .await
    .unwrap() else {
        panic!("list")
    };
    // A second stack for the stall: the first is listed (its holder is the stall's), so
    // the grant makes a new row, which is then listed too.
    let second = direct.grant_stack(other, "pistol_round", 16).await.unwrap();
    assert_ne!(first, second);
    let EconReply::Id(listing2) = econ(
        &other_conn,
        other_s,
        other,
        EconOp::StallList {
            item: second,
            price: 1,
        },
    )
    .await
    .unwrap() else {
        panic!("list")
    };
    direct.grant_coin(smith, 100, 0).await.unwrap();
    direct.grant_stack(smith, "pistol_round", 40).await.unwrap();
    let buy = |listing: i64| {
        zone_econ(
            &town,
            ZoneEconOp::StallBuy {
                character: smith,
                stall: stall.id,
                listing,
                price: 1,
            },
        )
    };
    let EconReply::Gear(reading) = buy(listing).await.unwrap() else {
        panic!("buy")
    };
    assert_eq!(read(&reading, "pistol_round"), Some(56), "16 onto 40");
    assert_eq!(
        reading
            .stacks
            .iter()
            .filter(|s| s.template == "pistol_round")
            .count(),
        1,
        "one row"
    );
    assert_eq!(
        buy(listing2).await,
        Err(HubError::Invalid(gm_hub::economy::at_the_cap(
            "pistol_round"
        ))),
        "56 + 16 is past 64"
    );
    let EconReply::Holder { coin, .. } = econ(&smith_conn, smith_s, smith, EconOp::Inventory)
        .await
        .unwrap()
    else {
        panic!("inventory")
    };
    assert_eq!(coin, 99, "one sale paid, the refused one not");
    let audit = direct.audit().await.unwrap();
    assert_eq!(audit, 0, "the books balance");
}
