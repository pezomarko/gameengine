//! Phase 11 over the real protocol (ITEMS.md 5, 7): a hub, the town zone and clients driven
//! by hand. A stall is bought from only by a body standing at it; what is worn changes
//! through the zone, at once, and not in a fight; and the zone's hits show the edge, before
//! and after, to the point. Needs `GM_TEST_DATABASE_URL` (a Postgres the test may wipe);
//! without it the test is skipped. The gun mode's hand (MODES.md 3.7) is pinned here too:
//! the weapon switched to is what everyone is told the body holds; and so is the build
//! worn at the trainer (MATRIX.md 9.1): the hub holds it before the zone says so, and it
//! is what the character wears when it enters again.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use glam::Vec3;
use gm_bsp::Bsp;
use gm_core::build::{Build, Sheet};
use gm_core::sim::{Input, buttons};
use gm_core::tick::TickRate;
use gm_core::trace::{CollisionWorld, Hull};
use gm_hub::economy::Economy;
use gm_hub::protocol::{
    BuildChoice, CharacterId, EconOp, EconReply, HubRequest, HubResponse, ItemSummary, SessionId,
    ZoneTicket,
};
use gm_hub::{Db, HubClient, HubConfig, HubKey, IngestMode};
use gm_net::PROTOCOL_VERSION;
use gm_net::client::ClientState;
use gm_net::control::{self, FromClient, FromZone, StallEntry, TRAINER_REACH};
use gm_net::transport::{Identity, SERVER_NAME, client_config, hub_server_config, server_config};
use gm_server::{HubLink, HubLinkConfig, ZoneConfig, ZoneWorld};
use quinn::rustls::pki_types::CertificateDer;

const TOWN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/maps/built/town.bsp"
);
const ARENA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/maps/built/arena.bsp"
);
const CONTENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/content");
const SECRET: &str = "items-zone-secret";
const PASSWORD: &str = "correct horse battery";
/// The fight lock of this test's zone: long enough to run into on a machine that is doing
/// other things, short enough to wait out.
const LOCK: Duration = Duration::from_secs(3);
const FIGHT: &str = "not in a fight: wait a moment";
const GATE: &str = "one thing at a time: try again in a moment";

/// What the test tells a client to do, and what the client has seen.
#[derive(Default)]
struct Shared {
    /// The body's own id in the zone, and the build the zone gave it on arrival.
    id: u32,
    own: Option<Build>,
    yaw: f32,
    buttons: u16,
    /// The gun mode's hand (MODES.md 3.7): 0 the gun, 1 the pistol, 2 the knife.
    held: u8,
    say: Vec<FromClient>,
    health: i32,
    alive: bool,
    /// The item bar the own block says it carries (LOOK.md 3.2), and the cell in use
    /// (MODES.md 11.3).
    bar: [u16; gm_core::sim::BAR_CELLS],
    using: Option<u8>,
    /// The cell the next frames' `USE` names (1-based).
    use_slot: u8,
    synced: bool,
    heard: Vec<FromZone>,
    stalls: Vec<StallEntry>,
}

/// A client driven by the test: it stands where it was put, looks where it is told,
/// holds the buttons it is told to and says to the zone what it is told to.
struct Hand {
    shared: Arc<Mutex<Shared>>,
    task: tokio::task::JoinHandle<()>,
}

impl Hand {
    async fn join(ticket: &ZoneTicket, name: &str, yaw: f32, world: Arc<Bsp>) -> Hand {
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(
            client_config(&[CertificateDer::from(ticket.cert_der.clone())]).unwrap(),
        );
        let conn = endpoint
            .connect(ticket.addr, SERVER_NAME)
            .unwrap()
            .await
            .expect("the zone answers");
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        control::send(
            &mut send,
            &FromClient::Hello {
                version: PROTOCOL_VERSION as u16,
                name: name.to_string(),
                token: bitcode::encode(&ticket.token),
                build: None,
                team: 0,
            },
        )
        .await
        .unwrap();
        let (entity, hz) = match control::recv(&mut recv).await.unwrap() {
            Some(FromZone::Welcome { entity, hz, .. }) => (entity, hz),
            other => panic!("{name}: no welcome: {other:?}"),
        };
        let (pack, own, team) = match control::recv(&mut recv).await.unwrap() {
            Some(FromZone::Content {
                pack, own, team, ..
            }) => (pack, own, team),
            other => panic!("{name}: no content: {other:?}"),
        };
        let rate = TickRate::new(hz as u32);
        let mut client = ClientState::new(entity, rate, Sheet::new(own.clone(), &pack, team));
        let shared = Arc::new(Mutex::new(Shared {
            id: entity,
            own: Some(own),
            yaw,
            ..Shared::default()
        }));
        let seen = shared.clone();
        let task = tokio::spawn(async move {
            let _endpoint = endpoint;
            let mut next = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(next) => {
                        next += rate.period();
                        let (input, say) = {
                            let mut s = seen.lock().unwrap();
                            s.health = client.own_health;
                            s.alive = client.own_alive;
                            s.bar = client.mover.bar;
                            s.using = client.mover.using_item(client.tick);
                            s.synced = client.synced();
                            let input = Input {
                                buttons: s.buttons,
                                yaw: s.yaw,
                                pitch: 0.0,
                                forward: 0.0,
                                side: 0.0,
                                ability: 0,
                                held: s.held,
                                target: 0,
                                use_slot: s.use_slot,
                            };
                            (input, std::mem::take(&mut s.say))
                        };
                        for msg in say {
                            if control::send(&mut send, &msg).await.is_err() {
                                return;
                            }
                        }
                        let datagram = client.local_tick(world.as_ref(), input);
                        if conn.send_datagram(Bytes::from(datagram.encode())).is_err() {
                            return;
                        }
                        let t = client.render_tick(0.0);
                        client.prune(t);
                    }
                    dg = conn.read_datagram() => match dg {
                        Ok(bytes) => {
                            let _ = client.on_snapshot(world.as_ref(), &bytes);
                        }
                        Err(_) => return,
                    },
                    msg = control::recv(&mut recv) => match msg {
                        Ok(Some(msg)) => {
                            let mut s = seen.lock().unwrap();
                            match &msg {
                                FromZone::Stalls(list) => s.stalls = list.clone(),
                                FromZone::StallOpened(stall) => s.stalls.push(stall.clone()),
                                FromZone::StallClosed(id) => s.stalls.retain(|x| x.id != *id),
                                _ => {}
                            }
                            s.heard.push(msg);
                        }
                        _ => return,
                    },
                }
            }
        });
        let hand = Hand { shared, task };
        hand.until("its first snapshot", |s| s.synced && s.alive)
            .await;
        hand
    }

    /// Wait until what the client has seen satisfies `ready` (five seconds at most).
    async fn until<T>(&self, what: &str, ready: impl Fn(&mut Shared) -> T) -> T
    where
        T: Ready,
    {
        let started = Instant::now();
        loop {
            let got = ready(&mut self.shared.lock().unwrap());
            if got.is_ready() {
                return got;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "waited five seconds for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Say something to the zone and wait for its answer to it.
    async fn ask(&self, msg: FromClient) -> Result<(), String> {
        self.ask_all(vec![msg]).await.remove(0)
    }

    /// Say several things at once (they reach the zone within one of its ticks) and wait
    /// for an answer to each, in the order the zone gave them.
    async fn ask_all(&self, msgs: Vec<FromClient>) -> Vec<Result<(), String>> {
        let n = msgs.len();
        {
            let mut s = self.shared.lock().unwrap();
            s.heard.clear();
            s.say.extend(msgs);
        }
        self.until("the zone's answers", |s| {
            let answers: Vec<Result<(), String>> = s
                .heard
                .iter()
                .filter_map(|m| match m {
                    FromZone::BuyResult { result, .. }
                    | FromZone::WearResult { result, .. }
                    | FromZone::StallResult(result)
                    | FromZone::RespecResult(result) => Some(result.clone()),
                    _ => None,
                })
                .collect();
            (answers.len() >= n).then_some(answers)
        })
        .await
        .unwrap()
    }

    /// Ask until the zone no longer says "in a fight" (or that it is busy with the last
    /// asking): what a person does who is told to wait a moment. `Err`: it said something
    /// else.
    async fn ask_when_calm(&self, msg: FromClient) -> Result<(), String> {
        let started = Instant::now();
        loop {
            match self.ask(msg.clone()).await {
                Err(why) if why == FIGHT || why == GATE => {
                    assert!(
                        started.elapsed() < Duration::from_secs(30),
                        "still \"{why}\" after thirty seconds"
                    );
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                other => return other,
            }
        }
    }

    fn health(&self) -> i32 {
        self.shared.lock().unwrap().health
    }

    /// One swing of the primary: held for a fifth of a second.
    async fn swing(&self) {
        self.shared.lock().unwrap().buttons = buttons::PRIMARY;
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.shared.lock().unwrap().buttons = 0;
    }

    /// One press of an item cell's key (MODES.md 11.3, LOOK.md 3.2): `USE` naming the
    /// cell (1-based; `F` is 1), held for a tenth of a second.
    async fn press_item(&self, cell: u8) {
        {
            let mut s = self.shared.lock().unwrap();
            s.buttons = buttons::USE;
            s.use_slot = cell;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut s = self.shared.lock().unwrap();
        s.buttons = 0;
        s.use_slot = 0;
    }

    /// Hang up without a goodbye: the zone saves the character and it goes offline.
    async fn leave(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

trait Ready {
    fn is_ready(&self) -> bool;
}

impl Ready for bool {
    fn is_ready(&self) -> bool {
        *self
    }
}

impl<T> Ready for Option<T> {
    fn is_ready(&self) -> bool {
        self.is_some()
    }
}

struct Someone {
    hub: HubClient,
    session: SessionId,
    character: CharacterId,
}

impl Someone {
    async fn new(addr: std::net::SocketAddr, cert: &[u8], name: &str, preset: &str) -> Someone {
        let hub = HubClient::connect_with_cert(addr, cert.to_vec())
            .await
            .unwrap();
        let HubResponse::Session { session, .. } = hub
            .request(&HubRequest::Register {
                email: format!("{name}@example.test"),
                password: PASSWORD.into(),
            })
            .await
            .unwrap()
        else {
            panic!("register")
        };
        let HubResponse::Character(c) = hub
            .request(&HubRequest::CreateCharacter {
                session,
                name: name.into(),
                build: BuildChoice::Preset(preset.into()),
            })
            .await
            .unwrap()
        else {
            panic!("create")
        };
        Someone {
            hub,
            session,
            character: c.id,
        }
    }

    async fn ticket(&self) -> ZoneTicket {
        self.ticket_for("town").await
    }

    async fn ticket_for(&self, zone: &str) -> ZoneTicket {
        // A character that just left is put away by its zone in a moment: asked again.
        for _ in 0..50 {
            match self
                .hub
                .request(&HubRequest::Enter {
                    session: self.session,
                    character: self.character,
                    zone: zone.into(),
                })
                .await
            {
                Ok(HubResponse::Ticket(t)) => return t,
                Ok(other) => panic!("{other:?}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        panic!("the hub never let the character in again")
    }

    async fn econ(&self, op: EconOp) -> EconReply {
        match self
            .hub
            .request(&HubRequest::Econ {
                session: self.session,
                character: self.character,
                op,
            })
            .await
            .unwrap()
        {
            HubResponse::Econ(reply) => reply,
            other => panic!("{other:?}"),
        }
    }

    async fn inventory(&self) -> (i64, Vec<ItemSummary>) {
        match self.econ(EconOp::Inventory).await {
            EconReply::Holder { coin, items } => (coin, items),
            other => panic!("{other:?}"),
        }
    }

    /// The build the hub holds for the character.
    async fn stored_build(&self, db: &Db) -> Build {
        db.character(self.character)
            .await
            .unwrap()
            .expect("the character is in the database")
            .build
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| p.to_string()).collect()
}

/// The tests of this file share one database and wipe it: one at a time.
static DATABASE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A hub and the town zone, stood up on this machine for one test: what a test needs
/// of them by name. The tasks run until the test's runtime is dropped.
struct Town {
    db: Db,
    hub_addr: std::net::SocketAddr,
    hub_cert: Vec<u8>,
    world: Arc<ZoneWorld>,
    map: Arc<Bsp>,
    grid: gm_bsp::StallGrid,
}

/// Another zone under the same hub, with the map at `path`; wild or a team zone.
async fn add_zone(town: &Town, zone: &str, path: &str, wild: bool) -> Arc<ZoneWorld> {
    let content = gm_content::load_dir(Path::new(CONTENT), TickRate::COMBAT).expect("content");
    let identity = Identity::generate(&["localhost"]).unwrap();
    let world = Arc::new(ZoneWorld::load(Path::new(path)).expect("the map is built"));
    let endpoint = quinn::Endpoint::server(
        server_config(&identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let link = HubLink::connect(HubLinkConfig {
        addr: town.hub_addr,
        cert_der: town.hub_cert.clone(),
        zone: zone.into(),
        secret: SECRET.into(),
        map: world.name.clone(),
        map_hash: world.hash,
        public_addr: endpoint.local_addr().unwrap(),
        zone_cert_der: identity.cert_der().to_vec(),
        web: None,
        min_trust: 0,
        requires: Vec::new(),
        max_players: 64,
    })
    .await
    .expect("the zone registers");
    let _zone = tokio::spawn(gm_server::run(
        ZoneConfig {
            max_ticks: Some(64 * 120),
            content,
            looks: gm_content::looks::Looks::load_dir(Path::new(CONTENT)).expect("looks"),
            hub: Some(link),
            gear_after_fight: LOCK,
            gm_names: Vec::new(),
            tuning_file: None,
            wild,
            ..ZoneConfig::default()
        },
        world.clone(),
        endpoint,
        std::future::pending(),
    ));
    world
}

/// `wild`: the town's creatures (the trainer, the dummies) stand on their posts and every
/// human is team 1 (COMPANIONS.md 3.1); else a team zone on the town's map.
async fn stand_up(url: &str, wild: bool) -> Town {
    let db = Db::connect(url).await.expect("database");
    db.migrate().await.expect("migrations");
    db.wipe().await.expect("wipe");
    let content = gm_content::load_dir(Path::new(CONTENT), TickRate::COMBAT).expect("content");
    let items = gm_content::items::load_items(Path::new(CONTENT)).expect("items");

    // The hub.
    let identity = Identity::generate(&[gm_hub::HUB_SERVER_NAME, "localhost"]).unwrap();
    let hub_cert = identity.cert_der().to_vec();
    let endpoint = quinn::Endpoint::server(
        hub_server_config(&identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let hub_addr = endpoint.local_addr().unwrap();
    let _hub = tokio::spawn(gm_hub::run(
        HubConfig {
            zone_secret: SECRET.into(),
            content: content.clone(),
            key: HubKey::generate(),
            session_secs: 3600,
            auth_per_minute: 1000.0,
            econ_per_second: 1000.0,
            party_sweep: std::time::Duration::from_millis(300),
            party_away: std::time::Duration::from_secs(2),
            items: items.clone(),
            looks: gm_content::looks::Looks::load_dir(Path::new(CONTENT)).expect("looks"),
            max_coin_grant: 500,
            models_dir: std::env::temp_dir()
                .join(format!("gm-items-models-{}", std::process::id())),
            ingest: IngestMode::InProcess,
            ingest_timeout: gm_hub::models::INGEST_TIMEOUT,
            start_zone: None,
            blurbs: Vec::new(),
        },
        db.clone(),
        endpoint,
        std::future::pending(),
    ));

    // The town, whose fight lock is short enough to wait out.
    let zone_identity = Identity::generate(&["localhost"]).unwrap();
    let world = Arc::new(ZoneWorld::load(Path::new(TOWN)).expect("town.bsp is built"));
    let map = Arc::new(world.bsp.clone());
    let grid = world.stall_grids[0];
    let zone_endpoint = quinn::Endpoint::server(
        server_config(&zone_identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let link = HubLink::connect(HubLinkConfig {
        addr: hub_addr,
        cert_der: hub_cert.clone(),
        zone: "town".into(),
        secret: SECRET.into(),
        map: world.name.clone(),
        map_hash: world.hash,
        public_addr: zone_endpoint.local_addr().unwrap(),
        zone_cert_der: zone_identity.cert_der().to_vec(),
        web: None,
        min_trust: 0,
        requires: Vec::new(),
        max_players: 64,
    })
    .await
    .expect("the zone registers");
    let _zone = tokio::spawn(gm_server::run(
        ZoneConfig {
            max_ticks: Some(64 * 120),
            content,
            looks: gm_content::looks::Looks::load_dir(Path::new(CONTENT)).expect("looks"),
            hub: Some(link),
            gear_after_fight: LOCK,
            gm_names: Vec::new(),
            tuning_file: None,
            wild,
            ..ZoneConfig::default()
        },
        world.clone(),
        zone_endpoint,
        std::future::pending(),
    ));

    Town {
        db,
        hub_addr,
        hub_cert,
        world,
        map,
        grid,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_weapon_is_bought_at_a_stall_worn_and_felt_in_the_zone_s_hits() {
    let Ok(url) = std::env::var("GM_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set GM_TEST_DATABASE_URL to a Postgres this test may wipe");
        return;
    };
    let _one_at_a_time = DATABASE.lock().await;
    if let Ok(filter) = std::env::var("GM_TRACE") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    }
    let Town {
        db,
        hub_addr,
        hub_cert,
        world,
        map,
        grid,
    } = stand_up(&url, false).await;
    // A keeper on a market tile, a buyer in front of the counter and facing it, and
    // somebody across the square. (An operator puts them there: `gm-hub --place`.)
    // (A wall of plate: each blow moves it a hair, and five of them leave it standing.)
    let smith = Someone::new(hub_addr, &hub_cert, "Smith", "ironclad").await;
    let buyer = Someone::new(hub_addr, &hub_cert, "Buyer", "blade").await;
    let far = Someone::new(hub_addr, &hub_cert, "Far", "blade").await;
    let tile = grid.centre(grid.base_x, grid.base_y).expect("a tile");
    // A hair above standing: a body exactly on the floor is in it, as far as a zone that
    // asks whether a saved place is still a place to stand can tell.
    let up = Vec3::Z * (1.0 - Hull::Player.mins().z);
    for at in [
        tile,
        tile + Vec3::new(
            grid.yaw.to_radians().cos(),
            grid.yaw.to_radians().sin(),
            0.0,
        ) * 52.0,
    ] {
        assert_eq!(
            world.bsp.point_contents(Hull::Player, at + up),
            gm_core::trace::Contents::Empty,
            "{at:?} is a place to stand"
        );
    }
    let (sin, cos) = grid.yaw.to_radians().sin_cos();
    let front = tile + Vec3::new(cos, sin, 0.0) * 52.0;
    assert!(
        db.place(smith.character, "town", (tile + up).into(), grid.yaw)
            .await
            .unwrap()
    );
    assert!(
        db.place(
            buyer.character,
            "town",
            (front + up).into(),
            grid.yaw + 180.0
        )
        .await
        .unwrap()
    );
    let smith_hand = Hand::join(&smith.ticket().await, "Smith", grid.yaw, map.clone()).await;
    let mut buyer_hand = Hand::join(
        &buyer.ticket().await,
        "Buyer",
        grid.yaw + 180.0,
        map.clone(),
    )
    .await;
    let far_hand = Hand::join(&far.ticket().await, "Far", 0.0, map.clone()).await;
    assert!(
        !db.place(buyer.character, "town", (front + up).into(), 0.0)
            .await
            .unwrap(),
        "a character that plays is not moved behind its zone's back"
    );

    // The keeper opens its stall where it stands and puts two swords up; it keeps a
    // cuirass. The buyer is given what the dearer sword costs, and a little.
    assert_eq!(smith_hand.ask(FromClient::StallOpen).await, Ok(()));
    let stall = buyer_hand
        .until("the stall", |s| {
            s.stalls.iter().find(|x| x.owner == "Smith").map(|x| x.id)
        })
        .await
        .unwrap();
    let direct = Economy::new(db.pool().clone());
    let best = strings(&[
        "shard/boss_scale",
        "core/dragonbone",
        "catalyst/ember",
        "frame/whalebone",
        "gem/opal",
        "gem/opal",
    ]);
    let sword = direct
        .grant_item(smith.character, "sword", &best, None)
        .await
        .unwrap();
    let plain = direct
        .grant_item(
            smith.character,
            "sword",
            &strings(&["core/iron", "frame/oak"]),
            None,
        )
        .await
        .unwrap();
    let cuirass = direct
        .grant_item(
            smith.character,
            "cuirass",
            &strings(&[
                "shard/boss_scale",
                "core/dragonbone",
                "frame/whalebone",
                "gem/opal",
                "gem/opal",
            ]),
            None,
        )
        .await
        .unwrap();
    direct
        .grant_coin_as(buyer.character, 12_500, "grant", 0)
        .await
        .unwrap();
    direct
        .grant_coin_as(far.character, 50_000, "grant", 0)
        .await
        .unwrap();
    let list = |item: i64, price: i64| smith.econ(EconOp::StallList { item, price });
    let EconReply::Id(listing) = list(sword, 12_000).await else {
        panic!("list")
    };
    let EconReply::Id(dear) = list(plain, 90_000).await else {
        panic!("list")
    };
    let buy = |listing: i64, price: i64| FromClient::StallBuy {
        stall,
        listing,
        price,
    };

    // Buying is done standing at the stall: across the square the zone says so, whatever
    // the purse holds. (A refusal of the zone's own costs nobody their second: asked
    // twice at once, it is told twice.)
    let walk = Err("walk up to the stall to buy".to_string());
    assert_eq!(
        far_hand
            .ask_all(vec![buy(listing, 12_000), buy(listing, 12_000)])
            .await,
        vec![walk.clone(), walk]
    );
    // At the counter. Two requests at once: the hub is asked the first (and refuses a
    // price that is not the listing's), the second is stopped by the gate, and both are
    // answered.
    let mut two = buyer_hand
        .ask_all(vec![buy(listing, 11_000), buy(listing, 12_000)])
        .await;
    two.sort();
    assert_eq!(
        two,
        vec![Err(GATE.to_string()), Err("the price changed".to_string())]
    );
    // Then, a second later each time: the purchase; not twice; not more than the purse
    // holds; not from oneself; not from a stall that is not there.
    assert_eq!(buyer_hand.ask_when_calm(buy(listing, 12_000)).await, Ok(()));
    assert_eq!(
        buyer_hand.ask_when_calm(buy(listing, 12_000)).await,
        Err("it is no longer for sale here".into())
    );
    assert_eq!(
        buyer_hand.ask_when_calm(buy(dear, 90_000)).await,
        Err("not enough coin".into())
    );
    assert_eq!(
        smith_hand.ask_when_calm(buy(dear, 90_000)).await,
        Err("that is your own stall".into())
    );
    let nowhere = FromClient::StallBuy {
        stall: stall + 7,
        listing: dear,
        price: 90_000,
    };
    assert_eq!(
        buyer_hand.ask_when_calm(nowhere).await,
        Err("that stall has closed".into())
    );
    let (coin, has) = buyer.inventory().await;
    assert_eq!((coin, has.len(), has[0].id), (500, 1, sword));
    // A part, for what cannot be worn.
    let gem = direct
        .grant_components("town", &[(buyer.character, "gem/quartz".to_string())], 0)
        .await
        .unwrap()[0];
    assert_eq!(smith.inventory().await.0, 12_000, "no tax and no fee");

    // The zone's hits, before and after. A swing in nothing first: what the keeper loses
    // is the number everything else is measured against.
    let hit = async |buyer_hand: &Hand| -> i32 {
        let before = smith_hand.health();
        buyer_hand.swing().await;
        smith_hand
            .until(&format!("a blow to land on {before}"), |s| {
                s.health < before
            })
            .await;
        // The swing is over before the next thing is asked.
        tokio::time::sleep(Duration::from_millis(300)).await;
        before - smith_hand.health()
    };
    let bare = hit(&buyer_hand).await;
    assert!(bare >= 10, "a sword on plate: {bare}");
    // The unrounded number is within half a point of `bare`: what a factor does to it.
    let moved = |factor: f32| {
        let (lo, hi) = (
            ((bare as f32 - 0.5) * factor + 0.5).floor() as i32,
            ((bare as f32 + 0.5) * factor + 0.5).floor() as i32,
        );
        lo..=hi
    };

    // What a body wears does not change in a fight: the buyer has just dealt damage and
    // the keeper has just taken it.
    assert_eq!(
        buyer_hand.ask(FromClient::Wear { item: sword }).await,
        Err(FIGHT.into())
    );
    assert_eq!(
        smith_hand.ask(FromClient::Wear { item: cuirass }).await,
        Err(FIGHT.into())
    );
    // A moment later it does (asked again until the zone has stopped saying so), at once:
    // the sword's edge on its own kind is 220 per mille, and a place counts for half:
    // eleven per cent more.
    assert_eq!(
        buyer_hand
            .ask_when_calm(FromClient::Wear { item: sword })
            .await,
        Ok(())
    );
    let armed = hit(&buyer_hand).await;
    assert!(
        moved(1.11).contains(&armed) && armed > bare,
        "{bare} in nothing, {armed} with the sword"
    );
    // What is not one's own is not put on, nor what cannot be worn at all; and of two
    // requests at once the gate stops one and answers both.
    assert_eq!(
        buyer_hand
            .ask_when_calm(FromClient::Wear { item: cuirass })
            .await,
        Err("that is not in the inventory".into())
    );
    assert_eq!(
        buyer_hand
            .ask_when_calm(FromClient::Wear { item: gem })
            .await,
        Err("that cannot be worn".into())
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut two = buyer_hand
        .ask_all(vec![
            FromClient::Wear { item: gem },
            FromClient::Wear { item: gem },
        ])
        .await;
    two.sort();
    assert_eq!(
        two,
        vec![
            Err(GATE.to_string()),
            Err("that cannot be worn".to_string())
        ]
    );
    // The keeper's cuirass takes the same eleven per cent off again: a wash.
    assert_eq!(
        smith_hand
            .ask_when_calm(FromClient::Wear { item: cuirass })
            .await,
        Ok(())
    );
    let both = hit(&buyer_hand).await;
    assert_eq!(both, bare, "sword against cuirass");

    // A claim carries what is worn: the buyer hangs up, is put away, comes back to where
    // it stood, and its sword still counts.
    buyer_hand.leave().await;
    buyer_hand = Hand::join(
        &buyer.ticket().await,
        "Buyer",
        grid.yaw + 180.0,
        map.clone(),
    )
    .await;
    let back = hit(&buyer_hand).await;
    assert_eq!(back, bare, "the sword came back with its wearer");

    // Taken off, the cuirass alone is left: eleven per cent less than in nothing.
    assert_eq!(
        buyer_hand
            .ask_when_calm(FromClient::TakeOff { item: sword })
            .await,
        Ok(())
    );
    let warded = hit(&buyer_hand).await;
    assert!(
        moved(1.0 / 1.11).contains(&warded) && warded < bare,
        "{bare} in nothing, {warded} against the cuirass"
    );
    println!(
        "items: a sword swing on plate {bare}; with the best sword {armed}; against the best cuirass too {both}; the cuirass alone {warded}"
    );

    // What the hub holds is what the zone applied; and the books are sound.
    let (_, has) = buyer.inventory().await;
    assert!(has.iter().all(|i| !i.worn));
    let (_, has) = smith.inventory().await;
    assert!(has.iter().any(|i| i.id == cuirass && i.worn));
    assert_eq!(direct.audit().await, Ok(0));
}

/// The gun mode's hand (MODES.md 3.7, LOOK.md 6.2): a musketeer arrives holding its
/// musket; what it switches to, everyone is told it holds, once a switch; and the musket
/// again when it switches back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_weapon_switched_to_is_what_everyone_sees_in_the_hand() {
    let Ok(url) = std::env::var("GM_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set GM_TEST_DATABASE_URL to a Postgres this test may wipe");
        return;
    };
    let _one_at_a_time = DATABASE.lock().await;
    let town = stand_up(&url, false).await;
    let looks = gm_content::looks::Looks::load_dir(Path::new(CONTENT)).expect("looks");
    let (musket, pistol, dagger) = (
        looks.prop_index("musket"),
        looks.prop_index("pistol"),
        looks.prop_index("dagger"),
    );
    assert!(musket != pistol && pistol != dagger && dagger != musket);

    let gunner = Someone::new(town.hub_addr, &town.hub_cert, "Gunner", "musketeer").await;
    let watcher = Someone::new(town.hub_addr, &town.hub_cert, "Watcher", "blade").await;
    let g = Hand::join(&gunner.ticket().await, "Gunner", 0.0, town.map.clone()).await;
    let w = Hand::join(&watcher.ticket().await, "Watcher", 0.0, town.map.clone()).await;
    let gunner_id = g.shared.lock().unwrap().id;

    // On arrival the watcher is told the musket, in the roster or the player's info.
    w.until("the gunner's musket on arrival", |s| {
        s.heard.iter().any(|m| match m {
            FromZone::Roster(list) => list
                .iter()
                .any(|e| e.id == gunner_id && e.look.held == musket),
            FromZone::PlayerInfo { id, look, .. } => *id == gunner_id && look.held == musket,
            _ => false,
        })
    })
    .await;

    for (hand, prop, what) in [
        (1u8, pistol, "the pistol"),
        (2, dagger, "the knife"),
        (0, musket, "the musket again"),
    ] {
        w.shared.lock().unwrap().heard.clear();
        g.shared.lock().unwrap().held = hand;
        w.until(what, |s| {
            s.heard.iter().any(
                |m| matches!(m, FromZone::Look { id, look } if *id == gunner_id && look.held == prop),
            )
        })
        .await;
    }
    // Said once a switch, not once a tick.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let said = w
        .shared
        .lock()
        .unwrap()
        .heard
        .iter()
        .filter(|m| matches!(m, FromZone::Look { id, .. } if *id == gunner_id))
        .count();
    assert_eq!(said, 1, "one look for the switch back");
}

/// A build worn at the trainer is the character's (MATRIX.md 9.1): the hub holds it
/// before the zone says "worn", it is what the character wears when it enters again,
/// and the same for one chosen in a team zone and worn at the next respawn. Away from
/// the trainer the zone says where to stand.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_worn_at_the_trainer_is_the_character_s_when_it_enters_again() {
    let Ok(url) = std::env::var("GM_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set GM_TEST_DATABASE_URL to a Postgres this test may wipe");
        return;
    };
    let _one_at_a_time = DATABASE.lock().await;
    if let Ok(filter) = std::env::var("GM_TRACE") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    }
    let town = stand_up(&url, true).await;
    let _arena = add_zone(&town, "arena", ARENA, false).await;
    let content = gm_content::load_dir(Path::new(CONTENT), TickRate::COMBAT).expect("content");
    let blade = content.build("blade").unwrap().clone();
    // What the director did: a point off STR and onto CON, by the trainer at the board.
    let mut chosen = blade.clone();
    chosen.attributes.str_ -= 1;
    chosen.attributes.con += 1;
    assert!(chosen.validate(&content).is_ok());
    let trainer = town
        .world
        .creature_posts
        .iter()
        .find(|p| p.creature == "trainer")
        .expect("the town posts a trainer")
        .origin;
    let up = Vec3::Z * (1.0 - Hull::Player.mins().z);
    // A spawn of the town's within reach of the trainer, and one well out of it.
    let near = town
        .world
        .spawns
        .iter()
        .map(|s| s.origin)
        .find(|o| (*o - trainer).length() <= TRAINER_REACH * 0.5)
        .expect("a spawn by the trainer");
    let far_off = town
        .world
        .spawns
        .iter()
        .map(|s| s.origin)
        .find(|o| (*o - trainer).length() > TRAINER_REACH * 3.0)
        .expect("a spawn away from the trainer");

    let student = Someone::new(town.hub_addr, &town.hub_cert, "Student", "blade").await;
    let far = Someone::new(town.hub_addr, &town.hub_cert, "Far", "blade").await;
    assert!(
        town.db
            .place(student.character, "town", (near + up).into(), 0.0)
            .await
            .unwrap()
    );
    assert!(
        town.db
            .place(far.character, "town", (far_off + up).into(), 0.0)
            .await
            .unwrap()
    );
    let hand = Hand::join(&student.ticket().await, "Student", 0.0, town.map.clone()).await;
    let far_hand = Hand::join(&far.ticket().await, "Far", 0.0, town.map.clone()).await;
    assert_eq!(hand.shared.lock().unwrap().own.as_ref(), Some(&blade));

    // Away from the trainer: refused, in words, and nothing is saved.
    assert_eq!(
        far_hand
            .ask(FromClient::Respec(BuildChoice::Custom(chosen.clone())))
            .await,
        Err("stand by the trainer in the town".into())
    );
    assert_eq!(far.stored_build(&town.db).await, blade);

    // By the trainer: worn, and the hub holds it by the time the zone says so.
    assert_eq!(
        hand.ask_when_calm(FromClient::Respec(BuildChoice::Custom(chosen.clone())))
            .await,
        Ok(())
    );
    assert_eq!(student.stored_build(&town.db).await, chosen);
    hand.until("the build applied", |s| {
        s.heard
            .iter()
            .any(|m| matches!(m, FromZone::BuildApplied(b) if *b == chosen))
    })
    .await;

    // Logged out and in again: the new build is what the zone gives the body.
    hand.leave().await;
    let hand = Hand::join(&student.ticket().await, "Student", 0.0, town.map.clone()).await;
    assert_eq!(hand.shared.lock().unwrap().own.as_ref(), Some(&chosen));
    hand.leave().await;

    // In a team zone the build is worn at the next respawn: chosen, saved, and worn on
    // entering the town again without ever having died in the arena.
    let mut again = chosen.clone();
    again.attributes.agi -= 1;
    again.attributes.spr += 1;
    assert!(again.validate(&content).is_ok());
    let world = ZoneWorld::load(Path::new(ARENA)).expect("arena.bsp is built");
    let arena_map = Arc::new(world.bsp.clone());
    let hand = Hand::join(
        &student.ticket_for("arena").await,
        "Student",
        0.0,
        arena_map,
    )
    .await;
    assert_eq!(hand.shared.lock().unwrap().own.as_ref(), Some(&chosen));
    assert_eq!(
        hand.ask(FromClient::Respec(BuildChoice::Custom(again.clone())))
            .await,
        Ok(())
    );
    assert_eq!(student.stored_build(&town.db).await, again);
    assert!(hand.shared.lock().unwrap().alive, "never died in the arena");
    hand.leave().await;
    let hand = Hand::join(&student.ticket().await, "Student", 0.0, town.map.clone()).await;
    assert_eq!(hand.shared.lock().unwrap().own.as_ref(), Some(&again));
    println!(
        "respec: worn at the trainer and saved; chosen in the arena and saved; both worn again on entering"
    );
}

/// The kit (MODES.md 11.3) over the real protocol: bought at a stall (the zone learns the
/// stack from the reading after the buy), used with a press of `USE` when hurt (a use of a
/// second and a half, the heal at its end, one kit fewer at the hub), and begun for nothing
/// at full health (cleared by the zone the same tick, the kit kept).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kit_bought_at_a_stall_heals_on_a_press_and_is_kept_at_full_health() {
    let Ok(url) = std::env::var("GM_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set GM_TEST_DATABASE_URL to a Postgres this test may wipe");
        return;
    };
    let _one_at_a_time = DATABASE.lock().await;
    if let Ok(filter) = std::env::var("GM_TRACE") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    }
    let Town {
        db,
        hub_addr,
        hub_cert,
        world: _,
        map,
        grid,
    } = stand_up(&url, false).await;
    let smith = Someone::new(hub_addr, &hub_cert, "Smith", "ironclad").await;
    let buyer = Someone::new(hub_addr, &hub_cert, "Buyer", "blade").await;
    let tile = grid.centre(grid.base_x, grid.base_y).expect("a tile");
    let up = Vec3::Z * (1.0 - Hull::Player.mins().z);
    let (sin, cos) = grid.yaw.to_radians().sin_cos();
    let front = tile + Vec3::new(cos, sin, 0.0) * 52.0;
    assert!(
        db.place(smith.character, "town", (tile + up).into(), grid.yaw)
            .await
            .unwrap()
    );
    assert!(
        db.place(
            buyer.character,
            "town",
            (front + up).into(),
            grid.yaw + 180.0
        )
        .await
        .unwrap()
    );
    let smith_hand = Hand::join(&smith.ticket().await, "Smith", grid.yaw, map.clone()).await;
    let buyer_hand = Hand::join(
        &buyer.ticket().await,
        "Buyer",
        grid.yaw + 180.0,
        map.clone(),
    )
    .await;
    let full = buyer_hand.health();
    assert!(full > 0);

    // Three kits on the keeper's stall; the buyer, who carries none, buys them.
    assert_eq!(smith_hand.ask(FromClient::StallOpen).await, Ok(()));
    let stall = buyer_hand
        .until("the stall", |s| {
            s.stalls.iter().find(|x| x.owner == "Smith").map(|x| x.id)
        })
        .await
        .unwrap();
    let items = gm_content::items::load_items(Path::new(CONTENT)).expect("items");
    let direct = Economy::new(db.pool().clone()).with_stacks(&items);
    let kits = direct.grant_stack(smith.character, "kit", 3).await.unwrap();
    direct
        .grant_coin_as(buyer.character, 2_500, "grant", 0)
        .await
        .unwrap();
    let EconReply::Id(listing) = smith
        .econ(EconOp::StallList {
            item: kits,
            price: 600,
        })
        .await
    else {
        panic!("list")
    };
    assert_eq!(buyer_hand.shared.lock().unwrap().bar, [0; 4]);
    assert_eq!(
        buyer_hand
            .ask_when_calm(FromClient::StallBuy {
                stall,
                listing,
                price: 600
            })
            .await,
        Ok(())
    );
    // The zone read the stack after the buy, and the own block carries it on the first
    // cell: the bar's default for a character that never arranged it (MODES.md 11.7).
    buyer_hand
        .until("the zone to learn of the kits", |s| s.bar == [3, 0, 0, 0])
        .await;
    let heals = buyer
        .inventory()
        .await
        .1
        .iter()
        .find(|i| i.template == "kit")
        .map(|i| {
            assert_eq!((i.quantity, i.cap), (3, 5));
            i.does
                .iter()
                .find_map(|d| d.strip_prefix("heals ").and_then(|r| r.split(',').next()))
                .and_then(|n| n.parse::<i32>().ok())
                .expect("what a kit heals, in its words")
        })
        .expect("the kits in the inventory");

    // At full health a press begins nothing that lasts: the kit is kept.
    buyer_hand.press_item(1).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    {
        let s = buyer_hand.shared.lock().unwrap();
        assert!(s.using.is_none(), "at full health the zone clears the use");
        assert_eq!(s.bar, [3, 0, 0, 0]);
    }

    // Hurt by the keeper's hammer, the press heals: a second and a half later, the kit's
    // worth (to full), one kit fewer.
    smith_hand.swing().await;
    let hurt = buyer_hand
        .until("the blow to land", |s| {
            (s.health < full).then_some(s.health)
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    buyer_hand.press_item(1).await;
    buyer_hand
        .until("the use to begin", |s| s.using == Some(0))
        .await;
    let began = Instant::now();
    let (healed_to, kits_left) = buyer_hand
        .until("the heal", |s| {
            (s.health > hurt).then_some((s.health, s.bar[0]))
        })
        .await
        .unwrap();
    let took = began.elapsed();
    assert!(
        took >= Duration::from_millis(1_200) && took <= Duration::from_millis(2_500),
        "a kit takes a second and a half, not {took:?}"
    );
    assert_eq!(healed_to, (hurt + heals).min(full));
    assert_eq!(kits_left, 2, "one kit fewer at the end");
    assert!(buyer_hand.shared.lock().unwrap().using.is_none());
    // The hub was told (Consume): the stack is two.
    let started = Instant::now();
    loop {
        let (_, has) = buyer.inventory().await;
        let kit = has.iter().find(|i| i.template == "kit").expect("the stack");
        if kit.quantity == 2 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the hub still holds {} kits",
            kit.quantity
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!(
        "items: a kit bought for 6 s, pressed at {hurt} of {full}, healed {heals} to {healed_to} in {:.2} s, two left",
        took.as_secs_f32()
    );

    // The bar is the player's (LOOK.md 3.2): the kit moved to the third cell through the
    // hub, the zone is told with the gear; `F` finds nothing there now, and the third
    // cell's key uses one.
    assert_eq!(
        buyer
            .econ(EconOp::SetBar {
                cells: vec![None, None, Some("kit".to_string()), None]
            })
            .await,
        EconReply::Done
    );
    buyer_hand
        .until("the zone to learn of the bar", |s| s.bar == [0, 0, 2, 0])
        .await;
    smith_hand.swing().await;
    let hurt = buyer_hand
        .until("the second blow to land", |s| {
            (s.health < full).then_some(s.health)
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    buyer_hand.press_item(1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let s = buyer_hand.shared.lock().unwrap();
        assert!(s.using.is_none(), "F: nothing on the first cell");
        assert_eq!(s.bar, [0, 0, 2, 0]);
    }
    buyer_hand.press_item(3).await;
    buyer_hand
        .until("the third cell's use to begin", |s| s.using == Some(2))
        .await;
    let (healed_to, left) = buyer_hand
        .until("the second heal", |s| {
            (s.health > hurt).then_some((s.health, s.bar))
        })
        .await
        .unwrap();
    assert_eq!(healed_to, (hurt + heals).min(full));
    assert_eq!(left, [0, 0, 1, 0]);
    assert_eq!(direct.audit().await, Ok(0));
}
