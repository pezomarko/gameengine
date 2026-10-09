//! Phase 12 over the real protocol (PARTY.md 4 to 6, 9 step 3): a hub, a town and a
//! dungeon, and clients driven by hand. People invite and join through their zones, a
//! party holds from one zone to the next, its lines and a whisper go where they are heard,
//! a trade is opened by two who stand together and both ask, and the party a body fights
//! under does not change in the middle of a fight. Needs `GM_TEST_DATABASE_URL` (a
//! Postgres the test may wipe); without it the test is skipped.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use glam::Vec3;
use gm_bsp::Bsp;
use gm_core::build::Sheet;
use gm_core::sim::{Input, buttons};
use gm_core::tick::TickRate;
use gm_core::trace::{CollisionWorld, Contents, Hull};
use gm_hub::economy::Economy;
use gm_hub::protocol::{
    BuildChoice, CharacterId, EconOp, EconReply, HubError, HubRequest, HubResponse, SessionId,
    TRADE_CANCELLED, TRADE_COMMITTED, TRADE_OPEN,
};
use gm_hub::{Db, HubClient, HubClientError, HubConfig, HubKey, IngestMode};
use gm_net::PROTOCOL_VERSION;
use gm_net::client::ClientState;
use gm_net::control::{
    self, CHANNEL_PARTY, CHANNEL_WHISPER, CHANNEL_WHISPERED, FromClient, FromZone, TRADE_REACH,
};
use gm_net::transport::{Identity, SERVER_NAME, client_config, hub_server_config, server_config};
use gm_server::{HubLink, HubLinkConfig, ZoneConfig, ZoneWorld};
use quinn::rustls::pki_types::CertificateDer;

const MAPS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/maps/built");
const CONTENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/content");
const SECRET: &str = "party-zone-secret";
const PASSWORD: &str = "correct horse battery";
const GATE: &str = "one thing at a time: try again in a moment";

/// What the test tells a client to do, and what the client has seen.
#[derive(Default)]
struct Shared {
    yaw: f32,
    buttons: u16,
    /// Walking: 1 is straight ahead.
    forward: f32,
    say: Vec<FromClient>,
    /// Its own health, and the most it has had.
    own: i32,
    full: i32,
    alive: bool,
    synced: bool,
    heard: Vec<FromZone>,
    /// The bodies the zone announced, by name.
    names: HashMap<String, u32>,
    /// The health the wire carries for each body it sends (a party's members, creatures).
    health: HashMap<u32, Option<u16>>,
}

/// A client driven by the test: it stands where it was put, looks where it is told,
/// holds the buttons it is told to and says to the zone what it is told to.
struct Hand {
    name: String,
    entity: u32,
    shared: Arc<Mutex<Shared>>,
    task: tokio::task::JoinHandle<()>,
}

impl Hand {
    async fn join(
        addr: std::net::SocketAddr,
        cert_der: &[u8],
        token: Vec<u8>,
        name: &str,
        yaw: f32,
        world: Arc<Bsp>,
    ) -> Hand {
        Hand::try_join(addr, cert_der, token, name, yaw, world)
            .await
            .unwrap_or_else(|why| panic!("{name}: not let in: {why}"))
    }

    /// `Err`: the zone's reason for not letting the character in.
    async fn try_join(
        addr: std::net::SocketAddr,
        cert_der: &[u8],
        token: Vec<u8>,
        name: &str,
        yaw: f32,
        world: Arc<Bsp>,
    ) -> Result<Hand, String> {
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(
            client_config(&[CertificateDer::from(cert_der.to_vec())]).unwrap(),
        );
        let conn = endpoint
            .connect(addr, SERVER_NAME)
            .unwrap()
            .await
            .expect("the zone answers");
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let hello = FromClient::Hello {
            version: PROTOCOL_VERSION as u16,
            name: name.to_string(),
            token,
            build: None,
            team: 0,
        };
        control::send(&mut send, &hello).await.unwrap();
        let (entity, hz) = match control::recv(&mut recv).await.unwrap() {
            Some(FromZone::Welcome { entity, hz, .. }) => (entity, hz),
            Some(FromZone::Reject(why)) => return Err(why),
            other => panic!("{name}: no welcome: {other:?}"),
        };
        let (pack, own, team) = match control::recv(&mut recv).await.unwrap() {
            Some(FromZone::Content {
                pack, own, team, ..
            }) => (pack, own, team),
            other => panic!("{name}: no content: {other:?}"),
        };
        let rate = TickRate::new(hz as u32);
        let mut client = ClientState::new(entity, rate, Sheet::new(own, &pack, team));
        let shared = Arc::new(Mutex::new(Shared {
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
                        let t = client.render_tick(0.0);
                        let (input, say) = {
                            let mut s = seen.lock().unwrap();
                            s.alive = client.own_alive;
                            s.own = client.own_health;
                            s.full = s.full.max(s.own);
                            s.synced = client.synced();
                            s.health = client
                                .others_at(t)
                                .iter()
                                .map(|e| (e.id, e.health))
                                .collect();
                            let input = Input {
                                buttons: s.buttons,
                                yaw: s.yaw,
                                pitch: 0.0,
                                forward: s.forward,
                                side: 0.0,
                                ability: 0,
                                held: 0,
                                target: 0,
                                use_slot: 0,
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
                        client.prune(t);
                    }
                    dg = conn.read_datagram() => match dg {
                        Ok(bytes) => {
                            let _ = client.on_snapshot(world.as_ref(), &bytes);
                        }
                        Err(_) => return,
                    },
                    msg = control::recv::<FromZone>(&mut recv) => match msg {
                        Ok(Some(msg)) => {
                            let mut s = seen.lock().unwrap();
                            match &msg {
                                FromZone::Roster(all) => {
                                    for p in all {
                                        s.names.insert(p.name.clone(), p.id);
                                    }
                                }
                                FromZone::PlayerInfo { id, name, .. } => {
                                    s.names.insert(name.clone(), *id);
                                }
                                _ => {}
                            }
                            s.heard.push(msg);
                        }
                        _ => return,
                    },
                }
            }
        });
        let hand = Hand {
            name: name.to_string(),
            entity,
            shared,
            task,
        };
        hand.until("its first snapshot", |s| s.synced && s.alive)
            .await;
        Ok(hand)
    }

    /// Hang up without a goodbye, as a lost connection does.
    async fn drop_connection(self) {
        self.task.abort();
        let _ = self.task.await;
    }

    /// Wait until what the client has seen satisfies `ready` (eight seconds at most).
    async fn until<T: Ready>(&self, what: &str, ready: impl Fn(&mut Shared) -> T) -> T {
        self.within(Duration::from_secs(8), what, ready).await
    }

    async fn within<T: Ready>(
        &self,
        patience: Duration,
        what: &str,
        ready: impl Fn(&mut Shared) -> T,
    ) -> T {
        let started = Instant::now();
        loop {
            let got = ready(&mut self.shared.lock().unwrap());
            if got.is_ready() {
                return got;
            }
            assert!(
                started.elapsed() < patience,
                "{}: waited for {what}; heard {:?}",
                self.name,
                self.shared.lock().unwrap().heard
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn say(&self, msg: FromClient) {
        self.shared.lock().unwrap().say.push(msg);
    }

    /// Say something the zone takes one of in a second, after that second.
    async fn say_gated(&self, msg: FromClient) {
        tokio::time::sleep(Duration::from_millis(1100)).await;
        self.say(msg);
    }

    /// The first message `want` takes: it is taken out of what was heard.
    async fn hears<T>(&self, what: &str, want: impl Fn(&FromZone) -> Option<T>) -> T {
        self.until(what, |s| {
            let (i, found) = s
                .heard
                .iter()
                .enumerate()
                .find_map(|(i, m)| want(m).map(|t| (i, t)))?;
            s.heard.remove(i);
            Some(found)
        })
        .await
        .unwrap()
    }

    /// The zone's next line to this client.
    async fn line(&self) -> String {
        self.hears("a line of the zone", |m| match m {
            FromZone::ChatFrom { from: 0, text } => Some(text.clone()),
            _ => None,
        })
        .await
    }

    /// Say something the zone answers with a line, and that line; asked again while the
    /// zone says it is still busy with the last asking (what a person does).
    async fn ask(&self, msg: FromClient) -> String {
        for _ in 0..20 {
            self.say(msg.clone());
            let line = self.line().await;
            if line != GATE {
                return line;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        panic!("{}: the zone was busy for six seconds", self.name)
    }

    /// The next word on the party: its members' names.
    async fn party(&self) -> Vec<String> {
        self.hears("the party", |m| match m {
            FromZone::Party(names) => Some(names.clone()),
            _ => None,
        })
        .await
    }

    async fn heard_line(&self, channel: u8) -> (String, String) {
        self.hears("a line through the hub", |m| match m {
            FromZone::Heard {
                channel: c,
                from,
                text,
            } if *c == channel => Some((from.clone(), text.clone())),
            _ => None,
        })
        .await
    }

    /// Nothing `unwanted` arrives in the next third of a second.
    async fn hears_no(&self, what: &str, unwanted: impl Fn(&FromZone) -> bool) {
        tokio::time::sleep(Duration::from_millis(350)).await;
        let s = self.shared.lock().unwrap();
        assert!(
            !s.heard.iter().any(unwanted),
            "{}: heard {what}: {:?}",
            self.name,
            s.heard
        );
    }

    /// The hub opened a trade: its id and the other's name.
    async fn opened(&self) -> (i64, String) {
        self.hears("the trade", |m| match m {
            FromZone::TradeOpened { trade, with } => Some((*trade, with.clone())),
            _ => None,
        })
        .await
    }

    /// The zone said an encounter began (twenty seconds at most).
    async fn engaged(&self) {
        self.within(Duration::from_secs(20), "the fight", |s| {
            s.heard.iter().any(|m| {
                matches!(m, FromZone::Encounter { state, .. }
                    if *state == control::EncounterState::Engaged)
            })
        })
        .await;
    }

    async fn entity_of(&self, name: &str) -> u32 {
        self.until("a body of that name", |s| s.names.get(name).copied())
            .await
            .unwrap()
    }

    /// Whether the wire carries the health of the body `id` to this client. (Not to be
    /// asked from inside `until`: that holds what this locks.)
    fn sees_health(&self, id: u32) -> bool {
        carries(&self.shared.lock().unwrap(), id)
    }

    /// Walk straight ahead (or stop) with these buttons held.
    fn go(&self, forward: f32, buttons: u16) {
        let mut s = self.shared.lock().unwrap();
        s.forward = forward;
        s.buttons = buttons;
    }

    /// Ask to be handed to another zone: where to go, and with what.
    async fn travel(self, zone: &str) -> (std::net::SocketAddr, Vec<u8>, Vec<u8>) {
        self.say(FromClient::Travel(zone.into()));
        let ticket = self
            .hears("a ticket", |m| match m {
                FromZone::TravelTicket {
                    addr,
                    cert_der,
                    token,
                    ..
                } => Some((addr.parse().unwrap(), cert_der.clone(), token.clone())),
                _ => None,
            })
            .await;
        self.say(FromClient::Bye);
        tokio::time::sleep(Duration::from_millis(100)).await;
        self.task.abort();
        let _ = self.task.await;
        ticket
    }
}

/// Wait (a minute and a half at most) for the zone to say `line` to this client.
async fn over(hand: &Hand, line: &str) {
    hand.within(Duration::from_secs(90), "the fight to be over", |s| {
        s.heard
            .iter()
            .any(|m| matches!(m, FromZone::ChatFrom { from: 0, text } if text == line))
    })
    .await;
}

/// Whether the wire carries the health of the body `id` to this client.
fn carries(s: &Shared, id: u32) -> bool {
    matches!(s.health.get(&id), Some(Some(_)))
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
    async fn new(addr: std::net::SocketAddr, cert: &[u8], name: &str) -> Someone {
        Someone::of(addr, cert, name, "blade").await
    }

    async fn of(addr: std::net::SocketAddr, cert: &[u8], name: &str, preset: &str) -> Someone {
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

    /// Enter `zone` as a hand-driven client.
    async fn enter(&self, zone: &str, name: &str, yaw: f32, world: Arc<Bsp>) -> Hand {
        self.try_enter(zone, name, yaw, world)
            .await
            .unwrap_or_else(|why| panic!("{name}: not let in: {why}"))
    }

    /// `Err`: the zone's reason for not letting the character in.
    async fn try_enter(
        &self,
        zone: &str,
        name: &str,
        yaw: f32,
        world: Arc<Bsp>,
    ) -> Result<Hand, String> {
        let req = HubRequest::Enter {
            session: self.session,
            character: self.character,
            zone: zone.into(),
        };
        // A character that just left is put away by its zone in a moment: asked again.
        for _ in 0..80 {
            match self.hub.request(&req).await {
                Ok(HubResponse::Ticket(t)) => {
                    let token = bitcode::encode(&t.token);
                    return Hand::try_join(t.addr, &t.cert_der, token, name, yaw, world).await;
                }
                Ok(other) => panic!("{other:?}"),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        panic!("{name}: the hub never let the character in again")
    }

    /// A trade as this one sees it: state, version, milliseconds until an accept is
    /// taken, the other's name, whether both are in one zone, and the two offers.
    async fn view(
        &self,
        trade: i64,
    ) -> (
        u8,
        i32,
        u32,
        String,
        bool,
        gm_hub::protocol::TradeOffer,
        gm_hub::protocol::TradeOffer,
    ) {
        match self.econ(EconOp::TradeView { trade }).await.unwrap() {
            EconReply::TradeView {
                state,
                version,
                wait_ms,
                with,
                together,
                mine,
                theirs,
            } => (state, version, wait_ms, with, together, mine, theirs),
            other => panic!("{other:?}"),
        }
    }

    async fn econ(&self, op: EconOp) -> Result<EconReply, HubError> {
        let req = HubRequest::Econ {
            session: self.session,
            character: self.character,
            op,
        };
        match self.hub.request(&req).await {
            Ok(HubResponse::Econ(reply)) => Ok(reply),
            Err(HubClientError::Refused(e)) => Err(e),
            other => panic!("{other:?}"),
        }
    }
}

struct Started {
    world: Arc<ZoneWorld>,
    map: Arc<Bsp>,
}

async fn zone(
    name: &str,
    hub_addr: std::net::SocketAddr,
    hub_cert: &[u8],
    content: &gm_core::build::ContentPack,
) -> Started {
    let identity = Identity::generate(&["localhost"]).unwrap();
    let path = format!("{MAPS}/{name}.bsp");
    let world = Arc::new(ZoneWorld::load(Path::new(&path)).expect("the map is built"));
    let endpoint = quinn::Endpoint::server(
        server_config(&identity).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let link = HubLink::connect(HubLinkConfig {
        addr: hub_addr,
        cert_der: hub_cert.to_vec(),
        zone: name.into(),
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
    tokio::spawn(gm_server::run(
        ZoneConfig {
            max_ticks: Some(64 * 600),
            content: content.clone(),
            hub: Some(link),
            wild: !world.creature_posts.is_empty(),
            ..ZoneConfig::default()
        },
        world.clone(),
        endpoint,
        std::future::pending(),
    ));
    Started {
        map: Arc::new(world.bsp.clone()),
        world,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn people_join_each_other_through_their_zones_and_a_fight_keeps_its_parties() {
    let Ok(url) = std::env::var("GM_TEST_DATABASE_URL") else {
        eprintln!("SKIPPED: set GM_TEST_DATABASE_URL to a Postgres this test may wipe");
        return;
    };
    if let Ok(filter) = std::env::var("GM_TRACE") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    }
    let db = Db::connect(&url).await.expect("database");
    db.migrate().await.expect("migrations");
    db.wipe().await.expect("wipe");
    let content = gm_content::load_dir(Path::new(CONTENT), TickRate::COMBAT).expect("content");
    let items = gm_content::items::load_items(Path::new(CONTENT)).expect("items");

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
            party_sweep: Duration::from_millis(300),
            party_away: std::time::Duration::from_secs(2),
            items: items.clone(),
            looks: gm_content::looks::Looks::load_dir(Path::new(CONTENT)).expect("looks"),
            max_coin_grant: 500,
            models_dir: std::env::temp_dir()
                .join(format!("gm-party-models-{}", std::process::id())),
            ingest: IngestMode::InProcess,
            ingest_timeout: gm_hub::models::INGEST_TIMEOUT,
            start_zone: None,
            blurbs: Vec::new(),
        },
        db.clone(),
        endpoint,
        std::future::pending(),
    ));
    let town = zone("town", hub_addr, &hub_cert, &content).await;
    let dungeon = zone("dungeon", hub_addr, &hub_cert, &content).await;

    // --- The town: Ana and Bojan a hundred units apart, Cvita across the square.
    let ana = Someone::new(hub_addr, &hub_cert, "Ana").await;
    let bojan = Someone::new(hub_addr, &hub_cert, "Bojan").await;
    let cvita = Someone::new(hub_addr, &hub_cert, "Cvita").await;
    let grid = town.world.stall_grids[0];
    let tile = grid.centre(grid.base_x, grid.base_y).expect("a tile");
    let up = Vec3::Z * (1.0 - Hull::Player.mins().z);
    let (sin, cos) = grid.yaw.to_radians().sin_cos();
    let beside = tile + Vec3::new(cos, sin, 0.0) * 100.0;
    let far = tile + Vec3::new(cos, sin, 0.0) * 600.0;
    for (who, at) in [(&ana, tile), (&bojan, beside), (&cvita, far)] {
        assert_eq!(
            town.world.bsp.point_contents(Hull::Player, at + up),
            Contents::Empty,
            "{at:?} is a place to stand"
        );
        assert!(
            db.place(who.character, "town", (at + up).into(), 0.0)
                .await
                .unwrap()
        );
    }
    assert!((far - tile).length() > TRADE_REACH * 2.0);
    let ana_hand = ana.enter("town", "Ana", 0.0, town.map.clone()).await;
    let bojan_hand = bojan.enter("town", "Bojan", 0.0, town.map.clone()).await;
    let cvita_hand = cvita.enter("town", "Cvita", 0.0, town.map.clone()).await;
    let (ana_body, bojan_body) = (
        bojan_hand.entity_of("Ana").await,
        ana_hand.entity_of("Bojan").await,
    );
    assert_eq!((ana_body, bojan_body), (ana_hand.entity, bojan_hand.entity));

    // Nobody is in a party: nobody's health is on anybody's wire.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!bojan_hand.sees_health(ana_body) && !ana_hand.sees_health(bojan_body));

    // --- An invitation, by a name as a person writes it; and its answer.
    let invite = |name: &str| FromClient::PartyInvite { name: name.into() };
    assert_eq!(
        ana_hand.ask(invite("Zed")).await,
        "nobody called Zed is in the game"
    );
    assert_eq!(ana_hand.ask(invite("ANA")).await, "that is you");
    assert_eq!(ana_hand.ask(invite("x")).await, "nobody can be called that");
    assert_eq!(
        ana_hand.ask(invite("bojan")).await,
        "Bojan was asked to join"
    );
    let from = bojan_hand
        .hears("the invitation", |m| match m {
            FromZone::Invited { from } => Some(from.clone()),
            _ => None,
        })
        .await;
    assert_eq!(from, "Ana");
    assert_eq!(
        ana_hand.ask(invite("Bojan")).await,
        "Bojan has been asked already"
    );
    let answer = |from: &str, join| FromClient::PartyAnswer {
        from: from.into(),
        join,
    };
    assert_eq!(
        bojan_hand.ask(answer("Cvita", true)).await,
        "that invitation is gone"
    );
    let joined = "you are in a party: Ana, Bojan";
    assert_eq!(bojan_hand.ask(answer("ana", true)).await, joined);
    assert_eq!(ana_hand.line().await, joined);
    for hand in [&ana_hand, &bojan_hand] {
        assert_eq!(hand.party().await, ["Ana", "Bojan"]);
    }
    // The wire carries the health of the party, and of nobody else.
    ana_hand
        .until("a member's health", |s| carries(s, bojan_body))
        .await;
    bojan_hand
        .until("a member's health", |s| carries(s, ana_body))
        .await;
    assert!(!cvita_hand.sees_health(ana_body) && !ana_hand.sees_health(cvita_hand.entity));

    // --- Lines: the party's to the party, a whisper to one, both through the hub.
    bojan_hand.say(FromClient::PartySay("  ready?  ".into()));
    for hand in [&ana_hand, &bojan_hand] {
        assert_eq!(
            hand.heard_line(CHANNEL_PARTY).await,
            ("Bojan".to_string(), "ready?".to_string())
        );
    }
    let remove = |name: &str| FromClient::PartyRemove { name: name.into() };
    let whisper = |to: &str, text: &str| FromClient::Whisper {
        to: to.into(),
        text: text.into(),
    };
    cvita_hand.say(whisper("ana", "psst"));
    assert_eq!(
        ana_hand.heard_line(CHANNEL_WHISPER).await,
        ("Cvita".to_string(), "psst".to_string())
    );
    assert_eq!(
        cvita_hand.heard_line(CHANNEL_WHISPERED).await,
        ("Ana".to_string(), "psst".to_string())
    );
    let heard = |m: &FromZone| matches!(m, FromZone::Heard { .. });
    bojan_hand.hears_no("somebody else's whisper", heard).await;
    cvita_hand.hears_no("a party's line", heard).await;
    cvita_hand.say(whisper("Zed", "hello"));
    assert_eq!(cvita_hand.line().await, "nobody called Zed is in the game");
    cvita_hand.say(FromClient::PartySay("anybody?".into()));
    assert_eq!(cvita_hand.line().await, "you are in no party");
    // They are chat: an account's five lines are five of any kind, and the sixth is
    // refused to its sender alone (Cvita has said three).
    cvita_hand.say(whisper("Ana", "one"));
    cvita_hand.say(FromClient::Chat("two".into()));
    cvita_hand.say(whisper("Ana", "three"));
    assert_eq!(cvita_hand.line().await, "too many lines; wait a moment");
    assert_eq!(ana_hand.heard_line(CHANNEL_WHISPER).await.1, "one");
    ana_hand
        .hears_no(
            "the line over the limit",
            |m| matches!(m, FromZone::Heard { text, .. } if text == "three"),
        )
        .await;

    // --- A trade: asked standing together, opened when both have asked.
    let trade_with = |with: u32| FromClient::TradeAsk { with };
    assert_eq!(
        cvita_hand.ask(trade_with(ana_body)).await,
        "walk up to them to trade"
    );
    assert_eq!(
        cvita_hand.ask(trade_with(cvita_hand.entity)).await,
        "that is you"
    );
    assert_eq!(
        cvita_hand.ask(trade_with(9999)).await,
        "there is nobody there to trade with"
    );
    assert_eq!(
        bojan_hand.ask(trade_with(ana_body)).await,
        "you asked Ana to trade"
    );
    let asker = ana_hand
        .hears("a request to trade", |m| match m {
            FromZone::TradeAsked { from } => Some(*from),
            _ => None,
        })
        .await;
    assert_eq!(asker, bojan_body);
    // Asked again: the other is not told twice.
    assert_eq!(
        bojan_hand.ask(trade_with(ana_body)).await,
        "you asked Ana to trade"
    );
    ana_hand
        .hears_no("the same request again", |m| {
            matches!(m, FromZone::TradeAsked { .. })
        })
        .await;
    ana_hand.say_gated(trade_with(bojan_body)).await;
    let (trade, with) = ana_hand.opened().await;
    assert_eq!(with, "Bojan");
    assert_eq!(bojan_hand.opened().await, (trade, "Ana".to_string()));

    // The window is the hub's: Bojan gives a sword for Ana's coin.
    let direct = Economy::new(db.pool().clone());
    let iron = vec!["core/iron".to_string(), "frame/oak".to_string()];
    let sword = direct
        .grant_item(bojan.character, "sword", &iron, None)
        .await
        .unwrap();
    direct
        .grant_coin_as(ana.character, 500, "grant", 0)
        .await
        .unwrap();
    assert_eq!(
        bojan
            .econ(EconOp::TradeOfferItem { trade, item: sword })
            .await,
        Ok(EconReply::Done)
    );
    assert_eq!(
        ana.econ(EconOp::TradeSetCoin { trade, coin: 300 }).await,
        Ok(EconReply::Done)
    );
    let (state, version, wait_ms, with, together, mine, theirs) = ana.view(trade).await;
    assert_eq!(
        (state, with.as_str(), together),
        (TRADE_OPEN, "Bojan", true)
    );
    assert_eq!((mine.coin, theirs.items.len()), (300, 1));
    assert_eq!(theirs.items[0].what, "a weapon, 40 of 250");
    assert!(wait_ms > 2000 && wait_ms <= 3000, "{wait_ms}");
    assert_eq!(
        ana.econ(EconOp::TradeAccept { trade, version }).await,
        Err(HubError::Cooldown)
    );
    tokio::time::sleep(Duration::from_millis(wait_ms as u64 + 100)).await;
    assert_eq!(ana.view(trade).await.2, 0, "the three seconds are over");
    assert_eq!(
        ana.econ(EconOp::TradeAccept { trade, version }).await,
        Ok(EconReply::Trade { committed: false })
    );
    assert_eq!(
        bojan.econ(EconOp::TradeAccept { trade, version }).await,
        Ok(EconReply::Trade { committed: true })
    );
    assert_eq!(bojan.view(trade).await.0, TRADE_COMMITTED);
    assert_eq!(direct.inventory(ana.character).await.unwrap().1.len(), 1);
    assert_eq!(direct.inventory(bojan.character).await.unwrap().0, 300);

    // --- A second trade, and one of the two goes elsewhere: the party holds from one zone
    // to the next and its lines with it; the trade does not.
    assert_eq!(
        ana_hand.ask(trade_with(bojan_body)).await,
        "you asked Bojan to trade"
    );
    bojan_hand.say_gated(trade_with(ana_body)).await;
    let (second, _) = ana_hand.opened().await;
    assert!(second > trade);
    let (addr, cert, token) = bojan_hand.travel("dungeon").await;
    let bojan_hand = Hand::join(addr, &cert, token, "Bojan", 0.0, dungeon.map.clone()).await;
    assert_eq!(bojan_hand.party().await, ["Ana", "Bojan"]);
    bojan_hand.say(FromClient::PartySay("here".into()));
    assert_eq!(ana_hand.heard_line(CHANNEL_PARTY).await.1, "here");
    assert_eq!(bojan_hand.heard_line(CHANNEL_PARTY).await.1, "here");
    // The trade ended when the other zone claimed him: a trade is between two that
    // stand together, and none waits for a day when they meet again.
    let trade = second;
    let (state, version, _, _, together, ..) = ana.view(trade).await;
    assert_eq!((state, together), (TRADE_CANCELLED, false));
    // (An accept of a trade that is over is answered with its state.)
    assert_eq!(
        ana.econ(EconOp::TradeAccept { trade, version }).await,
        Err(HubError::Invalid("cancelled".into()))
    );

    // --- Leaving: the one who leaves and the one who is left are both told, each by its
    // own zone.
    assert_eq!(
        ana_hand.ask(FromClient::PartyLeave).await,
        "the party is no more"
    );
    assert!(ana_hand.party().await.is_empty());
    assert_eq!(bojan_hand.line().await, "the party is no more");
    assert!(bojan_hand.party().await.is_empty());
    assert_eq!(
        ana_hand.ask(FromClient::PartyLeave).await,
        "you are in no party"
    );
    drop(bojan_hand);

    // --- The dungeon: two in one party walk into the sentinels (in plate: the fight is
    // to last). In the middle of it a third is asked in and one of the two is removed.
    // The hub's party changes at once both times; the bodies fight on as the fight
    // began (PARTY.md 2): its roster is closed, and nobody changes sides.
    let dane = Someone::of(hub_addr, &hub_cert, "Dane", "ironclad").await;
    let ema = Someone::of(hub_addr, &hub_cert, "Ema", "ironclad").await;
    let filip = Someone::new(hub_addr, &hub_cert, "Filip").await;
    // Round the corner from the gate room (the passage's second leg), out of the
    // sentinels' sight for as long as it takes to make a party; the third stays there.
    let north = 90.0;
    for (who, at) in [
        (&dane, Vec3::new(-940.0, 300.0, 0.0)),
        (&ema, Vec3::new(-1040.0, 300.0, 0.0)),
        (&filip, Vec3::new(-992.0, 180.0, 0.0)),
    ] {
        assert_eq!(
            dungeon.world.bsp.point_contents(Hull::Player, at + up),
            Contents::Empty,
            "{at:?} is a place to stand"
        );
        assert!(
            db.place(who.character, "dungeon", (at + up).into(), north)
                .await
                .unwrap()
        );
    }
    let facing = north;
    let dane_hand = dane
        .enter("dungeon", "Dane", facing, dungeon.map.clone())
        .await;
    let ema_hand = ema
        .enter("dungeon", "Ema", facing, dungeon.map.clone())
        .await;
    let filip_hand = filip
        .enter("dungeon", "Filip", facing, dungeon.map.clone())
        .await;
    assert_eq!(dane_hand.ask(invite("Ema")).await, "Ema was asked to join");
    let joined = "you are in a party: Dane, Ema";
    assert_eq!(ema_hand.ask(answer("Dane", true)).await, joined);
    assert_eq!(dane_hand.line().await, joined);
    for hand in [&dane_hand, &ema_hand] {
        assert_eq!(hand.party().await, ["Dane", "Ema"]);
    }
    let dane_body = ema_hand.entity_of("Dane").await;
    ema_hand
        .until("a member's health", |s| carries(s, dane_body))
        .await;
    // Up the passage to its corner, then east along it into the gate room, swinging; a
    // moment after the fight begins both have dealt or taken a blow.
    dane_hand.go(1.0, 0);
    ema_hand.go(1.0, 0);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for hand in [&dane_hand, &ema_hand] {
        hand.shared.lock().unwrap().yaw = 0.0;
        hand.go(1.0, buttons::PRIMARY);
    }
    ema_hand.engaged().await;
    for hand in [&dane_hand, &ema_hand] {
        hand.within(Duration::from_secs(20), "a blow", |s| s.own < s.full)
            .await;
    }
    // A third joins the party: for the hub at once, for the zone when the fight is over.
    assert_eq!(
        dane_hand.ask(invite("Filip")).await,
        "Filip was asked to join"
    );
    assert_eq!(
        filip_hand.ask(answer("Dane", true)).await,
        "you are in a party: Dane, Ema, Filip"
    );
    assert_eq!(filip_hand.party().await, ["Dane", "Ema", "Filip"]);
    assert_eq!(
        filip_hand.line().await,
        "your party is in a fight: you are of it when the fight is over"
    );
    assert_eq!(dane_hand.party().await, ["Dane", "Ema", "Filip"]);
    assert_eq!(ema_hand.party().await, ["Dane", "Ema", "Filip"]);
    // One of the two is removed: told all of it, and her body fights on as it began.
    assert_eq!(
        dane_hand.ask(remove("ema")).await,
        "ema is out of the party"
    );
    assert_eq!(dane_hand.party().await, ["Dane", "Filip"]);
    assert_eq!(ema_hand.line().await, "you are out of the party");
    assert!(ema_hand.party().await.is_empty());
    assert_eq!(
        ema_hand.line().await,
        "the party changes for you when this fight is over"
    );
    assert!(
        ema_hand.sees_health(dane_body),
        "still one party to the zone"
    );
    // They walk away (or are finished off): when the fight is over, the bodies carry
    // what the hub has held all along, and both who waited are told.
    dane_hand.go(-1.0, 0);
    ema_hand.go(-1.0, 0);
    over(&ema_hand, "the fight is over: you are on your own now").await;
    over(&filip_hand, "the fight is over: you are of the party now").await;
    // And the wire follows: no health of somebody who is no member any more.
    ema_hand
        .until("the wire to follow", |s| !carries(s, dane_body))
        .await;

    // --- Nobody who left a fight comes back into it. Somebody appears in the sentinels'
    // room, takes a blow, and loses the connection: the zone does not let that character
    // in again until the fight it left is over.
    let gita = Someone::of(hub_addr, &hub_cert, "Gita", "ironclad").await;
    let before_them = Vec3::new(-300.0, 544.0, 0.0);
    assert_eq!(
        dungeon
            .world
            .bsp
            .point_contents(Hull::Player, before_them + up),
        Contents::Empty
    );
    let placed = db.place(gita.character, "dungeon", (before_them + up).into(), 0.0);
    assert!(placed.await.unwrap());
    let gita_hand = gita
        .enter("dungeon", "Gita", 0.0, dungeon.map.clone())
        .await;
    gita_hand.engaged().await;
    gita_hand
        .within(Duration::from_secs(20), "a blow", |s| s.own < s.full)
        .await;
    // (A second connection for the same character while its body fights would be
    // turned away without ending the fight; the hub gives no ticket for a character
    // that plays somewhere, so there is no way to it from here.)
    gita_hand.drop_connection().await;
    let again = gita
        .try_enter("dungeon", "Gita", 0.0, dungeon.map.clone())
        .await;
    assert_eq!(
        again.err().as_deref(),
        Some("the fight you left here is not over: come back when it is")
    );
    // The sentinels lose her, the encounter resets, and she is let in.
    let started = Instant::now();
    let back = loop {
        match gita
            .try_enter("dungeon", "Gita", 0.0, dungeon.map.clone())
            .await
        {
            Ok(hand) => break hand,
            Err(why) => {
                assert!(why.starts_with("the fight you left"), "{why}");
                assert!(started.elapsed() < Duration::from_secs(60), "never let in");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    };
    drop(back);
}
