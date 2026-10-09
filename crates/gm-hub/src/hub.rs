//! The hub process (HUB.md): accounts, sessions, the zone registry, tickets, claims, saves and
//! handoffs. One QUIC endpoint for clients and zones; one task per connection, one task per
//! request stream; the database does the invariants.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use gm_core::build::{Build, ContentPack};
use gm_core::tick::TickRate;
use gm_net::control::{self, WebAddr, valid_name};
use gm_net::link::{Link, RecvHalf, SendHalf, WebEndpoint, web_accept};
use gm_net::transport::fnv1a64;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::db::Db;
use gm_hub_proto::protocol::{
    AccountId, BuildChoice, CHANNEL_PARTY, CHANNEL_WHISPER, CLOCK_SKEW_SECS, CharacterId,
    ContractOutcome, EconOp, EconReply, GearReading, HASH_PERMITS, HUB_PREAMBLE, HUB_VERSION,
    HiredAvatar, HubError, HubNotice, HubRequest, HubResponse, ItemSummary, ListingSummary,
    LocationSummary, MAX_REPLAY_BYTES, MAX_SESSIONS_PER_ACCOUNT, ModOp, ModelRef, PLACE_ARMOUR,
    PLACE_NONE, PLACE_WEAPON, PartyNews, PartyReply, SESSION_LIFETIMES, SayTo, SessionId,
    StackReading, StallSummary, TOKEN_VALID_SECS, TavernEntry, TradeOffer, ZoneEconOp, ZoneId,
    ZonePartyOp, ZoneSummary, ZoneTicket, now_secs,
};

use crate::conduct::Conduct;
use crate::economy::{EconError, Economy, Outcome, TradeState, TradeStatus};
use crate::models::{IngestMode, Models};
use crate::party::{Answered, Parties};
use gm_content::items::{ItemContent, Place};
use gm_hub_proto::protocol::{TRADE_CANCELLED, TRADE_COMMITTED, TRADE_OPEN};

/// The body of an upload must arrive within ten seconds plus its length at 64 KiB/s
/// (MODELS.md 6.2): 15 s for 300 KB, 138 s for the 8 MiB limit. A slow sender holds one of
/// the upload slots for that long at most, never a worker.
fn upload_body_timeout(len: u32) -> Duration {
    Duration::from_secs(10) + Duration::from_secs_f64(len as f64 / 65_536.0)
}

/// A zone whose last heartbeat is older than this gets no new players (HUB.md 2).
const ZONE_STALE: Duration = Duration::from_secs(15);
/// A notice that is sent without waiting for it is given up after this.
const NOTICE_PATIENCE: Duration = Duration::from_secs(5);
use gm_hub_proto::player::{self, PlayerRequest, PlayerResponse};
use gm_hub_proto::token::{HubKey, TokenVerifier};

pub struct HubConfig {
    pub zone_secret: String,
    pub content: ContentPack,
    pub key: HubKey,
    /// Idle sessions expire after this.
    pub session_secs: u64,
    /// `Register`/`Login` attempts per minute per source address.
    pub auth_per_minute: f64,
    /// Economy requests one account may make a second, with four seconds' worth in hand
    /// (ITEMS.md 4): each is a transaction, and a session is all it takes to ask.
    pub econ_per_second: f64,
    /// How often the hub looks at parties whose members left the game, and how long such
    /// a member is still of its party (PARTY.md 2; `party::SWEEP` and `party::AWAY`
    /// outside tests).
    pub party_sweep: Duration,
    pub party_away: Duration,
    /// The item content (`assets/content/items.toml`): the templates a craft may name
    /// (none listed: any), and what a worn item does (ITEMS.md 3.2).
    pub items: gm_content::items::ItemContent,
    /// The looks of the content (CONTENT.md 3), loaded beside the pack as a zone loads
    /// them: the prop each ability holds says which weapons a build may wear (ITEMS.md 2).
    pub looks: gm_content::looks::Looks,
    /// The largest coin drop a zone may report in one grant, in silver (ECONOMY.md 9).
    pub max_coin_grant: i64,
    /// Where ingested models, their previews and the uploads live (MODELS.md 6.1).
    pub models_dir: std::path::PathBuf,
    /// How uploads are parsed: a worker process in production.
    pub ingest: IngestMode,
    /// Wall clock of one ingestion (`models::INGEST_TIMEOUT` outside tests).
    pub ingest_timeout: Duration,
    /// Where a character that names no zone and was in none enters (CLIENT.md 7); `None`:
    /// the zone called `town` if there is one, else the first by name.
    pub start_zone: Option<ZoneId>,
    /// What each preset build of the content is, in plain words, in the content's order
    /// (`gm_content::load_blurbs`).
    pub blurbs: Vec<String>,
}

struct Session {
    account: AccountId,
    /// When it was last used: a session ends `session_secs` after that.
    seen: Instant,
    /// When it began: it ends `SESSION_LIFETIMES` of those after that, however much it
    /// is used (a session id that got out is not good for ever).
    created: Instant,
}

struct ZoneEntry {
    addr: SocketAddr,
    cert_der: Vec<u8>,
    web: Option<WebAddr>,
    /// The least trust tier the zone admits (ANTICHEAT.md 6).
    min_trust: i16,
    /// How many clients it takes.
    max_players: u32,
    map: String,
    #[allow(dead_code)]
    map_hash: u64,
    players: u32,
    #[allow(dead_code)]
    tick_mean_us: f32,
    since: Instant,
    last_heartbeat: Instant,
    conn: Link,
    /// Trials that open the zone; empty = open to all (COMPANIONS.md 11).
    requires: Vec<String>,
}

/// The most trials a zone may name as its key.
const MAX_ZONE_REQUIRES: usize = 16;

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    /// One token, if there is one: `per_sec` come back a second, up to `full`.
    fn take(&mut self, now: Instant, per_sec: f64, full: f64) -> bool {
        self.tokens =
            (self.tokens + now.duration_since(self.last).as_secs_f64() * per_sec).min(full);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[derive(Default)]
struct State {
    sessions: HashMap<SessionId, Session>,
    zones: HashMap<ZoneId, ZoneEntry>,
    buckets: HashMap<IpAddr, Bucket>,
    econ_buckets: HashMap<AccountId, Bucket>,
    last_sweep: Option<Instant>,
}

impl State {
    /// Buckets nobody has used for ten minutes are forgotten, once a minute.
    fn sweep_buckets(&mut self, now: Instant) {
        if self
            .last_sweep
            .is_none_or(|t| now.duration_since(t) > Duration::from_secs(60))
        {
            let fresh = |b: &Bucket| now.duration_since(b.last) < Duration::from_secs(600);
            self.buckets.retain(|_, b| fresh(b));
            self.econ_buckets.retain(|_, b| fresh(b));
            self.last_sweep = Some(now);
        }
    }
}

struct Hub {
    cfg: HubConfig,
    db: Db,
    econ: Economy,
    parties: Parties,
    models: Models,
    conduct: Conduct,
    state: Mutex<State>,
    hashing: Semaphore,
    /// The hash a login for an email nobody registered is checked against: of a password
    /// nobody knows, made when the hub starts.
    decoy_hash: String,
    /// Verifies our own tokens on `Claim` (defence in depth; the zone verified too).
    verifier: Mutex<HashMap<ZoneId, TokenVerifier>>,
}

/// What a connection has proven about itself.
#[derive(Default)]
struct ConnAuth {
    zone: Mutex<Option<ZoneId>>,
}

pub async fn run(
    cfg: HubConfig,
    db: Db,
    endpoint: quinn::Endpoint,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    run_with_web(cfg, db, endpoint, None, shutdown).await
}

/// `run`, with a WebTransport listener for browsers beside the QUIC endpoint (WEB.md 2.1):
/// the endpoint and the `Origin`s a session may come from (empty = any).
pub async fn run_with_web(
    cfg: HubConfig,
    db: Db,
    endpoint: quinn::Endpoint,
    web: Option<(WebEndpoint, Vec<String>)>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let models = Models::new(
        db.pool().clone(),
        &cfg.models_dir,
        cfg.ingest.clone(),
        cfg.ingest_timeout,
    )?;
    let conduct = Conduct::new(db.pool().clone(), &cfg.models_dir.join("replays"))?;
    let decoy_hash = {
        let (mut secret, mut salt) = ([0u8; 32], [0u8; 16]);
        rand::fill(&mut secret);
        rand::fill(&mut salt);
        Argon2::default()
            .hash_password_with_salt(&secret, &salt)
            .map(|h: PasswordHash| h.to_string())
            .map_err(|e| anyhow::anyhow!("hashing: {e}"))?
    };
    let mut parties = Parties::new(db.pool().clone());
    parties.away = cfg.party_away;
    let econ = Economy::new(db.pool().clone()).with_stacks(&cfg.items);
    let hub = Arc::new(Hub {
        conduct,
        decoy_hash,
        hashing: Semaphore::new(HASH_PERMITS),
        verifier: Mutex::new(HashMap::new()),
        models,
        cfg,
        econ,
        parties,
        db,
        state: Mutex::new(State::default()),
    });
    info!(listen = %endpoint.local_addr()?, "hub listening");
    // A character that went offline leaves its party (PARTY.md 2), whichever way it went.
    let party_sweeper = {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(hub.cfg.party_sweep);
            loop {
                every.tick().await;
                match hub.parties.sweep().await {
                    Ok(changes) => {
                        for news in changes {
                            info!(party = news.party.id, leader = news.party.leader, left = ?news.left, "the sweep changed a party");
                            hub.tell_party(&news).await;
                        }
                    }
                    Err(e) => warn!("party sweep: {e}"),
                }
            }
        })
    };
    // Stalls past their 48 h close and stalled contracts refund, once a minute.
    let sweeper = {
        let hub = hub.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(60));
            loop {
                every.tick().await;
                match hub.econ.stalls_expired().await {
                    Ok(owners) => {
                        for owner in owners {
                            match hub.econ.stall_close(owner).await {
                                Ok((stall, zone)) => {
                                    hub.notify(&zone, HubNotice::StallClosed { stall }).await;
                                }
                                Err(e) => warn!(owner, "closing an expired stall: {e}"),
                            }
                        }
                    }
                    Err(e) => warn!("stall sweep: {e}"),
                }
                if let Err(e) = hub.econ.contracts_expire().await {
                    warn!("contract sweep: {e}");
                }
                // A trade nobody touches is called off (PARTY.md 6).
                match hub.econ.trades_expire().await {
                    Ok(0) => {}
                    Ok(n) => info!(trades = n, "trades nobody touched were called off"),
                    Err(e) => warn!("trade sweep: {e}"),
                }
                // A ticket nobody used: its character is offline again (HUB.md 3.8).
                match hub
                    .db
                    .sweep_transits(TOKEN_VALID_SECS + CLOCK_SKEW_SECS)
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => info!(characters = n, "transits nobody arrived from ended"),
                    Err(e) => warn!("transit sweep: {e}"),
                }
                hub.sweep_sessions();
                // Replays past their retention go (ANTICHEAT.md 3.3).
                match hub.conduct.sweep().await {
                    Ok(0) => {}
                    Ok(n) => info!(replays = n, "replays past their retention deleted"),
                    Err(e) => warn!("replay sweep: {e}"),
                }
            }
        })
    };
    let accept = {
        let hub = hub.clone();
        let endpoint = endpoint.clone();
        async move {
            while let Some(incoming) = endpoint.accept().await {
                let hub = hub.clone();
                tokio::spawn(async move {
                    let remote = incoming.remote_address();
                    match incoming.await {
                        Ok(conn) => handle_connection(hub, Link::Quic(conn)).await,
                        Err(e) => debug!(%remote, "handshake failed: {e}"),
                    }
                });
            }
        }
    };
    // Browsers: the same requests on the streams of a WebTransport session.
    let accept_web = {
        let hub = hub.clone();
        async move {
            let Some((endpoint, origins)) = web else {
                return std::future::pending::<()>().await;
            };
            loop {
                let incoming = web_accept(&endpoint, &origins).await;
                let hub = hub.clone();
                tokio::spawn(async move {
                    let remote = incoming.remote_address();
                    match tokio::time::timeout(Duration::from_secs(5), incoming.accept()).await {
                        Ok(Ok(conn)) => handle_connection(hub, conn).await,
                        Ok(Err(e)) => debug!(%remote, "web session refused: {e}"),
                        Err(_) => debug!(%remote, "web session: no request in time"),
                    }
                });
            }
        }
    };
    tokio::select! {
        _ = accept => {}
        _ = accept_web => {}
        _ = shutdown => {}
    }
    sweeper.abort();
    party_sweeper.abort();
    endpoint.close(0u32.into(), b"hub stopped");
    info!("hub stopped");
    Ok(())
}

async fn handle_connection(hub: Arc<Hub>, conn: Link) {
    let remote = conn.remote_address();
    let auth = Arc::new(ConnAuth::default());
    debug!(%remote, "connection");
    loop {
        let (send, recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(e) => {
                debug!(%remote, "connection ended: {e}");
                break;
            }
        };
        let hub = hub.clone();
        let auth = auth.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_stream(hub, auth, conn, remote, send, recv).await {
                debug!(%remote, "request failed: {e}");
            }
        });
    }
    // A zone that disconnects takes its players with it: they go offline.
    let zone = auth.zone.lock().unwrap().clone();
    if let Some(zone) = zone {
        let removed = {
            let mut st = hub.state.lock().unwrap();
            match st.zones.get(&zone) {
                Some(entry) if entry.conn.stable_id() == conn.stable_id() => {
                    st.zones.remove(&zone);
                    true
                }
                _ => false,
            }
        };
        if removed {
            match hub.db.offline_zone(&zone).await {
                Ok(n) => info!(%zone, players = n, "zone disconnected; its players are offline"),
                Err(e) => warn!(%zone, "offline_zone failed: {e}"),
            }
            hub.db.log(&zone, "disconnected", "").await;
        }
    }
}

/// How long a stream may take to say what it wants.
const REQUEST_HEAD_TIMEOUT: Duration = Duration::from_secs(15);

async fn handle_stream(
    hub: Arc<Hub>,
    auth: Arc<ConnAuth>,
    conn: Link,
    remote: SocketAddr,
    mut send: SendHalf,
    mut recv: RecvHalf,
) -> anyhow::Result<()> {
    // What a stream begins with says what it speaks (HUB.md 3, 3.8): a frame of one byte,
    // the version of the hub's messages (zones, tools, bots); or an empty frame and then
    // the version of the players' messages. The hub's own version goes back before
    // anything else: a peer of another build learns that much whatever else changed. A
    // stream that says nothing for a while is dropped.
    let head = async {
        let Some(first) = control::recv_frame(&mut recv).await? else {
            return Ok(None);
        };
        let player = first.is_empty();
        let (version, ours, preamble) = if player {
            let Some(version) = control::recv_frame(&mut recv).await? else {
                return Ok(None);
            };
            (version, player::PLAYER_VERSION, &player::HUB_PREAMBLE)
        } else {
            (first, HUB_VERSION, &HUB_PREAMBLE)
        };
        send.write_all(preamble).await?;
        if version != [ours] {
            return Ok(None);
        }
        let Some(payload) = control::recv_frame(&mut recv).await? else {
            return Ok(None);
        };
        let req: HubRequest = if player {
            bitcode::decode::<PlayerRequest>(&payload)
                .map_err(control::ControlError::from)?
                .into()
        } else {
            bitcode::decode(&payload).map_err(control::ControlError::from)?
        };
        anyhow::Ok(Some((req, player)))
    };
    let Some((req, player)) = tokio::time::timeout(REQUEST_HEAD_TIMEOUT, head)
        .await
        .map_err(|_| anyhow::anyhow!("{remote}: a stream that asked nothing"))??
    else {
        let _ = send.finish();
        return Ok(());
    };
    // Requests that carry or are answered with raw bytes on the stream (MODELS.md 6.2).
    let resp = match req {
        HubRequest::ModelUpload {
            session,
            frame,
            tos_version,
            len,
        } => {
            let r = upload(&hub, session, frame, tos_version, len, &mut recv).await;
            if r.is_err() {
                // Refused, perhaps before the body was read: tell the sender to stop.
                recv.stop(0);
            }
            r.unwrap_or_else(HubResponse::Err)
        }
        HubRequest::ModelGet { session, model } => {
            let blob = async {
                let account = hub.session_account(session)?;
                hub.models.get(session, account, &model).await
            }
            .await;
            return send_blob(&mut send, blob, player).await;
        }
        // A zone's replay: the summary, then the file (ANTICHEAT.md 3.3).
        HubRequest::ZoneReplay { summary, len } => {
            let stored = async {
                let zone = hub.zone_of_conn(&auth)?;
                if len == 0 || len > MAX_REPLAY_BYTES {
                    return Err(HubError::Invalid("replay size".into()));
                }
                let mut body = vec![0u8; len as usize];
                match tokio::time::timeout(upload_body_timeout(len), recv.read_exact(&mut body))
                    .await
                {
                    Ok(Ok(())) => {}
                    _ => return Err(HubError::Invalid("the replay was cut short".into())),
                }
                hub.conduct.replay_put(&zone, &summary, body).await
            }
            .await;
            match stored {
                Ok(id) => HubResponse::ReplayStored { id },
                Err(e) => {
                    recv.stop(0);
                    HubResponse::Err(e)
                }
            }
        }
        HubRequest::Mod {
            session,
            op: ModOp::ReplayGet { id },
        } => {
            let blob = async {
                let moderator = hub.moderator(session).await?;
                hub.conduct.replay_get(moderator, id).await
            }
            .await;
            return send_blob(&mut send, blob, false).await;
        }
        HubRequest::Mod {
            session,
            op: ModOp::Preview { model },
        } => {
            let blob = async {
                hub.moderator(session).await?;
                hub.models.preview(&model)
            }
            .await;
            return send_blob(&mut send, blob, false).await;
        }
        req => match handle(&hub, &auth, &conn, remote, req).await {
            Ok(r) => r,
            Err(e) => HubResponse::Err(e),
        },
    };
    answer(&mut send, resp, player).await?;
    let _ = send.finish();
    Ok(())
}

/// Write the answer in the encoding the request came in.
async fn answer(
    send: &mut quinn::SendStream,
    resp: HubResponse,
    player: bool,
) -> Result<(), control::ControlError> {
    if player {
        // An answer no player's request has would be this hub's own mistake.
        let resp =
            PlayerResponse::try_from(resp).unwrap_or(PlayerResponse::Err(HubError::Internal));
        control::send_any(send, &resp).await
    } else {
        control::send_any(send, &resp).await
    }
}

/// Answer with `Blob { len }` and the bytes, or with the error.
async fn send_blob(
    send: &mut quinn::SendStream,
    blob: Result<Vec<u8>, HubError>,
    player: bool,
) -> anyhow::Result<()> {
    match blob {
        Ok(bytes) => {
            let len = bytes.len() as u32;
            answer(send, HubResponse::Blob { len }, player).await?;
            send.write_all(&bytes).await?;
        }
        Err(e) => answer(send, HubResponse::Err(e), player).await?,
    }
    let _ = send.finish();
    Ok(())
}

/// `ModelUpload`: everything that can be refused is refused before the body is read. One
/// upload per account at a time and at most `UPLOAD_SLOTS` bodies in memory; a worker is taken
/// only once the body is complete, so a slow sender never keeps one idle.
async fn upload(
    hub: &Hub,
    session: SessionId,
    frame: u8,
    tos_version: u16,
    len: u32,
    recv: &mut quinn::RecvStream,
) -> Result<HubResponse, HubError> {
    let account = hub.session_account(session)?;
    let _turn = hub.models.begin_upload(account)?;
    hub.models
        .precheck(account, frame, tos_version, len)
        .await?;
    let mut body = vec![0u8; len as usize];
    match tokio::time::timeout(upload_body_timeout(len), recv.read_exact(&mut body)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(HubError::Invalid(format!("the upload was cut short: {e}"))),
        Err(_) => return Err(HubError::Invalid("the upload took too long".into())),
    }
    let (model, status) = hub.models.upload(account, frame, body).await?;
    Ok(HubResponse::ModelAccepted { model, status })
}

impl Hub {
    /// Whether a session has run out: idle for too long, or too old.
    fn expired(&self, s: &Session) -> bool {
        let ttl = Duration::from_secs(self.cfg.session_secs);
        s.seen.elapsed() >= ttl || s.created.elapsed() >= ttl * SESSION_LIFETIMES
    }

    fn session_account(&self, session: SessionId) -> Result<AccountId, HubError> {
        let mut st = self.state.lock().unwrap();
        // A session that is used lives on: a client left in a zone overnight still has
        // one in the morning.
        match st.sessions.get_mut(&session) {
            Some(s) if !self.expired(s) => {
                s.seen = Instant::now();
                Ok(s.account)
            }
            Some(_) => {
                st.sessions.remove(&session);
                Err(HubError::Unauthorized)
            }
            None => Err(HubError::Unauthorized),
        }
    }

    /// Sessions that have run out are forgotten (once a minute, by the sweeper).
    fn sweep_sessions(&self) {
        let mut st = self.state.lock().unwrap();
        let before = st.sessions.len();
        st.sessions.retain(|_, s| !self.expired(s));
        let gone = before - st.sessions.len();
        if gone > 0 {
            debug!(gone, "sessions ended");
        }
    }

    fn new_session(&self, account: AccountId) -> SessionId {
        let mut id = [0u8; 16];
        rand::fill(&mut id);
        let id = SessionId(id);
        let mut st = self.state.lock().unwrap();
        // An account has so many sessions at once: the one used longest ago gives way.
        let mut own: Vec<(SessionId, Instant)> = st
            .sessions
            .iter()
            .filter(|(_, s)| s.account == account)
            .map(|(id, s)| (*id, s.seen))
            .collect();
        own.sort_by_key(|(_, seen)| *seen);
        while own.len() >= MAX_SESSIONS_PER_ACCOUNT {
            st.sessions.remove(&own.remove(0).0);
        }
        st.sessions.insert(
            id,
            Session {
                account,
                seen: Instant::now(),
                created: Instant::now(),
            },
        );
        id
    }

    /// Token bucket per source address for `Register` and `Login`.
    fn auth_allowed(&self, ip: IpAddr) -> bool {
        let mut st = self.state.lock().unwrap();
        let per_min = self.cfg.auth_per_minute.max(1.0);
        let now = Instant::now();
        st.sweep_buckets(now);
        let b = st.buckets.entry(ip).or_insert(Bucket {
            tokens: per_min,
            last: now,
        });
        b.take(now, per_min / 60.0, per_min)
    }

    /// Token bucket per account for the economy's requests: each is a transaction, and
    /// some read a row per item.
    fn econ_allowed(&self, account: AccountId) -> bool {
        let mut st = self.state.lock().unwrap();
        let per_sec = self.cfg.econ_per_second.max(1.0);
        let burst = per_sec * 4.0;
        let now = Instant::now();
        st.sweep_buckets(now);
        let b = st.econ_buckets.entry(account).or_insert(Bucket {
            tokens: burst,
            last: now,
        });
        b.take(now, per_sec, burst)
    }

    /// A preset name or a full build, validated against the hub's content (MATRIX.md 9).
    fn resolve_build(&self, choice: BuildChoice) -> Result<Build, HubError> {
        let build = match choice {
            BuildChoice::Preset(name) => self
                .cfg
                .content
                .build(&name)
                .cloned()
                .ok_or_else(|| HubError::Invalid(format!("unknown preset {name:?}")))?,
            BuildChoice::Custom(b) => b,
        };
        build
            .validate(&self.cfg.content)
            .map_err(|e| HubError::Invalid(e.to_string()))?;
        Ok(build)
    }

    /// The zones an `Enter` that names none may go to, best first: where the character was,
    /// the start zone, `town`, then whatever else is up, by name. Only zones that are up;
    /// from a browser, only zones a browser can reach. (Whether one has room is asked of
    /// each in turn, `zone_open`: a heartbeat's count is seconds old.)
    fn default_zones(&self, last: Option<&str>, web: bool) -> Vec<ZoneId> {
        let st = self.state.lock().unwrap();
        let open = |id: &str| {
            st.zones.get(id).is_some_and(|z| {
                z.last_heartbeat.elapsed() < ZONE_STALE && (!web || z.web.is_some())
            })
        };
        let mut order: Vec<ZoneId> = [last, self.cfg.start_zone.as_deref(), Some("town")]
            .into_iter()
            .flatten()
            .map(str::to_string)
            .collect();
        let mut rest: Vec<&ZoneId> = st.zones.keys().collect();
        rest.sort();
        order.extend(rest.into_iter().cloned());
        let mut seen = std::collections::HashSet::new();
        order
            .into_iter()
            .filter(|id| seen.insert(id.clone()) && open(id))
            .collect()
    }

    /// Whether a client may be sent to `zone` now: it is up, it has room (counted from the
    /// characters the database has in it: a heartbeat is seconds old), and a browser can
    /// reach it if the client is one.
    async fn zone_open(&self, zone: &ZoneId, web: bool, who: CharacterId) -> Result<(), HubError> {
        let max_players = {
            let st = self.state.lock().unwrap();
            let z = st
                .zones
                .get(zone)
                .filter(|z| z.last_heartbeat.elapsed() < ZONE_STALE)
                .ok_or(HubError::NotFound)?;
            if web && z.web.is_none() {
                return Err(HubError::Invalid(
                    "that zone cannot be reached from a browser".into(),
                ));
            }
            z.max_players
        };
        if self.db.zone_population(zone, who).await? >= max_players as i64 {
            return Err(HubError::Full);
        }
        Ok(())
    }

    /// The gate of a zone (COMPANIONS.md 11): a character that passed none of the trials the
    /// zone names is not let in, by `Enter` or by a handoff.
    async fn gate(&self, zone: &ZoneId, character: CharacterId) -> Result<(), HubError> {
        let (requires, min_trust) = self
            .state
            .lock()
            .unwrap()
            .zones
            .get(zone)
            .map(|z| (z.requires.clone(), z.min_trust))
            .unwrap_or_default();
        // A banned account enters nothing; a zone may ask for a trust tier (ANTICHEAT.md 6).
        let account = self
            .db
            .character(character)
            .await?
            .ok_or(HubError::NotFound)?
            .account_id;
        self.conduct.check(account).await?;
        if min_trust > 0 {
            self.conduct.promote(account).await?;
        }
        if min_trust > 0 && self.conduct.trust_tier(account).await? < min_trust {
            return Err(HubError::Locked(format!("trust tier {min_trust}")));
        }
        if requires.is_empty() {
            return Ok(());
        }
        let passed = self.db.trials_of(character).await?;
        if passed.iter().any(|(t, _)| requires.contains(t)) {
            Ok(())
        } else {
            Err(HubError::Locked(requires.join(", ")))
        }
    }

    /// The active hires of `hirer` as its zone and its client are told (COMPANIONS.md 3.3):
    /// oldest first, at most `capacity`. A stored build that no longer validates against
    /// the content is left out rather than sent into a zone.
    async fn hired(
        &self,
        hirer: CharacterId,
        capacity: usize,
    ) -> Result<Vec<HiredAvatar>, HubError> {
        let mut out = Vec::new();
        for h in self.econ.squad(hirer).await.map_err(econ_err)? {
            if out.len() >= capacity {
                break;
            }
            let Ok(build) = serde_json::from_value::<Build>(h.build) else {
                continue;
            };
            // A hire made under older rules is read under the current ones (MATRIX.md 9.1).
            let build = build.repaired(&self.cfg.content).unwrap_or(build);
            let Some((what, role)) = build_words(&self.cfg.content, &build) else {
                continue;
            };
            out.push(HiredAvatar {
                hire: h.id,
                character: h.avatar,
                name: h.name,
                what,
                role,
                model: self
                    .models
                    .worn(h.avatar)
                    .await?
                    .filter(|m| m.frame == gm_model::rig::frame_index(build.frame)),
                build,
                expires_at: h.expires_unix.max(0) as u64,
            });
        }
        Ok(out)
    }

    fn zone_of_conn(&self, auth: &ConnAuth) -> Result<ZoneId, HubError> {
        auth.zone
            .lock()
            .unwrap()
            .clone()
            .ok_or(HubError::Unauthorized)
    }

    fn ticket_for(
        &self,
        zone: &ZoneId,
        account: AccountId,
        character: CharacterId,
    ) -> Result<ZoneTicket, HubError> {
        let st = self.state.lock().unwrap();
        let entry = st.zones.get(zone).ok_or(HubError::NotFound)?;
        Ok(ZoneTicket {
            zone: zone.clone(),
            addr: entry.addr,
            cert_der: entry.cert_der.clone(),
            web: entry.web.clone(),
            token: self.cfg.key.issue(account, character, zone),
        })
    }

    /// The account of a session that is a moderator's.
    async fn moderator(&self, session: SessionId) -> Result<AccountId, HubError> {
        let account = self.session_account(session)?;
        if self.models.is_moderator(account).await? {
            Ok(account)
        } else {
            Err(HubError::Unauthorized)
        }
    }

    /// Send a notice to every connected zone.
    async fn notify_all(&self, notice: HubNotice) {
        let zones: Vec<ZoneId> = self.state.lock().unwrap().zones.keys().cloned().collect();
        for zone in zones {
            self.notify(&zone, notice.clone()).await;
        }
    }

    /// Send a notice to a zone, if it is connected, without waiting for it: what a zone
    /// that is slow to take notices costs is that zone's own (a line of chat, the news of
    /// a party), not the request that caused the notice.
    fn notify_later(&self, zone: &ZoneId, notice: HubNotice) {
        let conn = self
            .state
            .lock()
            .unwrap()
            .zones
            .get(zone)
            .map(|z| z.conn.clone());
        if let Some(conn) = conn {
            tokio::spawn(async move {
                let send = async {
                    if let Ok(mut uni) = conn.open_uni().await {
                        let _ = control::send_any(&mut uni, &notice).await;
                        let _ = uni.finish();
                    }
                };
                let _ = tokio::time::timeout(NOTICE_PATIENCE, send).await;
            });
        }
    }

    /// Tell every zone where a party's change concerns somebody (PARTY.md 3.2): where
    /// its members and those who left by it are now, the change being committed. The
    /// zone that asked is told too: its answer may be late or lost, and the notice is
    /// the same news with the same number.
    async fn tell_party(&self, news: &PartyNews) {
        let concerned: Vec<CharacterId> = news
            .party
            .members
            .iter()
            .map(|(id, _)| *id)
            .chain(news.left.iter().copied())
            .collect();
        match self.parties.zones_of(&concerned).await {
            Ok(zones) => {
                for zone in &zones {
                    self.notify_later(zone, HubNotice::Party(news.clone()));
                }
            }
            Err(e) => warn!(
                party = news.party.id,
                "nobody was told of a party's change: {e}"
            ),
        }
    }

    /// Tell the zones a character is in (or between) something.
    async fn tell_character(&self, character: CharacterId, notice: HubNotice) {
        if let Ok(zones) = self.parties.zones_of(&[character]).await {
            for zone in &zones {
                self.notify_later(zone, notice.clone());
            }
        }
    }

    /// Send a notice to a zone, if it is connected.
    async fn notify(&self, zone: &ZoneId, notice: HubNotice) {
        let conn = self
            .state
            .lock()
            .unwrap()
            .zones
            .get(zone)
            .map(|z| z.conn.clone());
        if let Some(conn) = conn
            && let Ok(mut uni) = conn.open_uni().await
        {
            let _ = control::send_any(&mut uni, &notice).await;
            let _ = uni.finish();
        }
    }
}

fn normalize_email(email: &str) -> Result<String, HubError> {
    let e = email.trim().to_lowercase();
    if e.len() < 3
        || e.len() > 254
        || !e.contains('@')
        || e.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(HubError::Invalid("email".into()));
    }
    Ok(e)
}

async fn handle(
    hub: &Arc<Hub>,
    auth: &ConnAuth,
    conn: &Link,
    remote: SocketAddr,
    req: HubRequest,
) -> Result<HubResponse, HubError> {
    match req {
        HubRequest::Register { email, password } => {
            if !hub.auth_allowed(remote.ip()) {
                return Err(HubError::Busy);
            }
            let email = normalize_email(&email)?;
            if password.chars().count() < 8 || password.len() > 256 {
                return Err(HubError::Invalid(
                    "a password is 8 characters or more, and 256 bytes at most".into(),
                ));
            }
            let _permit = hub.hashing.try_acquire().map_err(|_| HubError::Busy)?;
            let hash = tokio::task::spawn_blocking(move || {
                let mut salt = [0u8; 16];
                rand::fill(&mut salt);
                Argon2::default()
                    .hash_password_with_salt(password.as_bytes(), &salt)
                    .map(|h: PasswordHash| h.to_string())
            })
            .await
            .map_err(|_| HubError::Internal)?
            .map_err(|_| HubError::Internal)?;
            let account = hub.db.create_account(&email, &hash).await?;
            let session = hub.new_session(account);
            info!(%email, account, "registered");
            Ok(HubResponse::Session { session, account })
        }
        HubRequest::Login { email, password } => {
            if !hub.auth_allowed(remote.ip()) {
                return Err(HubError::Busy);
            }
            let email = normalize_email(&email)?;
            // An email nobody registered costs what a wrong password costs: the answer's
            // time does not say which accounts exist.
            let found = hub.db.account_by_email(&email).await?;
            let (account, hash) = match found {
                Some((account, hash)) => (Some(account), hash),
                None => (None, hub.decoy_hash.clone()),
            };
            let _permit = hub.hashing.try_acquire().map_err(|_| HubError::Busy)?;
            let ok = tokio::task::spawn_blocking(move || {
                PasswordHash::new(&hash)
                    .map(|parsed| {
                        Argon2::default()
                            .verify_password(password.as_bytes(), &parsed)
                            .is_ok()
                    })
                    .unwrap_or(false)
            })
            .await
            .map_err(|_| HubError::Internal)?;
            let Some(account) = account.filter(|_| ok) else {
                return Err(HubError::Credentials);
            };
            // A banned account is told why and until when (ANTICHEAT.md 6), after the
            // password: the reason is the account's own business.
            hub.conduct.check(account).await?;
            let session = hub.new_session(account);
            // A ban that landed between the check and the session ends the session.
            if let Err(e) = hub.conduct.check(account).await {
                hub.state
                    .lock()
                    .unwrap()
                    .sessions
                    .retain(|_, s| s.account != account);
                return Err(e);
            }
            Ok(HubResponse::Session { session, account })
        }
        HubRequest::Characters { session } => {
            let account = hub.session_account(session)?;
            let rows = hub.db.characters_of(account).await?;
            Ok(HubResponse::Characters(
                rows.iter().map(|r| r.summary()).collect(),
            ))
        }
        HubRequest::CreateCharacter {
            session,
            name,
            build,
        } => {
            let account = hub.session_account(session)?;
            // What every client can draw, and no name another could be taken for.
            let name = gm_hub_proto::names::character_name(&name)
                .map_err(|why| HubError::Invalid(why.into()))?;
            let key = gm_hub_proto::names::skeleton(&name);
            let build = hub.resolve_build(build)?;
            let row = hub
                .db
                .create_character(account, &name, &key, &build)
                .await?;
            Ok(HubResponse::Character(row.summary()))
        }
        HubRequest::SetBuild {
            session,
            character,
            build,
        } => {
            let account = hub.session_account(session)?;
            let build = hub.resolve_build(build)?;
            hub.db.set_build(account, character, &build).await?;
            Ok(HubResponse::Ok)
        }
        HubRequest::Content { session } => {
            hub.session_account(session)?;
            // A blurb for every preset, whatever the configuration gave.
            let mut blurbs = hub.cfg.blurbs.clone();
            blurbs.resize(hub.cfg.content.builds.len(), String::new());
            Ok(HubResponse::Content {
                pack: hub.cfg.content.clone(),
                blurbs,
            })
        }
        HubRequest::ListZones { session } => {
            hub.session_account(session)?;
            let st = hub.state.lock().unwrap();
            let mut zones: Vec<ZoneSummary> = st
                .zones
                .iter()
                .filter(|(_, z)| z.last_heartbeat.elapsed() < ZONE_STALE)
                .map(|(id, z)| ZoneSummary {
                    id: id.clone(),
                    map: z.map.clone(),
                    players: z.players,
                    max_players: z.max_players,
                    web: z.web.is_some(),
                    min_trust: z.min_trust,
                    requires: z.requires.clone(),
                    addr: z.addr,
                    cert_hash: fnv1a64(&z.cert_der),
                    up_secs: z.since.elapsed().as_secs(),
                })
                .collect();
            zones.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(HubResponse::Zones(zones))
        }
        HubRequest::Enter {
            session,
            character,
            zone,
        } => {
            let account = hub.session_account(session)?;
            let row = hub
                .db
                .character(character)
                .await?
                .filter(|r| r.account_id == account)
                .ok_or(HubError::NotFound)?;
            // A build the content no longer takes is repaired here (MATRIX.md 9.1) and
            // stored, so that a character outlives a change of the rules; one nothing can
            // be made of is refused in words, and not at a zone's door after the claim.
            let row = match row.build.repaired(&hub.cfg.content) {
                None => row,
                Some(build) => {
                    info!(character = row.id, "build repaired to the current rules");
                    hub.db.set_build(account, row.id, &build).await?;
                    crate::db::CharacterRow { build, ..row }
                }
            };
            row.build.validate(&hub.cfg.content).map_err(|e| {
                HubError::Invalid(format!("this character's build is no longer valid: {e}"))
            })?;
            let web = conn.is_web();
            let zone = if zone.is_empty() {
                // Where it was, else the start zone, else whatever will have it: the
                // first of them whose gate the character passes. With none, the refusal
                // of the first is the one to tell.
                let mut refusal = HubError::NotFound;
                let mut found = None;
                for (i, candidate) in hub
                    .default_zones(row.pos_zone.as_deref(), web)
                    .into_iter()
                    .enumerate()
                {
                    let open = match hub.zone_open(&candidate, web, character).await {
                        Ok(()) => hub.gate(&candidate, character).await,
                        Err(e) => Err(e),
                    };
                    match open {
                        Ok(()) => {
                            found = Some(candidate);
                            break;
                        }
                        Err(e) if i == 0 => refusal = e,
                        Err(_) => {}
                    }
                }
                found.ok_or(refusal)?
            } else {
                hub.zone_open(&zone, web, character).await?;
                hub.gate(&zone, character).await?;
                zone
            };
            hub.db.begin_enter(account, character, &zone).await?;
            Ok(HubResponse::Ticket(
                hub.ticket_for(&zone, account, character)?,
            ))
        }
        HubRequest::Logout { session } => {
            let account = hub.session_account(session)?;
            let others = {
                let mut st = hub.state.lock().unwrap();
                st.sessions.remove(&session);
                st.sessions.values().any(|s| s.account == account)
            };
            // The account is logged in somewhere else too: that client's play goes on.
            if others {
                return Ok(HubResponse::Ok);
            }
            // Characters still in a zone are kicked there and go offline here.
            let rows = hub.db.characters_of(account).await?;
            let in_zone: Vec<(CharacterId, ZoneId)> = rows
                .iter()
                .filter(|r| r.location_kind == "zone")
                .filter_map(|r| r.location_zone.clone().map(|z| (r.id, z)))
                .collect();
            // Characters in a live zone are saved and taken offline by that zone when it
            // handles the kick; everything else (transits, orphans) goes offline here.
            hub.db.offline_not_in_zone(account).await?;
            for (character, zone) in in_zone {
                hub.notify(
                    &zone,
                    HubNotice::Kick {
                        character,
                        reason: "logged out".into(),
                    },
                )
                .await;
            }
            Ok(HubResponse::Ok)
        }
        HubRequest::Trials { session, character } => {
            let account = hub.session_account(session)?;
            hub.db
                .character(character)
                .await?
                .filter(|r| r.account_id == account)
                .ok_or(HubError::NotFound)?;
            Ok(HubResponse::Trials(hub.db.trials_of(character).await?))
        }
        HubRequest::Trial {
            character,
            trial,
            secs,
        } => {
            // A zone speaks only for characters playing in it, and only about the trials
            // of its own map.
            let zone = hub.zone_of_conn(auth)?;
            if hub.db.zone_of(character).await?.as_ref() != Some(&zone) {
                return Err(HubError::Unauthorized);
            }
            let map = hub
                .state
                .lock()
                .unwrap()
                .zones
                .get(&zone)
                .map(|z| z.map.clone())
                .ok_or(HubError::Unauthorized)?;
            if !hub
                .cfg
                .content
                .trials
                .iter()
                .any(|t| t.key == trial && t.map == map)
            {
                return Err(HubError::Invalid(format!(
                    "{trial:?} is not a trial of the map {map:?}"
                )));
            }
            hub.db.trial_pass(character, &trial, &zone, secs).await?;
            hub.db
                .log(&zone, "trial", &format!("{character} {trial} {secs}s"))
                .await;
            Ok(HubResponse::Ok)
        }
        HubRequest::ZoneHello {
            secret,
            zone,
            map,
            map_hash,
            addr,
            cert_der,
            web,
            min_trust,
            requires,
            max_players,
        } => {
            if secret != hub.cfg.zone_secret || hub.cfg.zone_secret.is_empty() {
                warn!(%remote, %zone, "zone hello with a wrong secret");
                return Err(HubError::Unauthorized);
            }
            if valid_name(&zone).is_none() {
                return Err(HubError::Invalid("zone id".into()));
            }
            // (A zone that takes nobody would be full for good.)
            if max_players == 0 {
                return Err(HubError::Invalid("a zone takes one client at least".into()));
            }
            if requires.len() > MAX_ZONE_REQUIRES
                || requires
                    .iter()
                    .any(|r| !hub.cfg.content.trials.iter().any(|t| &t.key == r))
            {
                return Err(HubError::Invalid(
                    "the zone requires an unknown trial".into(),
                ));
            }
            // A restarted zone has lost its players: they go offline and re-enter.
            let orphaned = hub.db.offline_zone(&zone).await?;
            let previous = {
                let mut st = hub.state.lock().unwrap();
                let previous = st.zones.remove(&zone).map(|z| z.conn);
                st.zones.insert(
                    zone.clone(),
                    ZoneEntry {
                        addr,
                        cert_der,
                        web,
                        min_trust,
                        max_players,
                        map: map.clone(),
                        map_hash,
                        players: 0,
                        tick_mean_us: 0.0,
                        since: Instant::now(),
                        last_heartbeat: Instant::now(),
                        conn: conn.clone(),
                        requires,
                    },
                );
                previous
            };
            if let Some(old) = previous
                && old.stable_id() != conn.stable_id()
            {
                old.close(1, b"replaced by a new zone process");
            }
            hub.verifier.lock().unwrap().insert(
                zone.clone(),
                TokenVerifier::new(hub.cfg.key.public_key(), &zone)
                    .map_err(|_| HubError::Internal)?,
            );
            *auth.zone.lock().unwrap() = Some(zone.clone());
            hub.db
                .log(
                    &zone,
                    "hello",
                    &format!("{map} {map_hash:016x} {addr} orphaned={orphaned}"),
                )
                .await;
            info!(%zone, %map, %addr, orphaned, "zone registered");
            Ok(HubResponse::Registered {
                public_key: hub.cfg.key.public_key(),
            })
        }
        HubRequest::Heartbeat {
            players,
            tick_mean_us,
        } => {
            let zone = hub.zone_of_conn(auth)?;
            let mut st = hub.state.lock().unwrap();
            if let Some(z) = st.zones.get_mut(&zone) {
                z.players = players;
                z.tick_mean_us = tick_mean_us;
                z.last_heartbeat = Instant::now();
            }
            Ok(HubResponse::Ok)
        }
        HubRequest::Claim { token } => {
            let zone = hub.zone_of_conn(auth)?;
            let payload = {
                let mut v = hub.verifier.lock().unwrap();
                let verifier = v.get_mut(&zone).ok_or(HubError::Unauthorized)?;
                verifier
                    .accept(&token, now_secs())
                    .map_err(|e| HubError::Invalid(e.to_string()))?
            };
            // A ticket issued before a ban does not outlive it.
            hub.conduct.check(payload.account).await?;
            let row = hub.db.claim(payload.character, &zone).await?;
            // A handoff: the origin zone drops its ghost.
            if let Some(from) = &row.location_zone
                && from != &zone
            {
                hub.notify(from, HubNotice::Claimed { character: row.id })
                    .await;
            }
            let mut state = row.state();
            if state.zone.as_deref() != Some(zone.as_str()) {
                // The saved position is on another zone's map, or there is none: spawn here.
                state.zone = None;
            }
            // What follows can fail after the character has been moved here; the zone would
            // hear "claim failed" and have nothing to give back, so the hub does.
            let rest = async {
                // It is somewhere else now: whatever trade it had open is called off
                // (PARTY.md 6): a trade is between two that stand together.
                hub.econ.trades_end_of(row.id).await.map_err(econ_err)?;
                // The owner is playing this character now: every hire of it as an avatar
                // ends (ECONOMY.md 11), and the zones its hirers play in are told.
                for (hire, hirer) in hub.econ.end_hires_of(row.id).await.map_err(econ_err)? {
                    if let Some(z) = hub.db.zone_of(hirer).await? {
                        hub.notify(&z, HubNotice::HireEnded { hirer, hire }).await;
                    }
                }
                let squad = hub
                    .hired(row.id, row.build.squad_capacity(&hub.cfg.content))
                    .await?;
                // Read after the character became this zone's (ITEMS.md 3.3): whatever it
                // put on through the zone it came from is in it.
                let gear = hub
                    .econ
                    .gear(row.id, &hub.cfg.items)
                    .await
                    .map(reading)
                    .map_err(econ_err)?;
                Ok::<_, HubError>((
                    squad,
                    hub.models.worn(row.id).await?,
                    gear,
                    // As gear: read after the character became this zone's. A change
                    // that comes later is told to this zone, with a larger number.
                    hub.parties.reading(row.id).await?,
                    // A moderator's character is a game master there (GM.md 1).
                    hub.models.is_moderator(row.account_id).await?,
                ))
            }
            .await;
            match rest {
                Ok((squad, model, gear, party, gm)) => Ok(HubResponse::Claimed {
                    character: row.id,
                    name: row.name.clone(),
                    state,
                    team: 0,
                    model,
                    squad,
                    gear,
                    party,
                    gm,
                }),
                Err(e) => {
                    let _ = hub.db.release(row.id, &zone).await;
                    Err(e)
                }
            }
        }
        HubRequest::Release { character } => {
            let zone = hub.zone_of_conn(auth)?;
            if hub.db.release(character, &zone).await? {
                info!(%zone, character, "a character the zone had no body for is offline again");
            }
            Ok(HubResponse::Ok)
        }
        HubRequest::Save {
            character,
            state,
            leaving,
        } => {
            let zone = hub.zone_of_conn(auth)?;
            state
                .build
                .validate(&hub.cfg.content)
                .map_err(|e| HubError::Invalid(e.to_string()))?;
            hub.db.save(character, &zone, &state, leaving).await?;
            // Play time is what makes an account established (ANTICHEAT.md 6): looked at
            // when a character leaves, whether or not it ever shot at anybody.
            if leaving {
                if let Some(row) = hub.db.character(character).await? {
                    hub.conduct.promote(row.account_id).await?;
                }
                return Ok(HubResponse::Ok);
            }
            // The number of what the hub holds of its party: a zone that missed a notice
            // sees that it is behind, and asks (PARTY.md 3.2).
            Ok(HubResponse::Saved {
                party: hub.parties.seq_of(character).await?,
            })
        }
        HubRequest::Handoff {
            character,
            state,
            to_zone,
            web,
        } => {
            let zone = hub.zone_of_conn(auth)?;
            if to_zone == zone {
                return Err(HubError::Invalid("already there".into()));
            }
            // (The zone says whether its traveller is a browser.)
            hub.zone_open(&to_zone, web, character).await?;
            state
                .build
                .validate(&hub.cfg.content)
                .map_err(|e| HubError::Invalid(e.to_string()))?;
            hub.gate(&to_zone, character).await?;
            hub.db
                .begin_handoff(character, &zone, &to_zone, &state)
                .await?;
            let row = hub
                .db
                .character(character)
                .await?
                .ok_or(HubError::NotFound)?;
            Ok(HubResponse::Ticket(hub.ticket_for(
                &to_zone,
                row.account_id,
                character,
            )?))
        }
        HubRequest::Econ {
            session,
            character,
            op,
        } => {
            let account = hub.session_account(session)?;
            if !hub.econ_allowed(account) {
                return Err(HubError::Busy);
            }
            let row = hub
                .db
                .character(character)
                .await?
                .filter(|r| r.account_id == account)
                .ok_or(HubError::NotFound)?;
            let zone = match row.summary().location {
                LocationSummary::Zone(z) => Some(z),
                _ => None,
            };
            // What moves items without the zone's hand (MODES.md 11.2) is told to the
            // zone after: the storage, a stall of one's own, a craft, a trade (both sides).
            let moves_items = matches!(
                op,
                EconOp::StorageDeposit { .. }
                    | EconOp::StorageWithdraw { .. }
                    | EconOp::StallList { .. }
                    | EconOp::StallUnlist { .. }
                    | EconOp::StallClose
                    | EconOp::Craft { .. }
                    | EconOp::Decompose { .. }
                    | EconOp::TradeAccept { .. }
                    | EconOp::SetBar { .. }
            );
            let trade = match &op {
                EconOp::TradeAccept { trade, .. } => Some(*trade),
                _ => None,
            };
            let reply = econ_op(hub, character, zone, op).await?;
            if moves_items {
                if let (Some(trade), EconReply::Trade { committed: true }) = (trade, &reply)
                    && let Ok((a, b)) = hub.econ.trade_characters(trade).await
                {
                    tell_zone_of_items(hub, a).await;
                    tell_zone_of_items(hub, b).await;
                } else if trade.is_none() {
                    tell_zone_of_items(hub, character).await;
                }
            }
            Ok(HubResponse::Econ(reply))
        }
        HubRequest::ZoneEcon(op) => {
            let zone = hub.zone_of_conn(auth)?;
            Ok(HubResponse::Econ(zone_econ_op(hub, &zone, op).await?))
        }
        HubRequest::ZoneParty(op) => {
            let zone = hub.zone_of_conn(auth)?;
            Ok(HubResponse::Party(zone_party_op(hub, &zone, op).await?))
        }
        // Answered on the stream itself (`handle_stream`): they carry or return raw bytes.
        HubRequest::ModelUpload { .. } | HubRequest::ModelGet { .. } => Err(HubError::Internal),
        HubRequest::ModelList { session } => {
            let account = hub.session_account(session)?;
            Ok(HubResponse::Models(hub.models.list(account).await?))
        }
        HubRequest::ModelDrop { session, model } => {
            let account = hub.session_account(session)?;
            hub.models.drop_model(account, &model).await?;
            Ok(HubResponse::Ok)
        }
        HubRequest::SetModel {
            session,
            character,
            model,
        } => {
            let account = hub.session_account(session)?;
            let row = hub
                .db
                .character(character)
                .await?
                .filter(|r| r.account_id == account)
                .ok_or(HubError::NotFound)?;
            let frame = gm_model::rig::frame_index(row.build.frame);
            hub.models
                .set_model(account, character, frame, model.as_ref())
                .await?;
            Ok(HubResponse::Ok)
        }
        HubRequest::Mod { session, op } => {
            let moderator = hub.moderator(session).await?;
            match op {
                ModOp::Queue { limit } => Ok(HubResponse::ModQueue(hub.models.queue(limit).await?)),
                ModOp::Preview { .. } => Err(HubError::Internal),
                ModOp::Decide {
                    model,
                    approve,
                    code,
                    reason,
                } => {
                    hub.models
                        .decide(moderator, &model, approve, code, &reason)
                        .await?;
                    Ok(HubResponse::Ok)
                }
                ModOp::Takedown {
                    model,
                    code,
                    reason,
                    reference,
                } => {
                    let revoked = hub
                        .models
                        .takedown(moderator, &model, code, &reason, &reference)
                        .await?;
                    // The database already says so; now the zones and their clients.
                    for model in revoked {
                        hub.notify_all(HubNotice::ModelRevoked { model }).await;
                    }
                    Ok(HubResponse::Ok)
                }
                ModOp::Reinstate { model, reason } => {
                    hub.models.reinstate(moderator, &model, &reason).await?;
                    Ok(HubResponse::Ok)
                }
                ModOp::SetUpload { email, allow } => {
                    hub.models.set_upload(moderator, &email, allow).await?;
                    Ok(HubResponse::Ok)
                }
                ModOp::SetTrust { email, tier } => {
                    hub.models.set_trust(moderator, &email, tier).await?;
                    Ok(HubResponse::Ok)
                }
                ModOp::ClearStrikes { email } => {
                    hub.models.clear_strikes(moderator, &email).await?;
                    Ok(HubResponse::Ok)
                }
                ModOp::AimReport { weeks, min_shots } => Ok(HubResponse::AimReport(
                    hub.conduct.aim_report(moderator, weeks, min_shots).await?,
                )),
                ModOp::Replays {
                    email,
                    reported,
                    flagged,
                    limit,
                } => Ok(HubResponse::Replays(
                    hub.conduct
                        .replays(moderator, email.as_deref(), reported, flagged, limit)
                        .await?,
                )),
                // Answered on the stream itself (`handle_stream`): a blob.
                ModOp::ReplayGet { .. } => Err(HubError::Internal),
                ModOp::Reports { open_only } => Ok(HubResponse::Reports(
                    hub.conduct.reports(moderator, open_only).await?,
                )),
                ModOp::ReportVerdict { id, verdict, note } => {
                    hub.conduct
                        .report_verdict(moderator, id, verdict, &note)
                        .await?;
                    Ok(HubResponse::Ok)
                }
                ModOp::Ban {
                    email,
                    days,
                    reason,
                    cheat,
                } => {
                    let (account, kicked) = hub
                        .conduct
                        .ban(moderator, &email, days, &reason, cheat)
                        .await?;
                    // The account's sessions end here, its characters leave their zones,
                    // and its stalls close (ANTICHEAT.md 6).
                    hub.state
                        .lock()
                        .unwrap()
                        .sessions
                        .retain(|_, s| s.account != account);
                    for k in kicked {
                        hub.notify(
                            &k.zone,
                            HubNotice::Kick {
                                character: k.character,
                                reason: format!("banned: {reason}"),
                            },
                        )
                        .await;
                    }
                    // The ban stands whatever happens to the tidying after it.
                    match hub.db.characters_of(account).await {
                        Ok(rows) => {
                            for row in rows {
                                if let Ok((stall, zone)) = hub.econ.stall_close(row.id).await {
                                    hub.notify(&zone, HubNotice::StallClosed { stall }).await;
                                }
                            }
                        }
                        Err(e) => warn!(account, "closing a banned account's stalls: {e}"),
                    }
                    info!(account, days, "account banned");
                    Ok(HubResponse::Ok)
                }
                ModOp::Unban { email, note } => {
                    hub.conduct.unban(moderator, &email, &note).await?;
                    Ok(HubResponse::Ok)
                }
                ModOp::Reputation { email } => Ok(HubResponse::Standing(
                    hub.conduct.standing(moderator, &email).await?,
                )),
                ModOp::Adjust { email, delta, note } => {
                    hub.conduct.adjust(moderator, &email, delta, &note).await?;
                    Ok(HubResponse::Ok)
                }
            }
        }
        // Zones: conduct (ANTICHEAT.md 8).
        HubRequest::ZoneAim {
            nonce,
            character,
            stats,
        } => {
            let zone = hub.zone_of_conn(auth)?;
            // A zone speaks for the characters that play or played in it (a leaver's
            // numbers arrive after it went): the character must exist, no more.
            if hub.db.character(character).await?.is_none() {
                return Err(HubError::NotFound);
            }
            hub.conduct.aim(&zone, nonce, character, &stats).await?;
            Ok(HubResponse::Ok)
        }
        HubRequest::ZoneReport {
            reporter,
            target,
            reason,
        } => {
            let zone = hub.zone_of_conn(auth)?;
            if hub.db.zone_of(reporter).await?.as_deref() != Some(zone.as_str()) {
                return Err(HubError::Unauthorized);
            }
            let id = hub
                .conduct
                .report_open(&zone, reporter, target, reason)
                .await?;
            Ok(HubResponse::ReportOpened { id })
        }
        // Answered on the stream itself (`handle_stream`): it carries raw bytes.
        HubRequest::ZoneReplay { .. } => Err(HubError::Internal),
    }
}

fn econ_err(e: EconError) -> HubError {
    match e {
        EconError::NotFound => HubError::NotFound,
        EconError::Forbidden => HubError::Unauthorized,
        EconError::Insufficient => HubError::Insufficient,
        EconError::Full => HubError::Full,
        EconError::Cooldown => HubError::Cooldown,
        EconError::State(s) | EconError::Invalid(s) => HubError::Invalid(s),
        EconError::Busy => HubError::Busy,
        EconError::Internal => HubError::Internal,
    }
}

/// A build as a person is told it (PARTY.md 7): the content's name for it with its frame
/// and armour, and the role a mind plays it in (the same reading the zone's minds make).
/// The hub says both, so that no client works them out. Nothing for a build the content
/// no longer has (a sheet is made of it, and a sheet is made of what the content says).
fn build_words(content: &ContentPack, build: &Build) -> Option<(String, String)> {
    use gm_core::matrix::ArmourClass;
    use gm_core::vocab::ArchetypeFrame;
    build.validate(content).ok()?;
    let name = content
        .builds
        .iter()
        .find(|b| &b.build == build)
        .map_or("its own build", |b| b.name.as_str());
    let frame = match build.frame {
        ArchetypeFrame::Colossus => "colossus",
        ArchetypeFrame::Striker => "striker",
        ArchetypeFrame::Caster => "caster",
        ArchetypeFrame::Infiltrator => "infiltrator",
    };
    let armour = match build.armour {
        ArmourClass::Cloth => "cloth",
        ArmourClass::Leather => "leather",
        ArmourClass::Mail => "mail",
        ArmourClass::Plate => "plate",
    };
    let sheet = gm_core::build::Sheet::new(build.clone(), content, 1);
    let role = gm_ai::Role::of(&sheet).name();
    Some((format!("{name}: {frame} in {armour}"), role.to_string()))
}

/// What a build's hands hold (ITEMS.md 2): the props of its slotted abilities, in kit
/// order, each once. A weapon whose model is not among them is not worn by this build.
pub fn hands(cfg: &HubConfig, build: &Build) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (index, _) in build.slots() {
        let Some(def) = cfg.content.abilities.get(index as usize) else {
            continue;
        };
        let prop = cfg
            .looks
            .abilities
            .iter()
            .find(|a| a.key == def.key)
            .and_then(|a| a.prop.clone());
        if let Some(prop) = prop
            && !out.contains(&prop)
        {
            out.push(prop);
        }
    }
    out
}

/// The hands of a character's stored build, read now.
async fn hands_of(hub: &Hub, character: CharacterId) -> Result<Vec<String>, HubError> {
    let row = hub
        .db
        .character(character)
        .await?
        .ok_or(HubError::NotFound)?;
    Ok(hands(&hub.cfg, &row.build))
}

/// An item as a person is shown it: where it is worn, what it does there and the words
/// for both are the content's to say (ITEMS.md 3.2), so that no client works them out.
/// `hands` are the asking character's (ITEMS.md 2): a weapon they do not hold says so.
fn item_summary(
    content: &ItemContent,
    pack: &gm_core::build::ContentPack,
    hands: &[String],
    i: crate::economy::Item,
) -> ItemSummary {
    let mut view = content.view(
        &i.template,
        i.components
            .iter()
            .map(|c| (c.layer.as_str(), c.material.as_str())),
    );
    // A stack says how many it is (MODES.md 11.1), and rounds say which gun they load:
    // carried is loaded, there is nothing to equip (the director, 2026-10-07).
    let cap = match content.stack(&i.template) {
        Some((cap, heals)) => {
            view.what = content.stack_words(&i.template, i.quantity);
            if heals.is_none() {
                let guns: Vec<&str> = pack
                    .abilities
                    .iter()
                    .filter(|a| {
                        a.ability
                            .firearm
                            .as_ref()
                            .is_some_and(|f| f.ammo == i.template)
                    })
                    .map(|a| a.ability.name.as_str())
                    .collect();
                view.does = if guns.is_empty() {
                    vec!["rounds no gun of this world takes".to_string()]
                } else {
                    vec![format!(
                        "loads the {}: carried is loaded, R reloads",
                        guns.join(" and the ")
                    )]
                };
            }
            cap
        }
        None => 0,
    };
    let fits = crate::economy::fits(content, hands, &i.template, view.place).is_ok();
    if let Err(why) = crate::economy::fits(content, hands, &i.template, view.place) {
        view.does.push(why);
    }
    ItemSummary {
        quantity: i.quantity,
        cap,
        fits,
        id: i.id,
        template: i.template,
        place: match view.place {
            Some(Place::Weapon) => PLACE_WEAPON,
            Some(Place::Armour) => PLACE_ARMOUR,
            None => PLACE_NONE,
        },
        edge: view.edge,
        worn: i.worn,
        what: view.what,
        does: view.does,
        components: i
            .components
            .into_iter()
            .map(|c| (c.layer, c.material))
            .collect(),
    }
}

fn trade_offer(
    content: &ItemContent,
    pack: &gm_core::build::ContentPack,
    hands: &[String],
    (coin, accepted, items): (i64, bool, Vec<crate::economy::Item>),
) -> TradeOffer {
    TradeOffer {
        coin,
        accepted,
        items: items
            .into_iter()
            .map(|i| item_summary(content, pack, hands, i))
            .collect(),
    }
}

/// The hub's reading of a character's gear and stacks (ITEMS.md 3.3, MODES.md 11.2), as
/// a zone is told it.
fn reading((seq, gear, templates, stacks, bar): crate::economy::Reading) -> GearReading {
    GearReading {
        seq,
        gear,
        templates,
        bar,
        stacks: stacks
            .into_iter()
            .map(|s| StackReading {
                item: s.item,
                template: s.template,
                quantity: s.quantity,
                heals: s.heals,
            })
            .collect(),
    }
}

/// After a session's request moved items of `character` without its zone knowing (the
/// storage, a stall of its own, a trade, a craft): the zone it plays in, if any, is told
/// what it wears and carries now (MODES.md 11.2), so that a reserve is never stale.
async fn tell_zone_of_items(hub: &Hub, character: CharacterId) {
    let Ok(Some(zone)) = hub.db.zone_of(character).await else {
        return;
    };
    if let Ok(r) = hub.econ.gear(character, &hub.cfg.items).await {
        hub.notify(
            &zone,
            HubNotice::Gear {
                character,
                reading: reading(r),
            },
        )
        .await;
    }
}

fn holder_reply(
    content: &ItemContent,
    pack: &gm_core::build::ContentPack,
    hands: &[String],
    (coin, items): (i64, Vec<crate::economy::Item>),
) -> EconReply {
    EconReply::Holder {
        coin,
        items: items
            .into_iter()
            .map(|i| item_summary(content, pack, hands, i))
            .collect(),
    }
}

/// One economy request of a character its session owns. `zone` is where the character is
/// playing; the ops that happen in the world (ground, stalls, trades) need one.
async fn econ_op(
    hub: &Hub,
    me: CharacterId,
    zone: Option<ZoneId>,
    op: EconOp,
) -> Result<EconReply, HubError> {
    let e = &hub.econ;
    let items = &hub.cfg.items;
    let here = || {
        zone.clone()
            .ok_or_else(|| HubError::Invalid("the character is not in a zone".into()))
    };
    let done = |r: Result<(), EconError>| r.map(|()| EconReply::Done).map_err(econ_err);
    let id = |r: Result<i64, EconError>| r.map(EconReply::Id).map_err(econ_err);
    // What this character's hands hold, for the answers that show items (ITEMS.md 2).
    let hands = match &op {
        EconOp::Inventory
        | EconOp::Storage
        | EconOp::TradeView { .. }
        | EconOp::StallView { .. } => hands_of(hub, me).await?,
        _ => Vec::new(),
    };
    match op {
        EconOp::Inventory => e
            .inventory(me)
            .await
            .map(|h| holder_reply(items, &hub.cfg.content, &hands, h))
            .map_err(econ_err),
        EconOp::Bar => e.bar(me).await.map(EconReply::Bar).map_err(econ_err),
        EconOp::SetBar { cells } => done(e.set_bar(me, &cells).await),
        EconOp::Storage => e
            .storage(me)
            .await
            .map(|h| holder_reply(items, &hub.cfg.content, &hands, h))
            .map_err(econ_err),
        EconOp::StorageDeposit { item } => done(e.storage_deposit(me, item).await),
        EconOp::StorageWithdraw { item } => done(e.storage_withdraw(me, item).await),
        EconOp::Craft {
            template,
            components,
        } => {
            // The content decides which templates exist and what each has room for; a hub
            // that was given no content (a test) takes any.
            let layers = items.layers(&template);
            if layers.is_none() && !items.templates.is_empty() {
                return Err(HubError::Invalid(format!("unknown template {template:?}")));
            }
            id(e.craft(me, &template, &components, layers).await)
        }
        EconOp::Decompose { item } => e
            .decompose(me, item)
            .await
            .map(EconReply::Ids)
            .map_err(econ_err),
        EconOp::TradeOfferItem { trade, item } => done(e.trade_offer_item(trade, me, item).await),
        EconOp::TradeRetractItem { trade, item } => {
            done(e.trade_retract_item(trade, me, item).await)
        }
        EconOp::TradeSetCoin { trade, coin } => done(e.trade_set_coin(trade, me, coin).await),
        EconOp::TradeAccept { trade, version } => e
            .trade_accept(trade, me, version)
            .await
            .map(|s| EconReply::Trade {
                committed: s == TradeStatus::Committed,
            })
            .map_err(econ_err),
        EconOp::TradeCancel { trade } => done(e.trade_cancel(trade, me).await),
        EconOp::TradeView { trade } => e
            .trade_view(trade, me)
            .await
            .map(|seen| EconReply::TradeView {
                state: match seen.state {
                    TradeState::Open => TRADE_OPEN,
                    TradeState::Committed => TRADE_COMMITTED,
                    TradeState::Cancelled => TRADE_CANCELLED,
                },
                version: seen.version,
                wait_ms: seen.wait_ms,
                with: seen.with,
                together: seen.together,
                mine: trade_offer(items, &hub.cfg.content, &hands, seen.mine),
                theirs: trade_offer(items, &hub.cfg.content, &hands, seen.theirs),
            })
            .map_err(econ_err),
        // A stall is kept where it stands: its keeper lists and unlists while playing in
        // the stall's zone, not from the other end of the world.
        EconOp::StallList { item, price } => id(e.stall_list(me, &here()?, item, price).await),
        EconOp::StallUnlist { listing } => done(e.stall_unlist(me, &here()?, listing).await),
        EconOp::StallView { stall } => e
            .stall_view(stall)
            .await
            .map(|(owner, name, listings)| EconReply::Listings {
                owner: name,
                mine: owner == me,
                listings: listings
                    .into_iter()
                    .map(|l| ListingSummary {
                        id: l.id,
                        item: item_summary(items, &hub.cfg.content, &hands, l.item),
                        price: l.price,
                    })
                    .collect(),
            })
            .map_err(econ_err),
        EconOp::StallClose => {
            // The owner may close from anywhere; the zone the stall stands in is told.
            let (stall, stall_zone) = e.stall_close(me).await.map_err(econ_err)?;
            hub.notify(&stall_zone, HubNotice::StallClosed { stall })
                .await;
            Ok(EconReply::Done)
        }
        EconOp::BuyOrderPost {
            material,
            price,
            quantity,
        } => id(e.buy_order_post(me, &material, price, quantity).await),
        EconOp::BuyOrderFill { order, item } => done(e.buy_order_fill(me, order, item).await),
        EconOp::BuyOrderCancel { order } => done(e.buy_order_cancel(me, order).await),
        EconOp::ContractPost {
            instance,
            price,
            collateral,
        } => id(e.contract_post(me, &instance, price, collateral).await),
        EconOp::ContractCancel { contract } => done(e.contract_cancel(me, contract).await),
        EconOp::ContractAccept { contract, sellers } => {
            done(e.contract_accept(me, contract, &sellers).await)
        }
        EconOp::ChestDeposit { chest, item } => done(e.chest_deposit(me, chest, item).await),
        EconOp::ChestWithdraw { chest, item } => done(e.chest_withdraw(me, chest, item).await),
        EconOp::HireList { price } => done(e.hire_list(me, price).await),
        EconOp::HireUnlist => done(e.hire_unlist(me).await),
        EconOp::HireListed => e
            .hire_listed(me)
            .await
            .map(|price| EconReply::Id(price.unwrap_or(0)))
            .map_err(econ_err),
        EconOp::Hire { avatar, price } => e
            .hire(me, avatar, capacity(hub, me).await?, Some(price), |build| {
                // What the hire buys must be playable: a listing whose build the content
                // no longer has sells nothing.
                serde_json::from_value::<Build>(build.clone())
                    .is_ok_and(|b| b.validate(&hub.cfg.content).is_ok())
            })
            .await
            .map(|(hire, _burned)| EconReply::Id(hire))
            .map_err(econ_err),
        EconOp::Tavern => Ok(EconReply::Tavern(
            e.tavern()
                .await
                .map_err(econ_err)?
                .into_iter()
                // A listing whose build the content no longer has is not shown: a hire of
                // it would buy nothing a zone can play, and the hub would not make it.
                .filter_map(|t| {
                    let build: Build = serde_json::from_value(t.build).ok()?;
                    let (what, role) = build_words(&hub.cfg.content, &build)?;
                    Some(TavernEntry {
                        character: t.character,
                        name: t.name,
                        price: t.price,
                        hires: t.hires,
                        what,
                        role,
                        build,
                    })
                })
                .collect(),
        )),
        EconOp::Squad => Ok(EconReply::Squad(
            hub.hired(me, capacity(hub, me).await?).await?,
        )),
        EconOp::Dismiss { hire } => {
            e.dismiss(me, hire).await.map_err(econ_err)?;
            if let Some(z) = &zone {
                hub.notify(z, HubNotice::HireEnded { hirer: me, hire })
                    .await;
            }
            Ok(EconReply::Done)
        }
    }
}

/// The squad capacity of a character's stored build (COMPANIONS.md 3.2).
async fn capacity(hub: &Hub, character: CharacterId) -> Result<usize, HubError> {
    Ok(hub
        .db
        .character(character)
        .await?
        .ok_or(HubError::NotFound)?
        .build
        .squad_capacity(&hub.cfg.content))
}

/// What a zone asks about the parties of its characters (PARTY.md 3.2). The character a
/// request names must play in the asking zone; every other zone the answer concerns is
/// told by notice.
async fn zone_party_op(hub: &Hub, zone: &ZoneId, op: ZonePartyOp) -> Result<PartyReply, HubError> {
    match op {
        ZonePartyOp::Invite { from, to } => {
            let invitation = hub.parties.invite(from, zone, &to).await?;
            let notice = HubNotice::Invited {
                to: invitation.to,
                from: invitation.from_name,
            };
            hub.tell_character(invitation.to, notice).await;
            Ok(PartyReply::Invited {
                name: invitation.to_name,
            })
        }
        ZonePartyOp::Answer {
            character,
            from,
            join,
        } => match hub.parties.answer(character, zone, &from, join).await? {
            Answered::Joined(news) => {
                hub.tell_party(&news).await;
                Ok(PartyReply::News(news))
            }
            Answered::Declined(inviter, by) => {
                let notice = HubNotice::Declined { to: inviter, by };
                hub.tell_character(inviter, notice).await;
                Ok(PartyReply::Declined)
            }
        },
        ZonePartyOp::Leave { character } => {
            let news = hub.parties.leave(character, zone).await?;
            hub.tell_party(&news).await;
            Ok(PartyReply::News(news))
        }
        ZonePartyOp::Remove { leader, name } => {
            let news = hub.parties.remove(leader, zone, &name).await?;
            hub.tell_party(&news).await;
            Ok(PartyReply::News(news))
        }
        ZonePartyOp::Read { character } => Ok(PartyReply::Reading(
            hub.parties.read(character, zone).await?,
        )),
        ZonePartyOp::Say { from, to, text } => {
            // The zone checked the line and the speaker's bucket (PARTY.md 5); that it is
            // a line at all is checked again, because it is cheap.
            let Some(text) = control::valid_chat(&text) else {
                return Err(HubError::Invalid("that is not a line".into()));
            };
            let line = hub.parties.line(from, zone, &to).await?;
            let channel = match to {
                SayTo::Party => CHANNEL_PARTY,
                SayTo::Whisper(_) => CHANNEL_WHISPER,
            };
            // One notice a zone, naming everybody there who hears it.
            let mut by_zone: std::collections::BTreeMap<&ZoneId, Vec<CharacterId>> =
                std::collections::BTreeMap::new();
            for hearer in &line.hearers {
                for z in &hearer.zones {
                    by_zone.entry(z).or_default().push(hearer.character);
                }
            }
            for (z, to) in by_zone {
                let notice = HubNotice::Heard {
                    to,
                    channel,
                    from: line.from_name.clone(),
                    text: text.clone(),
                };
                hub.notify_later(z, notice);
            }
            Ok(PartyReply::Said { to: line.to_name })
        }
    }
}

/// What a zone reports. A zone speaks only for itself: it grants to characters playing in
/// it and decides contracts whose instance it is.
async fn zone_econ_op(hub: &Hub, zone: &ZoneId, op: ZoneEconOp) -> Result<EconReply, HubError> {
    let e = &hub.econ;
    match op {
        ZoneEconOp::GrantComponents { grants, reference } => {
            if grants.len() > 256 {
                return Err(HubError::Invalid("too many grants".into()));
            }
            for (character, _) in &grants {
                if hub.db.zone_of(*character).await?.as_ref() != Some(zone) {
                    return Err(HubError::Unauthorized);
                }
            }
            e.grant_components(zone, &grants, reference)
                .await
                .map(EconReply::Ids)
                .map_err(econ_err)
        }
        ZoneEconOp::GrantCoin {
            character,
            amount,
            reference,
        } => {
            if hub.db.zone_of(character).await?.as_ref() != Some(zone) {
                return Err(HubError::Unauthorized);
            }
            if amount > hub.cfg.max_coin_grant {
                return Err(HubError::Invalid("coin drops are tiny".into()));
            }
            e.grant_coin(character, amount, reference)
                .await
                .map(|()| EconReply::Done)
                .map_err(econ_err)
        }
        ZoneEconOp::GrantKill {
            reference,
            components,
            coin,
        } => {
            if components.len() > 256 || coin.len() > 64 {
                return Err(HubError::Invalid("too many grants".into()));
            }
            if coin
                .iter()
                .any(|(_, amount)| *amount > hub.cfg.max_coin_grant)
            {
                return Err(HubError::Invalid("coin drops are tiny".into()));
            }
            // A zone speaks only for characters playing in it. One who left between the
            // kill and this report forfeits: its coin is not made, its components lie on
            // the ground where the boss died.
            let mut here: Vec<(CharacterId, bool)> = Vec::new();
            for character in components
                .iter()
                .map(|(c, _)| *c)
                .chain(coin.iter().map(|(c, _)| *c))
            {
                if !here.iter().any(|(c, _)| *c == character) {
                    let playing = hub.db.zone_of(character).await?.as_ref() == Some(zone);
                    here.push((character, playing));
                }
            }
            let is_here = |c: CharacterId| here.iter().any(|(h, playing)| *h == c && *playing);
            let components: Vec<(Option<i64>, String)> = components
                .into_iter()
                .map(|(c, material)| (is_here(c).then_some(c), material))
                .collect();
            let coin: Vec<(i64, i64)> = coin.into_iter().filter(|(c, _)| is_here(*c)).collect();
            match e
                .grant_kill(zone, reference, &components, &coin)
                .await
                .map_err(econ_err)?
            {
                Some(ids) => Ok(EconReply::Ids(ids)),
                None => Ok(EconReply::Done),
            }
        }
        ZoneEconOp::StallOpen {
            character,
            tile_x,
            tile_y,
        } => {
            if hub.db.zone_of(character).await?.as_ref() != Some(zone) {
                return Err(HubError::Unauthorized);
            }
            let id = match e.stall_open(character, zone, tile_x, tile_y).await {
                Ok(id) => id,
                // The unique constraints: the tile is taken, or the character has a stall.
                Err(EconError::State(_)) => return Err(HubError::Taken),
                Err(other) => return Err(econ_err(other)),
            };
            let mut stalls = stall_summaries(hub, zone, Some(id)).await?;
            stalls.pop().map(EconReply::Stall).ok_or(HubError::Internal)
        }
        ZoneEconOp::StallClose { character } => {
            if hub.db.zone_of(character).await?.as_ref() != Some(zone) {
                return Err(HubError::Unauthorized);
            }
            let (stall, stall_zone) = e.stall_close(character).await.map_err(econ_err)?;
            hub.notify(&stall_zone, HubNotice::StallClosed { stall })
                .await;
            Ok(EconReply::Done)
        }
        ZoneEconOp::Stalls => Ok(EconReply::Stalls(stall_summaries(hub, zone, None).await?)),
        ZoneEconOp::StallBuy {
            character,
            stall,
            listing,
            price,
        } => {
            if hub.db.zone_of(character).await?.as_ref() != Some(zone) {
                return Err(HubError::Unauthorized);
            }
            e.stall_buy(character, zone, stall, listing, price)
                .await
                .map_err(econ_err)?;
            // What it carries now (MODES.md 11.2): a stack bought is a reserve filled.
            e.gear(character, &hub.cfg.items)
                .await
                .map(|r| EconReply::Gear(reading(r)))
                .map_err(econ_err)
        }
        // The zone spent them already (MODES.md 11.2); the hub's books follow, and the
        // zone takes the reading it is answered, whatever the hub found.
        ZoneEconOp::Consume {
            character,
            item,
            quantity,
        } => {
            if hub.db.zone_of(character).await?.as_ref() != Some(zone) {
                return Err(HubError::Unauthorized);
            }
            if let Err(why) = e.consume(character, item, quantity).await {
                tracing::info!(character, item, quantity, %why, "consume refused");
            }
            e.gear(character, &hub.cfg.items)
                .await
                .map(|r| EconReply::Gear(reading(r)))
                .map_err(econ_err)
        }
        // That the character plays in this zone is checked inside the transaction, with
        // its row held: a claim elsewhere sees the change or comes first (ITEMS.md 3.3).
        ZoneEconOp::Wear { character, item } => {
            let hands = hands_of(hub, character).await?;
            e.wear(character, zone, item, &hub.cfg.items, &hands)
                .await
                .map(|r| EconReply::Gear(reading(r)))
                .map_err(econ_err)
        }
        ZoneEconOp::TakeOff { character, item } => e
            .take_off(character, zone, item, &hub.cfg.items)
            .await
            .map(|r| EconReply::Gear(reading(r)))
            .map_err(econ_err),
        // The zone saw the two stand together and both ask (PARTY.md 6); that both play in
        // it is checked again here, under their rows.
        ZoneEconOp::TradeOpen { a, b } => e
            .trade_open_in(a, b, zone)
            .await
            .map(EconReply::Id)
            .map_err(econ_err),
        ZoneEconOp::Drop { character, item } | ZoneEconOp::Pickup { character, item }
            if hub.db.zone_of(character).await?.as_ref() != Some(zone) =>
        {
            let _ = item;
            Err(HubError::Unauthorized)
        }
        ZoneEconOp::Drop { character, item } => e
            .drop_item(character, item, zone)
            .await
            .map(|()| EconReply::Done)
            .map_err(econ_err),
        ZoneEconOp::Pickup { character, item } => e
            .pickup(character, item, zone)
            .await
            .map(|()| EconReply::Done)
            .map_err(econ_err),
        ZoneEconOp::ContractReport { contract, outcome } => {
            if e.contract_instance(contract).await.map_err(econ_err)? != *zone {
                return Err(HubError::Unauthorized);
            }
            let outcome = match outcome {
                ContractOutcome::Completed => Outcome::Completed,
                ContractOutcome::Wipe => Outcome::Wipe,
                ContractOutcome::Abandon => Outcome::Abandon,
            };
            let decided = e
                .contract_report(contract, outcome)
                .await
                .map_err(econ_err)?;
            // A contract that ended is reputation for its sellers (ANTICHEAT.md 6): paid, or
            // abandoned. A wipe is nobody's fault.
            if decided && outcome != Outcome::Wipe {
                let paid = outcome == Outcome::Completed;
                if let Err(e) = hub.conduct.contract_decided(contract, paid).await {
                    warn!(contract, "contract reputation: {e}");
                }
            }
            Ok(EconReply::Decided(decided))
        }
    }
}

/// The stalls of a zone as its zone shows them.
async fn stall_summaries(
    hub: &Hub,
    zone: &ZoneId,
    only: Option<i64>,
) -> Result<Vec<StallSummary>, HubError> {
    let rows = hub.econ.stalls_in(zone, only).await.map_err(econ_err)?;
    rows.into_iter()
        .map(|(id, tile_x, tile_y, owner, owner_name, build, model)| {
            let build: Build = serde_json::from_value(build).map_err(|_| HubError::Internal)?;
            Ok(StallSummary {
                id,
                tile_x,
                tile_y,
                owner,
                owner_name,
                frame: gm_model::rig::frame_index(build.frame),
                armour: build.armour as u8,
                model: model.and_then(|(hash, frame)| {
                    Some(ModelRef {
                        id: hash.try_into().ok()?,
                        frame: frame as u8,
                    })
                }),
            })
        })
        .collect()
}

/// The tick rate the hub validates content against (builds do not depend on it, but the pack
/// does).
pub fn content_rate() -> TickRate {
    TickRate::COMBAT
}
