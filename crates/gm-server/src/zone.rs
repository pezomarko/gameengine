//! The zone process: tick loop over `gm_core::sim::Zone`, sessions, snapshots and reports.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use glam::Vec3;
use gm_ai::director::{CompanionSpec, CreatureSpawn, Director, DirectorEvent, EncounterState};
use gm_core::build::{Build, ContentPack};
use gm_core::sim::{HitKind, MAX_CLAIMED_VIEW_LAG, Zone, ZoneEvent};
use gm_core::tick::TickRate;
use gm_core::trace::{CollisionWorld, Contents, Hull};
use gm_core::tuning::Tuning;
use gm_core::vocab::EntityId;
use gm_net::control::{
    self, BodyKind, BuildChoice, FromZone, GmNews, GmOp, Look, PlayerEntry, SquadEntry, StallEntry,
    stall_in_reach, trade_in_reach,
};
use rayon::prelude::*;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::hub_link::HubLink;
use crate::net::{ClientEvent, EVENT_CHANNEL, JoinInfo, NetConfig, PartyAsk, accept_loop};
use crate::party::ZoneParties;
use crate::recorder::{Recorder, RecorderConfig, Written};
use crate::session::{PvsCache, Session, TickTable};
use gm_hub_proto::protocol::{
    CharacterId, CharacterState, GearReading, ModelId, ModelRef, PartyNews, PartyReply, SayTo,
    StackReading, StallSummary, ZonePartyOp, now_secs,
};
use gm_replay::RosterEntry;

/// Zones save every character this often (HUB.md 3.2).
pub const SAVE_EVERY: Duration = Duration::from_secs(30);
/// A ghost waits this long for the other zone's claim (HUB.md 3.3).
pub const GHOST_TIMEOUT: Duration = Duration::from_secs(10);
/// A revoked model is remembered this long, so a claim that raced the takedown cannot bring
/// it back (MODELS.md 7).
pub const REVOKED_MEMORY: Duration = Duration::from_secs(60);
use crate::tick::{TickMetrics, TickScheduler};
use crate::world::ZoneWorld;

pub struct ZoneConfig {
    pub rate: TickRate,
    pub open: bool,
    pub seed: u64,
    pub max_players: usize,
    pub report_every: Duration,
    /// Stop after this many ticks (tests and benchmarks).
    pub max_ticks: Option<u64>,
    /// Latest report, for tests and dashboards.
    pub report_tx: Option<watch::Sender<ZoneReport>>,
    /// Abilities and preset builds (MATRIX.md 10).
    pub content: ContentPack,
    /// The looks of the content (CONTENT.md 6): the prop keys a `Look` indexes, and what
    /// each template and primary is held as. Empty: nobody holds anything.
    pub looks: gm_content::looks::Looks,
    /// Preset given to clients that ask for none.
    pub default_build: String,
    /// The hub this zone runs under (HUB.md); `None` = open development zone.
    pub hub: Option<Arc<HubLink>>,
    /// A wild zone (COMPANIONS.md 3.1): every human is team 1 and the map's creatures stand
    /// on their posts. Otherwise a team zone, as the arena.
    pub wild: bool,
    /// Companions come along with their commanders (COMPANIONS.md 3.2).
    pub squads: bool,
    /// Preset builds the zone lends to fill squad slots hires left empty (tutorial zones).
    pub recruits: Vec<String>,
    /// Every arrival starts at the map's spawns; a saved position is not resumed. For
    /// dungeons: nobody logs out past the gate and comes back at the boss's feet.
    pub arrive_at_entry: bool,
    /// Record fights between players and reports as replays (ANTICHEAT.md 3); the live aim
    /// statistics run with it.
    pub replay: Option<ReplayConfig>,
    /// What a body wears does not change until it has neither dealt nor taken damage for
    /// this long (ITEMS.md 5).
    pub gear_after_fight: Duration,
    /// Characters that are game masters here whatever the hub says (GM.md 1): an open
    /// zone's way, and a hand for tests.
    pub gm_names: Vec<String>,
    /// Where the game masters' tuning is read from at the start and written after every
    /// change (GM.md 3); none: a tuning lives as long as the zone.
    pub tuning_file: Option<std::path::PathBuf>,
}

/// Where replays go and how much of them an hour may hold.
#[derive(Clone, Debug)]
pub struct ReplayConfig {
    pub dir: std::path::PathBuf,
    /// Bytes of fight files per hour, before compression; reports are always written.
    pub bytes_per_hour: u64,
    /// The zone's name in file names and headers.
    pub zone: String,
}

impl Default for ZoneConfig {
    fn default() -> Self {
        ZoneConfig {
            rate: TickRate::COMBAT,
            open: true,
            seed: 1,
            max_players: 64,
            report_every: Duration::from_secs(5),
            max_ticks: None,
            report_tx: None,
            content: gm_core::sim::test_content::pack(TickRate::COMBAT),
            looks: Default::default(),
            default_build: "blade".into(),
            hub: None,
            wild: false,
            squads: false,
            recruits: Vec::new(),
            arrive_at_entry: false,
            replay: None,
            gear_after_fight: GEAR_AFTER_FIGHT,
            gm_names: Vec::new(),
            tuning_file: None,
        }
    }
}

/// A body that left can still be reported for this long (ANTICHEAT.md 5).
const REPORTABLE_AFTER_LEAVE: Duration = Duration::from_secs(120);
/// Chat lines a zone relays: this many a second over time, this many at once
/// (PROTOCOL.md 8). Past it lines are dropped.
const ZONE_CHAT_PER_SEC: f32 = 10.0;
const ZONE_CHAT_BURST: f32 = 30.0;

/// What a replay keeps of a simulation event (ANTICHEAT.md 3.1).
fn replay_event(ev: &ZoneEvent) -> Option<gm_replay::Event> {
    use gm_replay::{Event, Hit};
    Some(match *ev {
        ZoneEvent::Hit {
            attacker,
            target,
            amount,
            kind,
            absorbed,
            ..
        } => Event::Hit {
            attacker,
            target,
            amount,
            kind: match kind {
                HitKind::Melee => Hit::Melee,
                HitKind::Projectile => Hit::Projectile,
                HitKind::Area => Hit::Area,
                HitKind::Dot => Hit::Dot,
            },
            absorbed,
        },
        ZoneEvent::Killed { victim, killer } => Event::Killed { victim, killer },
        ZoneEvent::Parried { defender, attacker } => Event::Parried { defender, attacker },
        ZoneEvent::GuardBroken(id) => Event::GuardBroken(id),
        ZoneEvent::Respawned(id) => Event::Respawned(id),
        ZoneEvent::ProjectileSpawned {
            id,
            owner,
            view_lag: lag,
            lag: honoured,
            speed,
            gravity,
            lifetime,
            origin,
            ..
        } => Event::Shot {
            projectile: id,
            owner,
            origin: origin.into(),
            speed,
            gravity,
            lifetime,
            lag,
            honoured,
        },
        _ => return None,
    })
}

/// Say what a replay file holds: one line for the file, one per participant.
/// A client's body leaves the zone for good (its session ended, another zone claimed it,
/// its ghost ran out, it joined again): its aim numbers are logged and go to the hub
/// (ANTICHEAT.md 4.3), and it can still be reported for a while.
fn depart(
    recorder: &mut Option<Recorder>,
    hub: Option<&Arc<HubLink>>,
    recent_left: &mut Vec<(EntityId, CharacterId, Instant)>,
    id: EntityId,
    character: Option<CharacterId>,
) {
    recent_left.retain(|(_, _, at)| at.elapsed() < REPORTABLE_AFTER_LEAVE);
    recent_left.push((id, character.unwrap_or(0), Instant::now()));
    let Some(rec) = recorder else {
        return;
    };
    let name = rec.entry(id).map_or_else(String::new, |e| e.name.clone());
    let aim = rec.analyser.take(id);
    info!(entity = id, name = %name, "aim at leave: {}", aim.line());
    if let (Some(link), Some(character)) = (hub, character)
        && !aim.is_empty()
    {
        let link = link.clone();
        tokio::spawn(async move { link.aim(character, aim).await });
    }
}

fn replay_written(w: &Written) {
    info!(
        path = %w.path.display(),
        reason = ?w.header.reason,
        bytes = w.bytes.len(),
        seconds = format_args!("{:.1}", w.seconds),
        kills = w.kills,
        damage = w.damage,
        participants = w.participants.len(),
        "replay written"
    );
    for (who, aim) in &w.participants {
        info!(file = %w.path.display(), name = %who.name, "replay aim: {}", aim.line());
    }
}

/// What a body holds (LOOK.md 6.1): the model of the weapon it wears, else the prop of
/// its primary ability, else nothing; as an index into the pack's prop list. In the gun
/// mode (MODES.md 3.7) the hand is the weapon switched to, `1 2 3`, whatever is worn:
/// the prop of the ability in hand.
/// The stacks of a reading, as the simulation takes them (MODES.md 11.2).
fn stacks_of(reading: &GearReading) -> Vec<(String, u32, Option<i32>)> {
    reading
        .stacks
        .iter()
        .map(|s| (s.template.clone(), s.quantity, s.heals))
        .collect()
}

/// The body `id` spent `quantity` of its stacks that `which` picks (MODES.md 11.2), the
/// first first: the zone's copy is lowered at once (the next spend picks the right rows),
/// and the hub is told once per row; each reading comes back as a `Worn` nobody waits for.
/// The simulation's reserve is the sum of the stacks, so a spend may span two of them;
/// asked of one row for more than it holds, the hub would refuse and its reading would
/// hand the rounds back.
fn consume_stack(
    cfg: &ZoneConfig,
    hub_slots: &mut BTreeMap<EntityId, HubSlot>,
    event_tx: &mpsc::Sender<ClientEvent>,
    id: EntityId,
    which: impl Fn(&StackReading) -> bool,
    quantity: u32,
) {
    let Some(link) = cfg.hub.clone() else {
        return;
    };
    let Some(slot) = hub_slots.get_mut(&id) else {
        return;
    };
    let character = slot.character;
    let mut left = quantity;
    for stack in slot
        .stacks
        .iter_mut()
        .filter(|s| s.quantity > 0 && which(s))
    {
        if left == 0 {
            break;
        }
        let take = left.min(stack.quantity);
        let item = stack.item;
        stack.quantity -= take;
        left -= take;
        let link = link.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let result = link.consume(character, item, take).await;
            if let Err(why) = &result {
                warn!(character, item, quantity = take, %why, "consume");
            }
            let _ = tx
                .send(ClientEvent::Worn {
                    id,
                    character,
                    item: 0,
                    result,
                    tell: false,
                })
                .await;
        });
    }
    if left > 0 {
        warn!(
            character,
            quantity, left, "consume: more spent than the stacks carried"
        );
    }
}

fn look_of(
    looks: &gm_content::looks::Looks,
    zone: &Zone,
    hub_slots: &std::collections::BTreeMap<u32, HubSlot>,
    body: u32,
) -> Look {
    let player = zone.player(body);
    if let Some(p) = player
        && p.sheet.kit.mode == gm_core::vocab::Mode::Gun
    {
        let in_hand = p
            .mover
            .in_hand(&p.sheet.kit)
            .and_then(|slot| p.sheet.kit.abilities.get(slot as usize))
            .map(|a| a.id.0.saturating_sub(1) as usize);
        return Look {
            held: looks.held(None, in_hand),
            worn: Look::NONE,
            off: Look::NONE,
        };
    }
    let worn = hub_slots
        .get(&body)
        .map(|s| s.worn[0].as_str())
        .filter(|t| !t.is_empty());
    let primary = player.map(|p| p.sheet.build.primary as usize);
    // The off hand (LOOK.md 6.5): the guard's prop, a shield for a shield wall.
    let guard = player.and_then(|p| p.sheet.build.guard.map(|g| g as usize));
    Look {
        held: looks.held(worn, primary),
        worn: Look::NONE,
        off: looks.off(guard),
    }
}

fn wire_kind(kind: gm_ai::BodyKind) -> BodyKind {
    match kind {
        gm_ai::BodyKind::Human => BodyKind::Human,
        gm_ai::BodyKind::Companion { owner } => BodyKind::Companion { owner },
        gm_ai::BodyKind::Creature { def } => BodyKind::Creature { def },
    }
}

fn wire_order(order: gm_ai::Order) -> control::Order {
    match order {
        gm_ai::Order::Follow => control::Order::Follow,
        gm_ai::Order::Hold => control::Order::Hold,
        gm_ai::Order::MoveTo(p) => control::Order::MoveTo(p.into()),
        gm_ai::Order::Attack(id) => control::Order::Attack(id),
    }
}

fn ai_order(order: control::Order) -> gm_ai::Order {
    match order {
        control::Order::Follow => gm_ai::Order::Follow,
        control::Order::Hold => gm_ai::Order::Hold,
        control::Order::MoveTo(p) => gm_ai::Order::MoveTo(p.into()),
        control::Order::Attack(id) => gm_ai::Order::Attack(id),
    }
}

/// A commander's squad as its client is told.
fn squad_entries(director: &Director, zone: &Zone, commander: EntityId) -> Vec<SquadEntry> {
    director
        .squad(commander)
        .iter()
        .map(|m| SquadEntry {
            id: m.id,
            name: m.name.clone(),
            role: m.role() as u8,
            order: wire_order(m.mind.order()),
            max_health: zone
                .player(m.id)
                .map_or(0, |p| p.max_health().clamp(0, u16::MAX as i32) as u16),
            recruit: m.recruit,
        })
        .collect()
}

/// A recruit's name: the preset's, capitalised.
fn recruit_name(preset: &str) -> String {
    let mut c = preset.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => "Recruit".into(),
    }
}

/// Hub bookkeeping of one session.
struct HubSlot {
    character: CharacterId,
    joined: Instant,
    play_seconds_before: u32,
    last_save: Instant,
    /// Set when a travel ticket went out; the body is a ghost until the claim or the timeout.
    ghost_since: Option<Instant>,
    /// Another zone claimed the character: no save on leave.
    claimed_elsewhere: bool,
    /// The number of the hub's reading of its gear that the body has (ITEMS.md 3.3): a
    /// reading with a smaller number that arrives later changes nothing.
    gear_seq: u64,
    /// The templates worn by place, from the same reading (LOOK.md 6.2): what it holds.
    worn: [String; 2],
    /// The stacks it carries, from the same reading (MODES.md 11.2): the firearms'
    /// reserves and the kits, and which item row each is, for the hub's books.
    stacks: Vec<StackReading>,
    /// Its item bar, from the same reading (LOOK.md 3.2): the template on each cell.
    bar: Vec<Option<String>>,
}

/// An invitation for a character that has no body here yet is kept this long (the
/// invitation's own life, PARTY.md 3.2).
const INVITE_KEPT: Duration = Duration::from_secs(gm_hub_proto::protocol::INVITE_SECS);

/// A reading of gear for a character that has no body here at the moment is kept this
/// long: an answer to the body it had, arriving while it joins again.
const GEAR_KEPT: Duration = Duration::from_secs(120);
/// How long the zone waits for the hub on a player's buy or wear before it says so and
/// lets the player ask again.
const HUB_PATIENCE: Duration = Duration::from_secs(10);

/// What a body wears does not change in a fight (ITEMS.md 5): not until it has neither
/// dealt nor taken damage for this long.
pub const GEAR_AFTER_FIGHT: Duration = Duration::from_secs(10);
/// What a player is told when the hub did not answer in `HUB_PATIENCE`.
const HUB_SILENT: &str = "the hub did not answer: try again";
use gm_net::control::TRAINER_REACH;

/// A line of the zone itself to one client (a refusal, a notice): it is not lost.
fn zone_line(session: &Session, text: impl Into<String>) {
    session.send_control(FromZone::ChatFrom {
        from: 0,
        text: text.into(),
    });
}

/// The session of the body a character has here.
fn session_of<'a>(
    hub_slots: &BTreeMap<EntityId, HubSlot>,
    sessions: &'a BTreeMap<EntityId, Session>,
    character: CharacterId,
) -> Option<&'a Session> {
    let (id, _) = hub_slots.iter().find(|(_, s)| s.character == character)?;
    sessions.get(id)
}

/// The hub's news of a party (PARTY.md 3.2), from an answer or a notice: every character
/// here that it changes something for is told what its party is now, and, when it is in
/// one or out of one by this, in a line. (The number its body fights under follows when
/// the body is in no fight.)
fn party_news(
    parties: &mut ZoneParties,
    news: &PartyNews,
    hub_slots: &BTreeMap<EntityId, HubSlot>,
    sessions: &BTreeMap<EntityId, Session>,
) {
    let was: Vec<(CharacterId, bool)> = news
        .party
        .members
        .iter()
        .map(|(c, _)| *c)
        .chain(news.left.iter().copied())
        .map(|c| (c, parties.of(c).is_some()))
        .collect();
    for character in parties.news(news, Instant::now()) {
        let Some(s) = session_of(hub_slots, sessions, character) else {
            continue;
        };
        let names = parties.names(character);
        let before = was.iter().any(|(c, in_one)| *c == character && *in_one);
        match (before, names.is_empty()) {
            (false, false) => zone_line(s, format!("you are in a party: {}", names.join(", "))),
            (true, true) if news.party.members.is_empty() => zone_line(s, "the party is no more"),
            (true, true) => zone_line(s, "you are out of the party"),
            _ => {}
        }
        s.send_control(FromZone::Party(names));
    }
}

/// A stall as clients see it: the tile's centre, the keeper's looks. `None` when the tile is
/// not on this map (a stall left over from another build of it).
fn stall_entry(
    world: &ZoneWorld,
    s: &StallSummary,
    revoked: &[(ModelId, Instant)],
) -> Option<StallEntry> {
    let (grid, centre) = world
        .stall_grids
        .iter()
        .find_map(|g| Some((g, g.centre(s.tile_x, s.tile_y)?)))?;
    Some(StallEntry {
        id: s.id,
        pos: centre.into(),
        yaw: grid.yaw,
        owner: s.owner_name.clone(),
        frame: s.frame,
        armour: s.armour,
        model: s
            .model
            .filter(|m| m.frame == s.frame && !revoked.iter().any(|(id, _)| *id == m.id))
            .map(|m| m.id),
    })
}

fn character_state(
    zone: &Zone,
    link: &HubLink,
    id: EntityId,
    slot: &HubSlot,
) -> Option<CharacterState> {
    let p = zone.player(id)?;
    // The build the character chose (MATRIX.md 9.1): in a team zone one it asked for is
    // worn at the next respawn, and is what it wears when it next enters anywhere.
    let build = p
        .pending_build
        .clone()
        .unwrap_or_else(|| p.sheet.build.clone());
    Some(CharacterState {
        build,
        zone: Some(link.zone.clone()),
        position: p.mover.mv.origin.into(),
        yaw: p.mover.yaw,
        viewport: 0,
        play_seconds: slot.play_seconds_before + slot.joined.elapsed().as_secs() as u32,
    })
}

/// Counters over one report window plus running totals.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ZoneReport {
    pub tick: u64,
    pub players: usize,
    pub window_secs: f64,
    /// UDP bytes per connected player per second, from QUIC's own counters.
    pub tx_bytes_per_player_s: f64,
    pub rx_bytes_per_player_s: f64,
    pub snapshot_payload_per_player_s: f64,
    pub tick_mean_us: f64,
    pub tick_p99_us: f64,
    pub tick_max_us: f64,
    pub late_max_us: f64,
    pub overruns: u64,
    /// Where the tick went, mean microseconds over the window: network events, the
    /// simulation step, snapshot building, datagram sends.
    pub events_us_mean: f64,
    pub sim_us_mean: f64,
    pub snapshot_us_mean: f64,
    pub send_us_mean: f64,
    // Totals since start.
    pub joins: u64,
    pub leaves: u64,
    pub executed_frames: u64,
    pub starved_ticks: u64,
    pub dropped_frames: u64,
    pub oversize_drops: u64,
    pub send_failures: u64,
    pub malformed: u64,
    pub hits_melee: u64,
    pub hits_projectile: u64,
    pub hits_area: u64,
    pub hits_dot: u64,
    pub parries: u64,
    pub guard_breaks: u64,
    pub staggers: u64,
    pub statuses_applied: u64,
    pub kills: u64,
    /// Kills per team (index 0 = team 0 / none, 1, 2).
    pub team_kills: [u64; 3],
    pub max_tx_bytes_per_player_s: f64,
    pub max_rx_bytes_per_player_s: f64,
    /// Counters of the players connected right now (not yet folded into the totals).
    pub live_executed_frames: u64,
    pub live_starved_ticks: u64,
    /// Minds the zone runs (companions and creatures) and what they cost, mean microseconds
    /// per tick over the window (COMPANIONS.md 14).
    pub minds: usize,
    pub minds_us_mean: f64,
    /// Encounters, totals since start.
    pub encounters_engaged: u64,
    pub encounters_reset: u64,
    pub encounters_cleared: u64,
    /// The time the last cleared encounter took, seconds.
    pub last_clear_secs: u32,
    pub loot_items: u64,
    pub trials_passed: u64,
    pub orders: u64,
    pub orders_refused: u64,
    /// The recorder and the aim analysis (ANTICHEAT.md 10): mean microseconds per tick over
    /// the window, frames and their bytes since start, files written, what the ring holds.
    /// Chat lines relayed, and lines dropped by the zone's ceiling.
    pub chat_lines: u64,
    pub chat_dropped: u64,
    pub record_us_mean: f64,
    /// The part of it that is the line-of-sight sweep (ANTICHEAT.md 4.2).
    pub sight_us_mean: f64,
    pub replay_frames: u64,
    pub replay_frame_bytes: u64,
    pub replays_written: u64,
    pub replay_ring_bytes: usize,
}

/// Run the zone until `shutdown` resolves or `max_ticks` is reached. The endpoint must already
/// be bound; it is closed on exit.
pub async fn run(
    cfg: ZoneConfig,
    world: Arc<ZoneWorld>,
    endpoint: quinn::Endpoint,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<ZoneReport> {
    run_with_web(cfg, world, endpoint, None, shutdown).await
}

/// `run`, with a WebTransport listener beside the QUIC endpoint (WEB.md 2.1).
pub async fn run_with_web(
    cfg: ZoneConfig,
    world: Arc<ZoneWorld>,
    endpoint: quinn::Endpoint,
    web: Option<crate::net::WebListener>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<ZoneReport> {
    let rate = cfg.rate;
    let (tx, mut rx) = mpsc::channel::<ClientEvent>(EVENT_CHANNEL);
    let net_cfg = Arc::new(NetConfig {
        hz: rate.hz() as u16,
        map_name: world.name.clone(),
        map_hash: world.hash,
        open: cfg.open,
        props: Arc::new(cfg.looks.props.clone()),
        hub: cfg.hub.clone(),
        chat: Default::default(),
    });
    let web_acceptor =
        web.map(|w| tokio::spawn(crate::net::accept_loop_web(w, tx.clone(), net_cfg.clone())));
    let acceptor = tokio::spawn(accept_loop(endpoint.clone(), tx.clone(), net_cfg));
    let hub_stats: Arc<std::sync::Mutex<(u32, f32)>> = Arc::new(std::sync::Mutex::new((0, 0.0)));
    if let Some(link) = &cfg.hub {
        link.spawn_heartbeat(hub_stats.clone());
        link.spawn_notice_reader(tx.clone());
    }
    let event_tx = tx.clone();
    drop(tx);
    let mut hub_slots: BTreeMap<EntityId, HubSlot> = BTreeMap::new();
    // The gun hand each player was last seen with (MODES.md 3.7): a change is a `Look`.
    let mut hands_seen: HashMap<EntityId, u8> = HashMap::new();
    let mut revoked: Vec<(ModelId, Instant)> = Vec::new();
    // The market: open stalls by id (the hub is their owner; this is its mirror).
    let mut stalls: BTreeMap<i64, StallSummary> = BTreeMap::new();
    // Readings of gear for characters that have no body here at the moment.
    let mut gear_kept: HashMap<CharacterId, (GearReading, Instant)> = HashMap::new();
    // The hub's parties as this zone has them (PARTY.md 2), the humans whose body does
    // not carry its party's number yet because it is in a fight, and the requests to
    // trade that wait for the other's: who asked whom, and when.
    let mut parties = ZoneParties::default();
    let mut party_waits: HashSet<EntityId> = HashSet::new();
    let mut trade_asks: HashMap<EntityId, (EntityId, Instant)> = HashMap::new();
    // Whom a request to trade was told to, and when: once per pair in the time it waits.
    let mut trade_told: HashMap<(EntityId, EntityId), Instant> = HashMap::new();
    // Invitations for characters that are on their way here: told when the body is.
    let mut invites_kept: Vec<(CharacterId, String, Instant)> = Vec::new();
    // Characters whose body left while it was in a fight, and that body: nobody who left
    // a fight comes back into it (PARTY.md 2).
    let mut left_fights: HashMap<CharacterId, EntityId> = HashMap::new();
    if let Some(link) = cfg.hub.clone().filter(|_| !world.stall_grids.is_empty()) {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            match link.stalls().await {
                Ok(list) => {
                    let _ = tx.send(ClientEvent::StallsLoaded(list)).await;
                }
                Err(e) => warn!("loading the zone's stalls: {e}"),
            }
        });
    }

    // The content as loaded, and as a game master tuned it (GM.md 3): the tuning of the
    // last run is read back; one that content refuses is dropped with a warning.
    let mut tuning = match &cfg.tuning_file {
        Some(path) => match gm_content::tuning::read(path) {
            Ok(t) => t,
            Err(e) => {
                warn!(path = %path.display(), "tuning not read: {e}");
                Tuning::default()
            }
        },
        None => Tuning::default(),
    };
    let mut pack = Arc::new(tuning.apply(&cfg.content, rate));
    if let Err(e) = pack.validate(rate) {
        warn!("the tuning read is not valid content, dropped: {e}");
        tuning = Tuning::default();
        pack = Arc::new(cfg.content.clone());
    }
    if !tuning.is_default() {
        info!(
            tempo = tuning.tempo,
            abilities = tuning.abilities.len(),
            "tuned content"
        );
    }
    let mut zone = Zone::new(rate, cfg.seed, world.spawns.clone(), (*pack).clone());
    // Minds (COMPANIONS.md): the nav grid, the creatures on their posts, the squads.
    let seeds: Vec<glam::Vec3> = world.spawns.iter().map(|s| s.origin).collect();
    let posts: Vec<CreatureSpawn> = if cfg.wild {
        world
            .creature_posts
            .iter()
            .map(|p| CreatureSpawn {
                creature: p.creature.clone(),
                encounter: p.encounter.clone(),
                origin: p.origin,
                yaw: p.yaw,
            })
            .collect()
    } else {
        Vec::new()
    };
    let minded = cfg.squads || !posts.is_empty();
    let nav_started = std::time::Instant::now();
    let mut director = Director::new(
        &mut zone,
        &world.bsp,
        &world.name,
        if minded { &seeds } else { &[] },
        &posts,
        cfg.seed,
    );
    if minded {
        info!(
            nav_nodes = director.nav.len(),
            nav_ms = format_args!("{:.1}", nav_started.elapsed().as_secs_f64() * 1000.0),
            creatures = director.minds(),
            "minds ready"
        );
    }
    director.events.clear();
    // The avatar models hired companions wear, and when their hires run out.
    let mut companion_models: BTreeMap<EntityId, ModelRef> = BTreeMap::new();
    let mut hire_expiry: BTreeMap<EntityId, u64> = BTreeMap::new();
    let mut squads_dirty: Vec<EntityId> = Vec::new();
    let resolve = |zone: &Zone, choice: Option<&BuildChoice>| -> Result<Build, String> {
        match choice {
            None => zone
                .content
                .build(&cfg.default_build)
                .cloned()
                .ok_or_else(|| format!("no default build {:?}", cfg.default_build)),
            Some(BuildChoice::Preset(name)) => zone
                .content
                .build(name)
                .cloned()
                .ok_or_else(|| format!("unknown preset {name:?}")),
            Some(BuildChoice::Custom(b)) => Ok(b.clone()),
        }
    };
    let started_unix = now_secs();
    let mut sessions: BTreeMap<EntityId, Session> = BTreeMap::new();
    let mut pvs = PvsCache::default();
    let mut table = TickTable::default();
    let mut scheduler = TickScheduler::new(rate, Instant::now());
    let mut metrics = TickMetrics::new(rate.period());
    let mut phases = Phases::default();
    // The recorder (ANTICHEAT.md 3) and what it measures as it records.
    let mut recorder = cfg.replay.as_ref().map(|r| {
        Recorder::new(RecorderConfig {
            dir: r.dir.clone(),
            zone: r.zone.clone(),
            map: world.name.clone(),
            map_hash: world.hash,
            hz: rate.hz() as u16,
            teams: !cfg.wild,
            content_hash: gm_net::fnv1a64(&bitcode::encode(&cfg.content)),
            bytes_per_hour: r.bytes_per_hour,
        })
    });
    let mut sight = crate::sight::Sight::default();
    let (written_tx, mut written_rx) = mpsc::unbounded_channel::<Written>();
    let mut writing: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    // What an earlier zone left in the directory goes to the hub now: listed before this
    // zone writes anything of its own, read and sent one at a time.
    if let (Some(link), Some(r)) = (&cfg.hub, &cfg.replay) {
        let (link, left) = (link.clone(), Written::leftovers(&r.dir));
        if !left.is_empty() {
            tokio::spawn(async move {
                for path in left {
                    match tokio::task::spawn_blocking(move || Written::read(path)).await {
                        Ok(Ok(w)) => {
                            info!(file = %w.path.display(), "a replay left by an earlier zone");
                            link.replay(w).await;
                        }
                        Ok(Err(e)) => warn!("a leftover replay is not uploaded: {e}"),
                        Err(_) => {}
                    }
                }
            });
        }
    }
    // Characters whose leaving save is on its way to the hub.
    let mut leaving_saves: std::collections::HashSet<CharacterId> =
        std::collections::HashSet::new();
    // The zone's chat ceiling: lines it may still relay, and as of when.
    let mut chat_lines = ZONE_CHAT_BURST;
    let mut chat_at = Instant::now();
    // Bodies that left a moment ago can still be reported: entity, character, when.
    let mut recent_left: Vec<(EntityId, CharacterId, Instant)> = Vec::new();
    let mut report = ZoneReport::default();
    let mut last_report = Instant::now();
    let shutdown = std::pin::pin!(shutdown);
    let mut shutdown = shutdown.fuse_once();

    info!(
        hz = rate.hz(),
        map = %world.name,
        spawns = world.spawns.len(),
        "zone running"
    );

    loop {
        let deadline = scheduler.next_deadline();
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown requested");
                break;
            }
            _ = tokio::time::sleep_until(deadline) => {}
        }
        let started = Instant::now();
        if let Some(behind) = scheduler.resync_if_behind(started) {
            warn!(
                behind_ms = behind.as_millis(),
                "fell behind; resynchronising"
            );
        }

        // Network events since the last tick.
        let t_events = Instant::now();
        while let Ok(ev) = rx.try_recv() {
            match ev {
                ClientEvent::Join {
                    name,
                    build,
                    team,
                    hub,
                    conn,
                    control,
                    reply,
                } => {
                    // The same character again (a reconnect the hub allowed): drop the old
                    // body first, so that it does not count against the room it frees.
                    // Not while that body is in a fight (PARTY.md 2): a new body would be
                    // alive where it is dead and held, so the fight goes on with the body
                    // it has (its own connection, or nobody's until it is kicked), and
                    // the newcomer is turned away.
                    if let Some(h) = &hub
                        && let Some(old) = hub_slots
                            .iter()
                            .find(|(_, s)| s.character == h.character)
                            .map(|(id, _)| *id)
                    {
                        if director.engaged(old) {
                            let why =
                                "your body here is still in a fight: come back when it is over";
                            let _ = reply.send(Err(why.into()));
                            continue;
                        }
                        // What the old body wore, by the hub's own numbering, is kept for
                        // the new one: the claim's reading may be the older of the two.
                        if let (Some(slot), Some(p)) = (hub_slots.get(&old), zone.player(old)) {
                            let reading = GearReading {
                                seq: slot.gear_seq,
                                gear: p.gear,
                                templates: slot.worn.clone(),
                                stacks: slot.stacks.clone(),
                                bar: slot.bar.clone(),
                            };
                            gear_kept.insert(h.character, (reading, Instant::now()));
                        }
                        // (Its party's reading is kept the same way, by `parties`.)
                        parties.left(h.character, Instant::now());
                        director.human_left(&mut zone, old);
                        zone.remove_player(old);
                        if let Some(s) = sessions.remove(&old) {
                            s.conn.close(4, b"character joined again");
                        }
                        hub_slots.remove(&old);
                        hands_seen.remove(&old);
                        party_waits.remove(&old);
                        trade_asks.retain(|asker, (asked, _)| *asker != old && *asked != old);
                        // (The old session's own leave finds nothing left to announce.)
                        for s in sessions.values() {
                            s.send_control(FromZone::PlayerLeft(old));
                        }
                        depart(
                            &mut recorder,
                            cfg.hub.as_ref(),
                            &mut recent_left,
                            old,
                            Some(h.character),
                        );
                    }
                    // Nobody who left a fight comes back into it (PARTY.md 2): a new body
                    // would be alive where the old one was dead and held.
                    left_fights.retain(|_, old| director.in_encounter(*old));
                    if let Some(h) = &hub
                        && left_fights.contains_key(&h.character)
                    {
                        let why = "the fight you left here is not over: come back when it is";
                        let _ = reply.send(Err(why.into()));
                        continue;
                    }
                    if sessions.len() >= cfg.max_players {
                        let _ = reply.send(Err("zone full".into()));
                        continue;
                    }
                    let build = match resolve(&zone, build.as_ref()) {
                        Ok(b) => b,
                        Err(e) => {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                    };
                    let team = if cfg.wild {
                        1
                    } else if team == 0 || team > 2 {
                        zone.smallest_team()
                    } else {
                        team
                    };
                    // A saved position is used when it is still a place to stand (the map
                    // may have been rebuilt since); otherwise the player spawns.
                    let resume = hub.as_ref().and_then(|h| h.origin).filter(|(origin, _)| {
                        !cfg.arrive_at_entry
                            && origin.is_finite()
                            && world.bsp.point_contents(Hull::Player, *origin) == Contents::Empty
                    });
                    let id = match resume {
                        Some((origin, yaw)) => {
                            if let Err(e) = build.validate(&zone.content) {
                                let _ = reply.send(Err(format!("invalid build: {e}")));
                                continue;
                            }
                            zone.add_player_at(build.clone(), team, origin, yaw)
                        }
                        None => match zone.add_player(&world.bsp, build.clone(), team) {
                            Ok(id) => id,
                            Err(e) => {
                                let _ = reply.send(Err(format!("invalid build: {e}")));
                                continue;
                            }
                        },
                    };
                    revoked.retain(|(_, at)| at.elapsed() < REVOKED_MEMORY);
                    let model = hub
                        .as_ref()
                        .and_then(|h| h.model)
                        .filter(|m| !revoked.iter().any(|(id, _)| *id == m.id));
                    let hired = hub.as_ref().map(|h| h.squad.clone()).unwrap_or_default();
                    let hub_gm = hub.as_ref().is_some_and(|h| h.gm);
                    if let Some(h) = hub {
                        // What it wears (ITEMS.md 3.3): the hub's reading at the claim, or a
                        // later one this zone already has for the character (the answer to
                        // a change its last body asked for). From here on it changes only
                        // through this zone.
                        let mut reading = h.gear.clone();
                        if let Some((kept, _)) = gear_kept.remove(&h.character)
                            && kept.seq > reading.seq
                        {
                            reading = kept;
                        }
                        zone.set_gear(id, reading.gear);
                        zone.set_stacks(id, &stacks_of(&reading), &reading.bar);
                        // Its party (PARTY.md 4): the claim's reading, or a newer one the
                        // zone was told of before the body was here. The number its body
                        // carries follows before this tick is simulated, unless its
                        // party is in a fight here: that roster is closed.
                        parties.joined(h.character, h.party);
                        hub_slots.insert(
                            id,
                            HubSlot {
                                gear_seq: reading.seq,
                                worn: reading.templates.clone(),
                                stacks: reading.stacks.clone(),
                                bar: reading.bar.clone(),
                                character: h.character,
                                joined: Instant::now(),
                                play_seconds_before: h.play_seconds,
                                // Staggered so a zone full of players never saves at once.
                                last_save: Instant::now()
                                    - Duration::from_secs(
                                        (id % SAVE_EVERY.as_secs() as u32) as u64,
                                    ),
                                ghost_since: None,
                                claimed_elsewhere: false,
                            },
                        );
                    }
                    let _ = reply.send(Ok(JoinInfo {
                        entity: id,
                        server_tick: zone.tick,
                        build,
                        team,
                        pack: pack.clone(),
                    }));
                    let mut session = Session::new(id, name.clone(), conn, control);
                    session.last_input_at = scheduler.tick();
                    // A game master (GM.md 1): the hub's word, or the zone's own list.
                    session.gm = hub_gm || cfg.gm_names.contains(&name);
                    if session.gm {
                        info!(entity = id, %name, "game master");
                        session.send_control(FromZone::Gm(GmNews::Granted));
                        if !tuning.is_default() {
                            session.send_control(FromZone::Gm(GmNews::Tuning(tuning.clone())));
                        }
                    }
                    session.model = model;
                    session.announced = zone.player(id).and_then(|p| session.wears(p.frame()));
                    let look = look_of(&cfg.looks, &zone, &hub_slots, id);
                    for s in sessions.values() {
                        s.send_control(FromZone::PlayerInfo {
                            id,
                            name: name.clone(),
                            team,
                            model: session.announced,
                            kind: BodyKind::Human,
                            look,
                        });
                    }
                    sessions.insert(id, session);
                    // The squad (COMPANIONS.md 3.2): hired avatars first, then the zone's
                    // recruits for the slots left.
                    if cfg.squads {
                        for h in hired {
                            let spec = CompanionSpec {
                                name: h.name.clone(),
                                build: h.build.clone(),
                                recruit: false,
                                hire: Some(h.hire),
                            };
                            if let Some(c) = director.add_companion(&mut zone, &world.bsp, id, spec)
                            {
                                hire_expiry.insert(c, h.expires_at);
                                if let Some(m) = h.model.filter(|m| {
                                    m.frame == crate::session::frame_index(h.build.frame)
                                        && !revoked.iter().any(|(r, _)| *r == m.id)
                                }) {
                                    companion_models.insert(c, m);
                                }
                            }
                        }
                        for preset in &cfg.recruits {
                            let Some(build) = zone.content.build(preset).cloned() else {
                                continue;
                            };
                            let spec = CompanionSpec {
                                name: recruit_name(preset),
                                build,
                                recruit: true,
                                hire: None,
                            };
                            director.add_companion(&mut zone, &world.bsp, id, spec);
                        }
                    }
                    // The joiner learns everyone, itself included, in one message: a town
                    // of hundreds must not overflow its control queue.
                    let mut roster: Vec<PlayerEntry> = sessions
                        .values()
                        .map(|s| PlayerEntry {
                            id: s.id,
                            name: s.name.clone(),
                            team: zone.player(s.id).map_or(0, |p| p.team()),
                            model: s.announced,
                            kind: BodyKind::Human,
                            look: look_of(&cfg.looks, &zone, &hub_slots, s.id),
                        })
                        .collect();
                    roster.extend(director.driven().into_iter().map(|(body, name, kind)| {
                        PlayerEntry {
                            id: body,
                            name,
                            team: zone.player(body).map_or(0, |p| p.team()),
                            model: companion_models.get(&body).map(|m| m.id),
                            kind: wire_kind(kind),
                            look: look_of(&cfg.looks, &zone, &hub_slots, body),
                        }
                    }));
                    sessions[&id].send_control(FromZone::Roster(roster));
                    if let Some(slot) = hub_slots.get(&id) {
                        if parties.of(slot.character).is_some() {
                            let names = parties.names(slot.character);
                            sessions[&id].send_control(FromZone::Party(names));
                        }
                        // Whoever invited it while it was on its way.
                        let character = slot.character;
                        invites_kept.retain(|(to, from, at)| {
                            let waits = at.elapsed() < INVITE_KEPT;
                            if waits && *to == character {
                                sessions[&id]
                                    .send_control(FromZone::Invited { from: from.clone() });
                            }
                            waits && *to != character
                        });
                    }
                    if !stalls.is_empty() {
                        sessions[&id].send_control(FromZone::Stalls(
                            stalls
                                .values()
                                .filter_map(|s| stall_entry(&world, s, &revoked))
                                .collect(),
                        ));
                    }
                    report.joins += 1;
                }
                ClientEvent::Order { id, slots, order } => {
                    let Some(session) = sessions.get(&id) else {
                        continue;
                    };
                    let visible = |target: EntityId| session.sees(target);
                    match director.order(&zone, id, slots, ai_order(order), &visible) {
                        Ok(()) => report.orders += 1,
                        Err(why) => {
                            report.orders_refused += 1;
                            session.send_control(FromZone::OrderRefused(why));
                        }
                    }
                }
                ClientEvent::HubHireEnded { hirer, hire } => {
                    // The companion leaves now, or when its encounter ends (COMPANIONS.md 3.3).
                    if let Some(commander) = hub_slots
                        .iter()
                        .find(|(_, s)| s.character == hirer)
                        .map(|(id, _)| *id)
                        && let Some(m) = director
                            .squad(commander)
                            .iter()
                            .find(|m| m.hire == Some(hire))
                    {
                        hire_expiry.insert(m.id, 0);
                    }
                }
                ClientEvent::Respec { id, build } => {
                    // MATRIX.md 9.1: in a team zone (the arena, the practice ground) a new
                    // build is worn at the next respawn, as always; in the world it is
                    // worn at once, at the trainer, by a living body out of any fight.
                    let result = resolve(&zone, Some(&build)).and_then(|b| {
                        if !cfg.wild {
                            return zone.request_respec(id, b).map_err(|e| e.to_string());
                        }
                        let p = zone
                            .player(id)
                            .filter(|p| p.alive && !p.ghost)
                            .ok_or("not now")?;
                        let fight = zone
                            .rate
                            .ms_to_ticks(cfg.gear_after_fight.as_millis() as u32);
                        if p.fought_within(zone.tick, fight) {
                            return Err("not in a fight: wait a moment".into());
                        }
                        let at = p.mover.mv.origin;
                        let near_trainer = zone.players().any(|t| {
                            t.alive
                                && t.unhurt
                                && (t.mover.mv.origin - at).length() <= TRAINER_REACH
                        });
                        if !near_trainer {
                            return Err("stand by the trainer in the town".into());
                        }
                        zone.respec_now(id, b).map_err(|e| e.to_string())
                    });
                    // Under a hub the build it chose is saved at once, and the answer is
                    // the save's: a build worn here that the hub does not hold would be
                    // the old one again at the next claim. (The periodic save carries it
                    // too, as it carries a pending one.)
                    let saving = result.is_ok()
                        && cfg
                            .hub
                            .as_ref()
                            .zip(hub_slots.get_mut(&id))
                            .and_then(|(link, slot)| {
                                let state = character_state(&zone, link, id, slot)?;
                                slot.last_save = Instant::now();
                                let (link, tx, character) =
                                    (link.clone(), event_tx.clone(), slot.character);
                                tokio::spawn(async move {
                                    let saved = link.save_told(character, state).await;
                                    let result = match saved {
                                        Ok(seq) => {
                                            let _ = tx
                                                .send(ClientEvent::PartySeq { character, seq })
                                                .await;
                                            Ok(())
                                        }
                                        Err(why) => Err(why),
                                    };
                                    let _ = tx.send(ClientEvent::RespecSaved { id, result }).await;
                                });
                                Some(())
                            })
                            .is_some();
                    if !saving && let Some(s) = sessions.get(&id) {
                        s.send_control(FromZone::RespecResult(result));
                    }
                }
                ClientEvent::RespecSaved { id, result } => {
                    if let Err(why) = &result {
                        warn!(entity = id, "a respec was worn but not saved: {why}");
                    }
                    if let Some(s) = sessions.get(&id) {
                        s.send_control(FromZone::RespecResult(
                            result.map_err(|why| format!("worn here, but not saved: {why}")),
                        ));
                    }
                }
                ClientEvent::Gm { id, op } => {
                    let Some(s) = sessions.get(&id) else { continue };
                    if !s.gm {
                        s.send_control(FromZone::Gm(GmNews::Refused("not a game master".into())));
                        continue;
                    }
                    let name = s.name.clone();
                    // A change of the tuning is tried on the content as loaded; what content
                    // would refuse is refused here, and the tuning stands as it was.
                    let mut tuned = None;
                    let result: Result<(), String> = match op {
                        GmOp::Tempo(t) => {
                            let (lo, hi) = gm_core::tuning::TEMPO_RANGE;
                            if !(lo..=hi).contains(&t) {
                                Err(format!("the tempo is between {lo} and {hi}"))
                            } else {
                                let mut next = tuning.clone();
                                next.tempo = t;
                                tuned = Some(next);
                                Ok(())
                            }
                        }
                        GmOp::Ability(a) => {
                            if !cfg.content.abilities.iter().any(|d| d.key == a.key) {
                                Err(format!("no ability {}", a.key))
                            } else {
                                let mut next = tuning.clone();
                                next.set(a);
                                tuned = Some(next);
                                Ok(())
                            }
                        }
                        GmOp::ResetTuning => {
                            tuned = Some(Tuning::default());
                            Ok(())
                        }
                        GmOp::Heal { everyone } => {
                            let ids: Vec<EntityId> = if everyone {
                                zone.players().map(|p| p.id).collect()
                            } else {
                                vec![id]
                            };
                            for who in ids {
                                zone.make_whole(who);
                            }
                            info!(%name, everyone, "gm: healed");
                            Ok(())
                        }
                        GmOp::Respec(build) => {
                            let r = zone.respec_now(id, build).map_err(|e| e.to_string());
                            if r.is_ok() {
                                info!(%name, "gm: respec now");
                            }
                            r
                        }
                    };
                    let result = match tuned {
                        Some(next) if result.is_ok() => {
                            let next_pack = next.apply(&cfg.content, rate);
                            match next_pack.validate(rate) {
                                Ok(()) => {
                                    tuning = next;
                                    pack = Arc::new(next_pack);
                                    zone.retune((*pack).clone());
                                    info!(
                                        %name,
                                        tempo = tuning.tempo,
                                        abilities = tuning.abilities.len(),
                                        "gm: content tuned"
                                    );
                                    if let Some(path) = &cfg.tuning_file
                                        && let Err(e) = gm_content::tuning::write(path, &tuning)
                                    {
                                        warn!(path = %path.display(), "tuning not written: {e}");
                                    }
                                    // Everyone runs the new numbers: a new `Content` each,
                                    // and the tuning as it stands to the game masters.
                                    for (sid, s) in &sessions {
                                        let Some(p) = zone.player(*sid) else { continue };
                                        s.send_control(FromZone::Content {
                                            pack: (*pack).clone(),
                                            own: p.sheet.build.clone(),
                                            team: p.team(),
                                            props: cfg.looks.props.clone(),
                                        });
                                        if s.gm {
                                            s.send_control(FromZone::Gm(GmNews::Tuning(
                                                tuning.clone(),
                                            )));
                                        }
                                    }
                                    Ok(())
                                }
                                Err(e) => Err(format!("content refuses that: {e}")),
                            }
                        }
                        _ => result,
                    };
                    if let (Err(e), Some(s)) = (result, sessions.get(&id)) {
                        s.send_control(FromZone::Gm(GmNews::Refused(e)));
                    }
                }
                ClientEvent::Travel { id, zone: to_zone } => {
                    let Some(link) = cfg.hub.clone() else {
                        if let Some(s) = sessions.get(&id) {
                            s.send_control(FromZone::TravelRefused("no hub".into()));
                        }
                        continue;
                    };
                    let Some(slot) = hub_slots.get(&id) else {
                        continue;
                    };
                    if slot.ghost_since.is_some() {
                        continue;
                    }
                    let Some(state) = character_state(&zone, &link, id, slot) else {
                        continue;
                    };
                    // One handoff request per player at the hub, and no more than one a
                    // second: the rest of a flood is dropped here.
                    if !sessions
                        .get_mut(&id)
                        .is_some_and(|s| s.begin_travel_request())
                    {
                        continue;
                    }
                    let character = slot.character;
                    let web = sessions.get(&id).is_some_and(|s| s.conn.is_web());
                    let tx = event_tx.clone();
                    tokio::spawn(async move {
                        let result = link.handoff(character, state, to_zone, web).await;
                        let _ = tx.send(ClientEvent::TravelResult { id, result }).await;
                    });
                }
                ClientEvent::TravelResult { id, result } => {
                    let Some(s) = sessions.get_mut(&id) else {
                        continue;
                    };
                    s.end_travel_request();
                    match result {
                        Ok(ticket) => {
                            s.send_control(FromZone::TravelTicket {
                                zone: ticket.zone,
                                addr: ticket.addr.to_string(),
                                cert_der: ticket.cert_der,
                                token: bitcode::encode(&ticket.token),
                                web: ticket.web,
                            });
                            zone.set_ghost(id, true);
                            director.human_left(&mut zone, id);
                            if let Some(slot) = hub_slots.get_mut(&id) {
                                slot.ghost_since = Some(Instant::now());
                            }
                        }
                        Err(e) => {
                            s.send_control(FromZone::TravelRefused(e));
                        }
                    }
                }
                ClientEvent::HubClaimed { character } => {
                    if let Some(id) = hub_slots
                        .iter()
                        .find(|(_, s)| s.character == character)
                        .map(|(id, _)| *id)
                    {
                        if let Some(slot) = hub_slots.get_mut(&id) {
                            slot.claimed_elsewhere = true;
                        }
                        if director.engaged(id) {
                            left_fights.insert(character, id);
                        }
                        director.human_left(&mut zone, id);
                        zone.remove_player(id);
                        if let Some(s) = sessions.remove(&id) {
                            s.send_control(FromZone::Kick("claimed by another zone".into()));
                            s.conn.close(0, b"travelled");
                        }
                        hub_slots.remove(&id);
                        hands_seen.remove(&id);
                        parties.left(character, Instant::now());
                        party_waits.remove(&id);
                        trade_asks.retain(|asker, (asked, _)| *asker != id && *asked != id);
                        depart(
                            &mut recorder,
                            cfg.hub.as_ref(),
                            &mut recent_left,
                            id,
                            Some(character),
                        );
                        // The body leaves here: said once, counted once (the `Leave` that
                        // follows the closed connection finds nothing left to announce).
                        for s in sessions.values() {
                            s.send_control(FromZone::PlayerLeft(id));
                        }
                        report.leaves += 1;
                    }
                }
                ClientEvent::HubKick { character, reason } => {
                    let here = hub_slots
                        .iter()
                        .find(|(_, s)| s.character == character)
                        .and_then(|(id, _)| sessions.get(id));
                    match here {
                        Some(s) => s.kick(&reason, 3),
                        // The hub has the character here and the zone has no body for
                        // it. If it has just left, the save it left with is on its way
                        // and says so itself (a logout races its own leaving save, and
                        // that save must not find the character gone). Otherwise the hub
                        // is told, or the character would stay "here" until this zone
                        // restarts. (A client of its that is still on the way in gets a
                        // body all the same; the hub refuses its first save, and that
                        // ends it: `SaveRefused`.)
                        None if leaving_saves.contains(&character) => {}
                        None => {
                            if let Some(link) = cfg.hub.clone() {
                                tokio::spawn(async move { link.release(character).await });
                            }
                        }
                    }
                }
                ClientEvent::LeftSaved { character } => {
                    leaving_saves.remove(&character);
                }
                ClientEvent::SaveRefused { character } => {
                    // The hub does not have this character here (it was logged out or
                    // banned while its client was on the way in, or the hub lost track):
                    // what the hub does not know of does not play on. Not a traveller on
                    // its way out, whose character is the other zone's by now.
                    let here = hub_slots
                        .iter()
                        .find(|(_, s)| {
                            s.character == character
                                && s.ghost_since.is_none()
                                && !s.claimed_elsewhere
                        })
                        .and_then(|(id, _)| sessions.get(id));
                    if let Some(s) = here {
                        warn!(
                            character,
                            "the hub does not have this character here: it leaves"
                        );
                        s.kick("the hub no longer has this character here", 3);
                    }
                }
                ClientEvent::HubModelRevoked { model } => {
                    revoked.retain(|(_, at)| at.elapsed() < REVOKED_MEMORY);
                    revoked.push((model, Instant::now()));
                    for s in sessions.values_mut() {
                        if s.model.is_some_and(|m| m.id == model) {
                            s.model = None;
                            s.announced = None;
                        }
                    }
                    for stall in stalls.values_mut() {
                        if stall.model.is_some_and(|m| m.id == model) {
                            stall.model = None;
                        }
                    }
                    // Everyone forgets it, whoever wore it.
                    for s in sessions.values() {
                        s.send_control(FromZone::ModelRevoked(model));
                    }
                }
                ClientEvent::StallsLoaded(list) => {
                    stalls = list.into_iter().map(|s| (s.id, s)).collect();
                    let entries: Vec<StallEntry> = stalls
                        .values()
                        .filter_map(|s| stall_entry(&world, s, &revoked))
                        .collect();
                    info!(stalls = entries.len(), "market loaded");
                    for s in sessions.values() {
                        s.send_control(FromZone::Stalls(entries.clone()));
                    }
                }
                ClientEvent::StallOpen { id } => {
                    let Some(session) = sessions.get_mut(&id) else {
                        continue;
                    };
                    if !session.begin_stall_request() {
                        continue;
                    }
                    // Only the zone knows where the player stands: it must be alive, on a
                    // tile of the market, and the tile must be free (ECONOMY.md 7).
                    let request = (|| {
                        let link = cfg.hub.clone().ok_or("this zone has no market")?;
                        let slot = hub_slots.get(&id).ok_or("this zone has no market")?;
                        let p = zone
                            .player(id)
                            .filter(|p| p.alive && !p.ghost)
                            .ok_or("you cannot open a stall now")?;
                        let (x, y) = world
                            .stall_grids
                            .iter()
                            .find_map(|g| g.tile_at(p.mover.mv.origin))
                            .ok_or("stand on a market tile to open a stall")?;
                        // One stall per character (ECONOMY.md 7): a keeper back after a
                        // restart finds its stall standing, and is told so here rather
                        // than by the hub's constraint on every ask.
                        if stalls.values().any(|s| s.owner == slot.character) {
                            return Err("you already have a stall");
                        }
                        if stalls.values().any(|s| (s.tile_x, s.tile_y) == (x, y)) {
                            return Err("that tile is taken");
                        }
                        Ok((link, slot.character, x, y))
                    })();
                    match request {
                        Ok((link, character, x, y)) => {
                            let tx = event_tx.clone();
                            tokio::spawn(async move {
                                let result = link.stall_open(character, x, y).await;
                                let _ = tx.send(ClientEvent::StallOpened { id, result }).await;
                            });
                        }
                        Err(why) => {
                            session.end_stall_request();
                            session.send_control(FromZone::StallResult(Err(why.to_string())));
                        }
                    }
                }
                ClientEvent::StallOpened { id, result } => {
                    let answer = match result {
                        Ok(stall) => {
                            if let Some(entry) = stall_entry(&world, &stall, &revoked) {
                                for s in sessions.values() {
                                    s.send_control(FromZone::StallOpened(entry.clone()));
                                }
                            }
                            stalls.insert(stall.id, stall);
                            Ok(())
                        }
                        Err(e) => Err(e),
                    };
                    if let Some(s) = sessions.get_mut(&id) {
                        s.end_stall_request();
                        s.send_control(FromZone::StallResult(answer));
                    }
                }
                ClientEvent::StallClose { id } => {
                    let Some(session) = sessions.get_mut(&id) else {
                        continue;
                    };
                    if !session.begin_stall_request() {
                        continue;
                    }
                    match (cfg.hub.clone(), hub_slots.get(&id)) {
                        (Some(link), Some(slot)) => {
                            let character = slot.character;
                            let tx = event_tx.clone();
                            tokio::spawn(async move {
                                let result = link.stall_close(character).await;
                                let _ = tx.send(ClientEvent::StallCloseResult { id, result }).await;
                            });
                        }
                        _ => {
                            session.end_stall_request();
                            session.send_control(FromZone::StallResult(Err(
                                "this zone has no market".into(),
                            )));
                        }
                    }
                }
                ClientEvent::StallCloseResult { id, result } => {
                    // The stall itself goes when the hub's notice arrives.
                    if let Some(s) = sessions.get_mut(&id) {
                        s.end_stall_request();
                        s.send_control(FromZone::StallResult(result));
                    }
                }
                ClientEvent::HubStallClosed { stall } => {
                    if stalls.remove(&stall).is_some() {
                        for s in sessions.values() {
                            s.send_control(FromZone::StallClosed(stall));
                        }
                    }
                }
                ClientEvent::StallBuy {
                    id,
                    stall,
                    listing,
                    price,
                } => {
                    let Some(session) = sessions.get_mut(&id) else {
                        continue;
                    };
                    // A buy is always answered: somebody is looking at a screen.
                    let refuse = |session: &Session, why: &str| {
                        session.send_control(FromZone::BuyResult {
                            listing,
                            result: Err(why.to_string()),
                        });
                    };
                    if !session.begin_stall_request() {
                        refuse(session, "one thing at a time: try again in a moment");
                        continue;
                    }
                    // Only the zone knows where the buyer stands: alive, in person, and at
                    // that stall (ITEMS.md 5). The rest is the hub's to check.
                    let request = (|| {
                        let link = cfg.hub.clone().ok_or("this zone has no market")?;
                        let slot = hub_slots.get(&id).ok_or("this zone has no market")?;
                        let p = zone
                            .player(id)
                            .filter(|p| p.alive && !p.ghost)
                            .ok_or("you cannot buy now")?;
                        let at = stalls
                            .get(&stall)
                            .and_then(|s| {
                                world
                                    .stall_grids
                                    .iter()
                                    .find_map(|g| g.centre(s.tile_x, s.tile_y))
                            })
                            .ok_or("that stall has closed")?;
                        let feet = p.mover.mv.origin + Vec3::Z * Hull::Player.mins().z;
                        if !stall_in_reach(at.into(), feet.into()) {
                            return Err("walk up to the stall to buy");
                        }
                        Ok((link, slot.character))
                    })();
                    match request {
                        Ok((link, character)) => {
                            let tx = event_tx.clone();
                            tokio::spawn(async move {
                                let asked = link.stall_buy(character, stall, listing, price);
                                let result = tokio::time::timeout(HUB_PATIENCE, asked)
                                    .await
                                    .unwrap_or_else(|_| Err(HUB_SILENT.to_string()));
                                let done = ClientEvent::StallBought {
                                    id,
                                    listing,
                                    result: result.as_ref().map(|_| ()).map_err(Clone::clone),
                                };
                                let _ = tx.send(done).await;
                                // What it carries now (MODES.md 11.2): a stack bought is a
                                // reserve filled, taken as any reading of its gear is.
                                if let Ok(reading) = result {
                                    let carried = ClientEvent::Worn {
                                        id,
                                        character,
                                        item: 0,
                                        result: Ok(reading),
                                        tell: false,
                                    };
                                    let _ = tx.send(carried).await;
                                }
                            });
                        }
                        Err(why) => {
                            // The hub was not asked: this does not count against the next.
                            session.cancel_stall_request();
                            refuse(session, why);
                        }
                    }
                }
                ClientEvent::StallBought {
                    id,
                    listing,
                    result,
                } => {
                    if let Some(s) = sessions.get_mut(&id)
                        && s.end_stall_request()
                    {
                        s.send_control(FromZone::BuyResult { listing, result });
                    }
                }
                ClientEvent::Wear { id, item, on } => {
                    let Some(session) = sessions.get_mut(&id) else {
                        continue;
                    };
                    let refuse = |session: &Session, why: &str| {
                        session.send_control(FromZone::WearResult {
                            item,
                            result: Err(why.to_string()),
                        });
                    };
                    if !session.begin_gear_request() {
                        refuse(session, "one thing at a time: try again in a moment");
                        continue;
                    }
                    // What a body wears changes while it stands alive and in person, and
                    // not in a fight (ITEMS.md 5): only the zone knows either.
                    let request = (|| {
                        let link = cfg.hub.clone().ok_or("nothing is worn in this zone")?;
                        let slot = hub_slots.get(&id).ok_or("nothing is worn in this zone")?;
                        let p = zone
                            .player(id)
                            .filter(|p| p.alive && !p.ghost)
                            .ok_or("not now")?;
                        let fight = zone
                            .rate
                            .ms_to_ticks(cfg.gear_after_fight.as_millis() as u32);
                        if p.fought_within(zone.tick, fight) {
                            return Err("not in a fight: wait a moment");
                        }
                        Ok((link, slot.character))
                    })();
                    match request {
                        Ok((link, character)) => {
                            let tx = event_tx.clone();
                            tokio::spawn(async move {
                                let worn = |result, tell| ClientEvent::Worn {
                                    id,
                                    character,
                                    item,
                                    result,
                                    tell,
                                };
                                let asked = link.wear(character, item, on);
                                tokio::pin!(asked);
                                match tokio::time::timeout(HUB_PATIENCE, &mut asked).await {
                                    Ok(result) => {
                                        let _ = tx.send(worn(result, true)).await;
                                    }
                                    Err(_) => {
                                        // The player is told and may ask again; what the
                                        // hub answers in the end is still what the
                                        // character wears, and is applied when it comes.
                                        let silent = Err(HUB_SILENT.to_string());
                                        let _ = tx.send(worn(silent, true)).await;
                                        let late = asked.await;
                                        if late.is_ok() {
                                            let _ = tx.send(worn(late, false)).await;
                                        }
                                    }
                                }
                            });
                        }
                        Err(why) => {
                            session.cancel_gear_request();
                            refuse(session, why);
                        }
                    }
                }
                ClientEvent::Worn {
                    id,
                    character,
                    item,
                    result,
                    tell,
                } => {
                    // What the hub holds is what the body has, at once: a build waits for
                    // the respawn because the client predicts with it; gear it does not.
                    // It goes to the body the *character* has now (it may have joined
                    // again since it asked), and only if it is newer than what that has.
                    if let Ok(reading) = &result {
                        let here = hub_slots
                            .iter()
                            .find(|(_, slot)| slot.character == character)
                            .map(|(body, slot)| (*body, slot.gear_seq));
                        match here {
                            Some((body, gear_seq)) => {
                                if reading.seq > gear_seq {
                                    // What it held before the reading, then the reading
                                    // (Gemini's review: measured after, the look never
                                    // changed and nobody was told).
                                    let before = look_of(&cfg.looks, &zone, &hub_slots, body);
                                    if let Some(slot) = hub_slots.get_mut(&body) {
                                        slot.gear_seq = reading.seq;
                                        slot.worn = reading.templates.clone();
                                        slot.stacks = reading.stacks.clone();
                                        slot.bar = reading.bar.clone();
                                    }
                                    zone.set_gear(body, reading.gear);
                                    zone.set_stacks(body, &stacks_of(reading), &reading.bar);
                                    info!(
                                        character,
                                        dealt = ?reading.gear.dealt,
                                        taken = ?reading.gear.taken,
                                        "gear"
                                    );
                                    // What it holds may have changed with it (LOOK.md 6.2).
                                    let look = look_of(&cfg.looks, &zone, &hub_slots, body);
                                    if look != before {
                                        info!(character, body, held = look.held, "look");
                                        for s in sessions.values() {
                                            s.send_control(FromZone::Look { id: body, look });
                                        }
                                    }
                                }
                            }
                            None => {
                                // No body at the moment: kept for the one a join in
                                // flight is about to make.
                                gear_kept.retain(|_, (_, at)| at.elapsed() < GEAR_KEPT);
                                let newer = gear_kept
                                    .get(&character)
                                    .is_none_or(|(known, _)| reading.seq > known.seq);
                                if newer {
                                    gear_kept.insert(character, (reading.clone(), Instant::now()));
                                }
                            }
                        }
                    }
                    if tell
                        && let Some(s) = sessions.get_mut(&id)
                        && s.end_gear_request()
                    {
                        s.send_control(FromZone::WearResult {
                            item,
                            result: result.map(|_| ()),
                        });
                    }
                }
                ClientEvent::Party { id, ask } => {
                    let Some(session) = sessions.get_mut(&id) else {
                        continue;
                    };
                    if !session.begin_party_request() {
                        zone_line(session, "one thing at a time: try again in a moment");
                        continue;
                    }
                    // The zone's own part (PARTY.md 4): the asker is a body in person,
                    // and a name is one a character can have. The rest is the hub's.
                    let named = |name: &str| gm_hub_proto::names::character_name(name).is_ok();
                    let request = (|| {
                        let link = cfg.hub.clone().ok_or("this zone has no parties")?;
                        let character = hub_slots
                            .get(&id)
                            .ok_or("this zone has no parties")?
                            .character;
                        if zone.player(id).is_none_or(|p| p.ghost) {
                            return Err("not now");
                        }
                        let op = match &ask {
                            PartyAsk::Invite(name) | PartyAsk::Remove(name) if !named(name) => {
                                return Err("nobody can be called that");
                            }
                            PartyAsk::Answer { from, .. } if !named(from) => {
                                return Err("nobody can be called that");
                            }
                            PartyAsk::Invite(name) => ZonePartyOp::Invite {
                                from: character,
                                to: name.clone(),
                            },
                            PartyAsk::Answer { from, join } => ZonePartyOp::Answer {
                                character,
                                from: from.clone(),
                                join: *join,
                            },
                            PartyAsk::Leave => ZonePartyOp::Leave { character },
                            PartyAsk::Remove(name) => ZonePartyOp::Remove {
                                leader: character,
                                name: name.clone(),
                            },
                        };
                        Ok((link, op))
                    })();
                    match request {
                        Ok((link, op)) => {
                            let tx = event_tx.clone();
                            tokio::spawn(async move {
                                let answered = |result, tell| ClientEvent::PartyAnswered {
                                    id,
                                    ask: ask.clone(),
                                    result,
                                    tell,
                                };
                                let asked = link.party(op);
                                tokio::pin!(asked);
                                match tokio::time::timeout(HUB_PATIENCE, &mut asked).await {
                                    Ok(result) => {
                                        let _ = tx.send(answered(result, true)).await;
                                    }
                                    Err(_) => {
                                        // As with gear: the player is told and may ask
                                        // again, and what the hub did in the end is
                                        // still what the party is (this zone is not
                                        // told of its own change in any other way).
                                        let silent = Err(HUB_SILENT.to_string());
                                        let _ = tx.send(answered(silent, true)).await;
                                        let late = asked.await;
                                        if late.is_ok() {
                                            let _ = tx.send(answered(late, false)).await;
                                        }
                                    }
                                }
                            });
                        }
                        Err(why) => {
                            session.cancel_party_request();
                            zone_line(session, why);
                        }
                    }
                }
                ClientEvent::PartyAnswered {
                    id,
                    ask,
                    result,
                    tell,
                } => {
                    if let Ok(PartyReply::News(news)) = &result {
                        party_news(&mut parties, news, &hub_slots, &sessions);
                    }
                    if tell
                        && let Some(s) = sessions.get_mut(&id)
                        && s.end_party_request()
                    {
                        let line = match (ask, result) {
                            (_, Err(why)) => why,
                            (_, Ok(PartyReply::Invited { name })) => {
                                format!("{name} was asked to join")
                            }
                            (_, Ok(PartyReply::Declined)) => "declined".to_string(),
                            (PartyAsk::Remove(name), Ok(_)) => {
                                format!("{name} is out of the party")
                            }
                            // The news said the rest to everybody it concerns.
                            (_, Ok(_)) => continue,
                        };
                        zone_line(s, line);
                    }
                }
                ClientEvent::HubParty(news) => {
                    party_news(&mut parties, &news, &hub_slots, &sessions);
                }
                ClientEvent::PartySeq { character, seq } => {
                    // A save's answer: the zone is behind (a notice was lost) when the
                    // hub's number is larger than its own. It asks, once.
                    let Some(link) = cfg.hub.clone() else {
                        continue;
                    };
                    if parties.behind(character, seq) {
                        let tx = event_tx.clone();
                        tokio::spawn(async move {
                            let op = ZonePartyOp::Read { character };
                            if let Ok(PartyReply::Reading(reading)) = link.party(op).await {
                                let _ =
                                    tx.send(ClientEvent::PartyRead { character, reading }).await;
                            }
                        });
                    }
                }
                ClientEvent::PartyRead { character, reading } => {
                    if parties.read(character, reading)
                        && let Some(s) = session_of(&hub_slots, &sessions, character)
                    {
                        warn!(character, "a party's news had been missed: mended");
                        s.send_control(FromZone::Party(parties.names(character)));
                    }
                }
                ClientEvent::HubInvited { to, from } => {
                    match session_of(&hub_slots, &sessions, to) {
                        Some(s) => {
                            s.send_control(FromZone::Invited { from });
                        }
                        // On its way here: kept for the body (a minute, as the
                        // invitation itself; a few hundred at most).
                        None => {
                            invites_kept.retain(|(_, _, at)| at.elapsed() < INVITE_KEPT);
                            if invites_kept.len() < 512 {
                                invites_kept.push((to, from, Instant::now()));
                            }
                        }
                    }
                }
                ClientEvent::HubDeclined { to, by } => {
                    if let Some(s) = session_of(&hub_slots, &sessions, to) {
                        zone_line(s, format!("{by} declined"));
                    }
                }
                ClientEvent::HubHeard {
                    to,
                    channel,
                    from,
                    text,
                } => {
                    for character in to {
                        if let Some(s) = session_of(&hub_slots, &sessions, character) {
                            // A line may be lost; nobody is disconnected over one.
                            s.send_droppable(FromZone::Heard {
                                channel,
                                from: from.clone(),
                                text: text.clone(),
                            });
                        }
                    }
                }
                ClientEvent::Say { id, to, text } => {
                    // The connection checked the line and its sender's rate (`net.rs`).
                    // A party's line and a whisper go through the hub, whoever hears
                    // them (PARTY.md 5).
                    let Some(session) = sessions.get(&id) else {
                        continue;
                    };
                    let (Some(link), Some(slot)) = (cfg.hub.clone(), hub_slots.get(&id)) else {
                        session.send_droppable(FromZone::ChatFrom {
                            from: 0,
                            text: "this zone has no parties".into(),
                        });
                        continue;
                    };
                    let (tx, from) = (event_tx.clone(), slot.character);
                    tokio::spawn(async move {
                        let whisper = matches!(to, SayTo::Whisper(_));
                        let op = ZonePartyOp::Say {
                            from,
                            to,
                            text: text.clone(),
                        };
                        let result = match tokio::time::timeout(HUB_PATIENCE, link.party(op)).await
                        {
                            Ok(Ok(PartyReply::Said { to })) => Ok(to),
                            Ok(Ok(_)) => Err("the hub could not be asked: try again".to_string()),
                            Ok(Err(why)) => Err(why),
                            Err(_) => Err(HUB_SILENT.to_string()),
                        };
                        let said = ClientEvent::Said {
                            id,
                            whisper,
                            text,
                            result,
                        };
                        let _ = tx.send(said).await;
                    });
                }
                ClientEvent::Said {
                    id,
                    whisper,
                    text,
                    result,
                } => {
                    let Some(s) = sessions.get(&id) else {
                        continue;
                    };
                    match result {
                        // A whisper is answered to the whisperer as it went out; a
                        // party's line comes back with everybody else's.
                        Ok(to) if whisper => {
                            report.chat_lines += 1;
                            s.send_droppable(FromZone::Heard {
                                channel: control::CHANNEL_WHISPERED,
                                from: to,
                                text,
                            });
                        }
                        Ok(_) => report.chat_lines += 1,
                        Err(why) => {
                            s.send_droppable(FromZone::ChatFrom { from: 0, text: why });
                        }
                    }
                }
                ClientEvent::TradeAsk { id, with } => {
                    let other_here = sessions.contains_key(&with);
                    let Some(session) = sessions.get_mut(&id) else {
                        continue;
                    };
                    if !session.begin_party_request() {
                        zone_line(session, "one thing at a time: try again in a moment");
                        continue;
                    }
                    let now = Instant::now();
                    let lapse = Duration::from_secs(control::TRADE_ASK_SECS);
                    trade_asks.retain(|_, (_, at)| now.duration_since(*at) < lapse);
                    // Only the zone knows that two bodies stand together, alive and in
                    // person (PARTY.md 6). The rest is the hub's to check.
                    let request = (|| {
                        let link = cfg.hub.clone().ok_or("this zone has no market")?;
                        let mine = hub_slots
                            .get(&id)
                            .ok_or("this zone has no market")?
                            .character;
                        if with == id {
                            return Err("that is you");
                        }
                        let theirs = hub_slots
                            .get(&with)
                            .filter(|_| other_here)
                            .ok_or("there is nobody there to trade with")?
                            .character;
                        let feet = |body: EntityId| {
                            zone.player(body)
                                .filter(|p| p.alive && !p.ghost)
                                .map(|p| p.mover.mv.origin + Vec3::Z * Hull::Player.mins().z)
                        };
                        let (Some(a), Some(b)) = (feet(id), feet(with)) else {
                            return Err("not now");
                        };
                        if !trade_in_reach(a.into(), b.into()) {
                            return Err("walk up to them to trade");
                        }
                        Ok((link, mine, theirs))
                    })();
                    let (link, mine, theirs) = match request {
                        Ok(r) => r,
                        Err(why) => {
                            session.cancel_party_request();
                            zone_line(session, why);
                            continue;
                        }
                    };
                    if trade_asks.get(&with).is_some_and(|(whom, _)| *whom == id) {
                        // Both asked: the hub opens the trade.
                        trade_asks.remove(&with);
                        trade_asks.remove(&id);
                        // (The pair may ask each other again once this is open.)
                        trade_told.remove(&(id, with));
                        trade_told.remove(&(with, id));
                        let tx = event_tx.clone();
                        let named = |body: EntityId| {
                            sessions
                                .get(&body)
                                .map_or_else(String::new, |s| s.name.clone())
                        };
                        let names = (named(with), named(id));
                        tokio::spawn(async move {
                            let opened = |result, tell| ClientEvent::TradeOpened {
                                a: with,
                                b: id,
                                names: names.clone(),
                                result,
                                tell,
                            };
                            let asked = link.trade_open(theirs, mine);
                            tokio::pin!(asked);
                            match tokio::time::timeout(HUB_PATIENCE, &mut asked).await {
                                Ok(result) => {
                                    let _ = tx.send(opened(result, true)).await;
                                }
                                Err(_) => {
                                    // Told, and a trade the hub opened in the end is
                                    // still theirs: its window comes up late (and the
                                    // asker's gate, freed by the first answer, is not
                                    // freed again by the second).
                                    let silent = Err(HUB_SILENT.to_string());
                                    let _ = tx.send(opened(silent, true)).await;
                                    if let Ok(trade) = asked.await {
                                        let _ = tx.send(opened(Ok(trade), false)).await;
                                    }
                                }
                            }
                        });
                        continue;
                    }
                    // The first to ask: the other is told, once for as long as the
                    // request waits. (The gate keeps its second: this reached somebody.)
                    session.end_party_request();
                    let name = |body: EntityId| {
                        sessions
                            .get(&body)
                            .map_or_else(String::new, |s| s.name.clone())
                    };
                    trade_asks.insert(id, (with, now));
                    if let Some(s) = sessions.get(&id) {
                        zone_line(s, format!("you asked {} to trade", name(with)));
                    }
                    // The other is told once per pair in the time a request waits:
                    // asking A, then B, then A again tells nobody twice.
                    trade_told.retain(|_, at| now.duration_since(*at) < lapse);
                    if !trade_told.contains_key(&(id, with))
                        && let Some(other) = sessions.get(&with)
                    {
                        trade_told.insert((id, with), now);
                        other.send_control(FromZone::TradeAsked { from: id });
                    }
                }
                ClientEvent::TradeOpened {
                    a,
                    b,
                    names,
                    result,
                    tell,
                } => {
                    if tell && let Some(s) = sessions.get_mut(&b) {
                        s.end_party_request();
                    }
                    match result {
                        Ok(trade) => {
                            // (Whoever is still here is told, with the other's name as it
                            // was: a trade the hub holds open must be seen to be closed.)
                            for (to, with) in [(a, names.1), (b, names.0)] {
                                if let Some(s) = sessions.get(&to) {
                                    s.send_control(FromZone::TradeOpened { trade, with });
                                }
                            }
                        }
                        Err(why) => {
                            for to in [a, b] {
                                if let Some(s) = sessions.get(&to) {
                                    zone_line(s, format!("the trade did not open: {why}"));
                                }
                            }
                        }
                    }
                }
                ClientEvent::Input { id, datagram } => {
                    if let Some(s) = sessions.get_mut(&id) {
                        s.last_input_at = scheduler.tick();
                        s.had_input = true;
                        s.on_ack(datagram.ack_tick);
                        for (t, frame) in datagram.frames() {
                            zone.queue_input(id, t, frame.to_sim(), datagram.view_tick);
                        }
                    }
                }
                ClientEvent::Malformed { id } => {
                    if let Some(s) = sessions.get_mut(&id) {
                        s.malformed += 1;
                        report.malformed += 1;
                        if s.malformed > 100 {
                            s.conn.close(2, b"malformed datagrams");
                        }
                    }
                }
                ClientEvent::Chat { id, text } => {
                    // The connection checked the line and its sender's rate (`net.rs`);
                    // here is the zone's own ceiling: everybody hears everybody, and a
                    // crowd of talkers must not become everybody's traffic.
                    if !sessions.contains_key(&id) {
                        continue;
                    }
                    let now = Instant::now();
                    let back = now.duration_since(chat_at).as_secs_f32() * ZONE_CHAT_PER_SEC;
                    chat_lines = (chat_lines + back).min(ZONE_CHAT_BURST);
                    chat_at = now;
                    if chat_lines < 1.0 {
                        report.chat_dropped += 1;
                        // Its sender is told (their own bucket bounds how often).
                        if let Some(s) = sessions.get(&id) {
                            s.send_droppable(FromZone::ChatFrom {
                                from: 0,
                                text: "too many are talking: that line was not heard".into(),
                            });
                        }
                        continue;
                    }
                    chat_lines -= 1.0;
                    report.chat_lines += 1;
                    for s in sessions.values() {
                        // A chat line may be lost; nobody is disconnected over one.
                        s.send_droppable(FromZone::ChatFrom {
                            from: id,
                            text: text.clone(),
                        });
                    }
                }
                ClientEvent::Report { id, target, reason } => {
                    let Some(s) = sessions.get_mut(&id) else {
                        continue;
                    };
                    let refuse = |s: &Session, why: &str| {
                        s.send_control(FromZone::ReportResult(Err(why.to_string())));
                    };
                    let Some(rec) = &mut recorder else {
                        refuse(s, "this zone keeps no replays");
                        continue;
                    };
                    // A client's body here now, or one that left within two minutes.
                    recent_left.retain(|(_, _, at)| at.elapsed() < REPORTABLE_AFTER_LEAVE);
                    let target_character = rec
                        .entry(target)
                        .filter(|e| e.human() && e.id != id)
                        .map(|e| e.character)
                        .or_else(|| {
                            recent_left
                                .iter()
                                .find(|(body, _, _)| *body == target && *body != id)
                                .map(|(_, character, _)| *character)
                        });
                    if target_character.is_none() {
                        refuse(s, "nobody to report by that id");
                        continue;
                    }
                    if !s.may_report() {
                        refuse(s, "one report in thirty seconds");
                        continue;
                    }
                    if !rec.may_report() {
                        refuse(s, "too many reports in this zone this hour");
                        continue;
                    }
                    match (&cfg.hub, target_character, hub_slots.get(&id)) {
                        (None, Some(_), _) => {
                            rec.report(zone.tick, None);
                            info!(reporter = id, target, reason = reason.name(), "report");
                            s.send_control(FromZone::ReportResult(Ok(())));
                        }
                        // The hub opens the report first (its limits are the hub's); the
                        // replay is written when it has.
                        (Some(link), Some(target_character), Some(slot)) => {
                            let (link, reporter, tx) =
                                (link.clone(), slot.character, event_tx.clone());
                            tokio::spawn(async move {
                                let result = link.report(reporter, target_character, reason).await;
                                let _ = tx.send(ClientEvent::ReportOpened { id, result }).await;
                            });
                        }
                        _ => refuse(s, "nobody to report by that id"),
                    }
                }
                ClientEvent::ReportOpened { id, result } => {
                    let answer = match (result, &mut recorder) {
                        (Ok(report), Some(rec)) => {
                            rec.report(zone.tick, Some(report));
                            info!(reporter = id, report, "report opened");
                            Ok(())
                        }
                        (Ok(_), None) => Err("this zone keeps no replays".to_string()),
                        (Err(why), _) => Err(why),
                    };
                    if let Some(s) = sessions.get(&id) {
                        s.send_control(FromZone::ReportResult(answer));
                    }
                }
                ClientEvent::Leave { id } => {
                    let leaving_character = hub_slots.get(&id).map(|s| s.character);
                    // A ghost's connection ends with its `Bye` on the way to another zone: the
                    // body and the hub's transit stay until the claim or the timeout (HUB.md 3.3).
                    if hub_slots.get(&id).is_some_and(|s| s.ghost_since.is_some()) {
                        if let Some(s) = sessions.remove(&id) {
                            report.oversize_drops += s.oversize_drops;
                            report.send_failures += s.send_failures;
                        }
                        // The body is still here: it leaves at the claim or the timeout.
                        continue;
                    }
                    if let (Some(link), Some(slot)) = (&cfg.hub, hub_slots.remove(&id))
                        && !slot.claimed_elsewhere
                        && let Some(state) = character_state(&zone, link, id, &slot)
                    {
                        let (link, tx) = (link.clone(), event_tx.clone());
                        let character = slot.character;
                        leaving_saves.insert(character);
                        tokio::spawn(async move {
                            let _ = link.save(character, state, true).await;
                            let _ = tx.send(ClientEvent::LeftSaved { character }).await;
                        });
                    }
                    if let Some(character) = leaving_character {
                        parties.left(character, Instant::now());
                        if director.engaged(id) {
                            left_fights.insert(character, id);
                        }
                    }
                    party_waits.remove(&id);
                    trade_asks.retain(|asker, (asked, _)| *asker != id && *asked != id);
                    director.human_left(&mut zone, id);
                    let body = zone.remove_player(id);
                    if body.is_some() {
                        depart(
                            &mut recorder,
                            cfg.hub.as_ref(),
                            &mut recent_left,
                            id,
                            leaving_character,
                        );
                    }
                    if let Some(p) = &body {
                        info!(
                            entity = id,
                            executed = p.executed_frames,
                            starved = p.starved_ticks,
                            dropped = p.dropped_frames,
                            queued = p.queued_frames(),
                            kills = p.kills,
                            deaths = p.deaths,
                            "player stats at leave"
                        );
                        report.executed_frames += p.executed_frames;
                        report.starved_ticks += p.starved_ticks;
                        report.dropped_frames += p.dropped_frames;
                    }
                    let session = sessions.remove(&id);
                    if let Some(s) = &session {
                        report.oversize_drops += s.oversize_drops;
                        report.send_failures += s.send_failures;
                        let st = s.conn.stats();
                        info!(
                            entity = id,
                            name = %s.name,
                            web = s.conn.is_web(),
                            damage_dealt = s.damage_dealt,
                            damage_taken = s.damage_taken,
                            hits_landed = s.hits_landed,
                            udp_tx_bytes = st.udp_tx.bytes,
                            udp_rx_bytes = st.udp_rx.bytes,
                            rtt_ms = format_args!("{:.1}", s.conn.rtt().as_secs_f64() * 1000.0),
                            "session at leave"
                        );
                    }
                    // Claimed by another zone a moment ago: that said and counted it.
                    if body.is_none() && session.is_none() {
                        continue;
                    }
                    for s in sessions.values() {
                        s.send_control(FromZone::PlayerLeft(id));
                    }
                    report.leaves += 1;
                }
            }
        }

        // The party a body fights under (PARTY.md 2): made what the hub says it is, but
        // never while the body is on the ledger of an engaged encounter. Rules are
        // immutable in combat: somebody removed in the middle of a boss fight is paid as
        // a member of the party it fought in, and no ledger sees a body change sides.
        // And a fight's roster is closed: nobody takes the number of a party that is in
        // one, alive in the place of its dead or as one more than it had.
        for (&id, slot) in &hub_slots {
            let want = parties.number(slot.character, id);
            if zone.player(id).is_none_or(|p| p.party == want) {
                party_waits.remove(&id);
                continue;
            }
            let wait = if director.engaged(id) {
                Some("the party changes for you when this fight is over")
            } else if want != id && director.party_engaged(want) {
                Some("your party is in a fight: you are of it when the fight is over")
            } else {
                None
            };
            match wait {
                Some(why) => {
                    if party_waits.insert(id)
                        && let Some(s) = sessions.get(&id)
                    {
                        zone_line(s, why);
                    }
                }
                None => {
                    director.set_party(&mut zone, id, want);
                    // Somebody who was told to wait is told when the wait is over.
                    if party_waits.remove(&id)
                        && let Some(s) = sessions.get(&id)
                    {
                        zone_line(
                            s,
                            if want == id {
                                "the fight is over: you are on your own now"
                            } else {
                                "the fight is over: you are of the party now"
                            },
                        );
                    }
                }
            }
        }

        // Once a second: a client that has sent no input for `INPUT_IDLE_SECS` leaves (one
        // that has not sent its first yet has `FIRST_INPUT_SECS`: it may be loading the
        // map). A ghost is waiting for another zone's claim and owes no input.
        if scheduler.tick().is_multiple_of(rate.hz() as u64) {
            let hz = rate.hz() as u64;
            for (id, s) in &sessions {
                let secs = if s.had_input {
                    crate::session::INPUT_IDLE_SECS
                } else {
                    crate::session::FIRST_INPUT_SECS
                };
                let ghost = hub_slots.get(id).is_some_and(|h| h.ghost_since.is_some());
                let idle = scheduler.tick().saturating_sub(s.last_input_at);
                if !ghost && (secs * hz..(secs + 1) * hz).contains(&idle) {
                    info!(entity = *id, name = %s.name, "kicked: no input");
                    s.kick(&format!("no input for {secs} seconds"), 5);
                }
            }
        }

        // Hub bookkeeping once a second: periodic saves (staggered), ghost timeouts.
        if let Some(link) = &cfg.hub
            && scheduler.tick().is_multiple_of(rate.hz() as u64)
        {
            let now = Instant::now();
            let mut expired_ghosts: Vec<EntityId> = Vec::new();
            for (&id, slot) in hub_slots.iter_mut() {
                if let Some(since) = slot.ghost_since
                    && now.duration_since(since) >= GHOST_TIMEOUT
                {
                    slot.ghost_since = None;
                    match sessions.get(&id) {
                        Some(s) => {
                            // The other zone never claimed: the player plays on here. The
                            // hub still has the character on its way, and learns that it
                            // is here again from a save: at once (the block below), not in
                            // up to half a minute.
                            slot.last_save = now - SAVE_EVERY;
                            zone.set_ghost(id, false);
                            s.send_control(FromZone::TravelRefused(
                                "the other zone never claimed you".into(),
                            ));
                        }
                        // The client is gone too: the body leaves and the character goes
                        // offline (the hub accepts the origin's leaving save of a transit).
                        None => expired_ghosts.push(id),
                    }
                }
                if slot.ghost_since.is_none()
                    && now.duration_since(slot.last_save) >= SAVE_EVERY
                    && let Some(state) = character_state(&zone, link, id, slot)
                {
                    slot.last_save = now;
                    let link = link.clone();
                    let character = slot.character;
                    let tx = event_tx.clone();
                    tokio::spawn(async move {
                        let said = match link.save(character, state, false).await {
                            // What the hub holds of its party, by number: the zone looks
                            // whether it is behind (PARTY.md 3.2).
                            Ok(seq) => ClientEvent::PartySeq { character, seq },
                            Err(()) => ClientEvent::SaveRefused { character },
                        };
                        let _ = tx.send(said).await;
                    });
                }
            }
            for id in expired_ghosts {
                let slot = hub_slots.remove(&id);
                if let Some(slot) = &slot {
                    parties.left(slot.character, Instant::now());
                    if director.engaged(id) {
                        left_fights.insert(slot.character, id);
                    }
                }
                party_waits.remove(&id);
                trade_asks.retain(|asker, (asked, _)| *asker != id && *asked != id);
                depart(
                    &mut recorder,
                    Some(link),
                    &mut recent_left,
                    id,
                    slot.as_ref().map(|s| s.character),
                );
                if let Some(slot) = slot
                    && let Some(state) = character_state(&zone, link, id, &slot)
                {
                    let (link, tx) = (link.clone(), event_tx.clone());
                    let character = slot.character;
                    leaving_saves.insert(character);
                    tokio::spawn(async move {
                        let _ = link.save(character, state, true).await;
                        let _ = tx.send(ClientEvent::LeftSaved { character }).await;
                    });
                }
                director.human_left(&mut zone, id);
                zone.remove_player(id);
                for s in sessions.values() {
                    s.send_control(FromZone::PlayerLeft(id));
                }
                report.leaves += 1;
            }
        }

        // Hires that ran out, or ended early: the companion leaves once it is not in the
        // middle of an encounter (COMPANIONS.md 3.3). Looked at once a second.
        if scheduler.tick().is_multiple_of(rate.hz() as u64) && !hire_expiry.is_empty() {
            let now = now_secs();
            let due: Vec<EntityId> = hire_expiry
                .iter()
                .filter(|(id, at)| **at <= now && !director.in_encounter(**id))
                .map(|(id, _)| *id)
                .collect();
            for id in due {
                hire_expiry.remove(&id);
                director.remove_companion(&mut zone, id);
            }
        }

        // Latency-bounded lag compensation (PROTOCOL.md 4), refreshed once a second.
        if scheduler.tick().is_multiple_of(rate.hz() as u64) {
            for s in sessions.values() {
                let half_rtt = s.conn.rtt() / 2;
                let ticks = half_rtt.as_secs_f64() / rate.period().as_secs_f64();
                zone.set_half_rtt_ticks(s.id, ticks.ceil() as u32);
            }
        }

        phases.events += t_events.elapsed();
        let t_minds = Instant::now();
        director.pre_step(&mut zone, &world.bsp);
        phases.minds += t_minds.elapsed();
        let t_sim = Instant::now();
        zone.step(&world.bsp);
        phases.sim += t_sim.elapsed();
        // The weapon switched to in the gun mode (MODES.md 3.7) is what the hand shows
        // (LOOK.md 6.2): everyone is told when it changes.
        for s in sessions.values() {
            let Some(p) = zone.player(s.id) else {
                continue;
            };
            if p.sheet.kit.mode != gm_core::vocab::Mode::Gun {
                continue;
            }
            let held = p.mover.held;
            if hands_seen.insert(s.id, held) != Some(held) {
                let look = look_of(&cfg.looks, &zone, &hub_slots, s.id);
                for other in sessions.values() {
                    other.send_control(FromZone::Look { id: s.id, look });
                }
            }
        }

        let events: Vec<ZoneEvent> = zone.events.drain(..).collect();
        let mut recorded: Vec<gm_replay::Event> = match &recorder {
            Some(_) => events.iter().filter_map(replay_event).collect(),
            None => Vec::new(),
        };
        let t_minds = Instant::now();
        director.post_step(&mut zone, &world.bsp, &events);
        phases.minds += t_minds.elapsed();
        // What the director did: rosters, squads, encounters, loot, trials.
        for ev in std::mem::take(&mut director.events) {
            match ev {
                DirectorEvent::Spawned { id, name, kind } => {
                    let team = zone.player(id).map_or(0, |p| p.team());
                    let model = companion_models.get(&id).map(|m| m.id);
                    let look = look_of(&cfg.looks, &zone, &hub_slots, id);
                    for s in sessions.values() {
                        s.send_control(FromZone::PlayerInfo {
                            id,
                            name: name.clone(),
                            team,
                            model,
                            kind: wire_kind(kind),
                            look,
                        });
                    }
                }
                DirectorEvent::Removed { id } => {
                    companion_models.remove(&id);
                    hire_expiry.remove(&id);
                    for s in sessions.values() {
                        s.send_control(FromZone::PlayerLeft(id));
                    }
                }
                DirectorEvent::Squad { commander } => {
                    if !squads_dirty.contains(&commander) {
                        squads_dirty.push(commander);
                    }
                }
                DirectorEvent::Encounter { name, state, tell } => {
                    let wire = match state {
                        EncounterState::Engaged => {
                            report.encounters_engaged += 1;
                            control::EncounterState::Engaged
                        }
                        EncounterState::Reset => {
                            report.encounters_reset += 1;
                            control::EncounterState::Reset
                        }
                        EncounterState::Cleared { secs } => {
                            report.encounters_cleared += 1;
                            report.last_clear_secs = secs;
                            control::EncounterState::Cleared { secs }
                        }
                    };
                    info!(encounter = %name, ?state, "encounter");
                    // A fight that has trials: they are for one player and a squad, and a
                    // party of people is told so when it begins, not after the kill.
                    let trials = state == EncounterState::Engaged
                        && zone.content.trials.iter().any(|t| {
                            t.map == world.name && t.encounter == name && t.max_humans == 1
                        });
                    for id in tell {
                        if let Some(s) = sessions.get(&id) {
                            s.send_control(FromZone::Encounter {
                                name: name.clone(),
                                state: wire,
                            });
                            let of_a_party = hub_slots
                                .get(&id)
                                .is_some_and(|slot| parties.of(slot.character).is_some());
                            if trials && of_a_party {
                                zone_line(
                                    s,
                                    "a party of people passes no trial: those are for one player and a squad",
                                );
                            }
                        }
                    }
                }
                DirectorEvent::Loot {
                    encounter,
                    kill,
                    grants,
                } => {
                    // The kill's reference: unique across restarts of this zone process.
                    let reference = ((started_unix & 0xffff_ffff) << 24) as i64 | kill as i64;
                    let mut items: Vec<(CharacterId, String)> = Vec::new();
                    let mut coins: Vec<(CharacterId, i64)> = Vec::new();
                    for g in &grants {
                        report.loot_items += g.items.len() as u64;
                        info!(encounter = %encounter, human = g.human, items = ?g.items, coin = g.coin, "loot");
                        if let Some(s) = sessions.get(&g.human) {
                            s.send_control(FromZone::Loot {
                                encounter: encounter.clone(),
                                items: g.items.clone(),
                                coin: g.coin,
                            });
                        }
                        if let Some(slot) = hub_slots.get(&g.human) {
                            items.extend(g.items.iter().map(|m| (slot.character, m.clone())));
                            if g.coin > 0 {
                                coins.push((slot.character, g.coin as i64));
                            }
                        }
                    }
                    // One report per kill, repeated until the hub answers; the hub pays a
                    // reference once (ECONOMY.md 9).
                    if let Some(link) = cfg.hub.clone()
                        && (!items.is_empty() || !coins.is_empty())
                    {
                        tokio::spawn(async move {
                            match link.grant_kill(reference, items, coins).await {
                                Ok(true) => {}
                                Ok(false) => info!(reference, "the kill was already paid"),
                                Err(e) => warn!(reference, "the drop of a kill is lost: {e}"),
                            }
                        });
                    }
                }
                DirectorEvent::Trial {
                    human,
                    key,
                    name,
                    verdict,
                    standing,
                    secs,
                } => {
                    let passed = verdict.is_ok();
                    info!(human, trial = %key, passed, %standing, "trial");
                    if passed {
                        report.trials_passed += 1;
                    }
                    if let Some(s) = sessions.get(&human) {
                        s.send_control(FromZone::Trial {
                            key: key.clone(),
                            name,
                            passed,
                            // Why not; or, for a pass, what the ledger said.
                            detail: verdict
                                .err()
                                .map_or_else(|| standing.to_string(), |f| f.to_string()),
                            secs,
                        });
                    }
                    if passed
                        && let (Some(link), Some(slot)) = (cfg.hub.clone(), hub_slots.get(&human))
                    {
                        let character = slot.character;
                        tokio::spawn(async move {
                            if let Err(e) = link.trial(character, key, secs).await {
                                warn!(character, "recording a trial: {e}");
                            }
                        });
                    }
                }
            }
        }
        // Squads that changed: their commanders are told, once, what the squad is now.
        for commander in squads_dirty.drain(..) {
            let entries = squad_entries(&director, &zone, commander);
            if let Some(s) = sessions.get_mut(&commander)
                && s.squad_told != entries
            {
                s.squad_told = entries.clone();
                s.send_control(FromZone::Squad(entries));
            }
        }
        for ev in events {
            match ev {
                ZoneEvent::Hit {
                    kind,
                    attacker,
                    target,
                    amount,
                    absorbed,
                    at,
                } => {
                    match kind {
                        HitKind::Melee => report.hits_melee += 1,
                        HitKind::Projectile => report.hits_projectile += 1,
                        HitKind::Area => report.hits_area += 1,
                        HitKind::Dot => report.hits_dot += 1,
                    }
                    let amount = amount.max(0) as u64;
                    if attacker != target
                        && let Some(s) = sessions.get_mut(&attacker)
                    {
                        s.damage_dealt += amount;
                        s.hits_landed += 1;
                        // The attacker is told what it did: the number over the body
                        // it hit (LOOK.md 13.8).
                        s.send_control(FromZone::Hit {
                            target,
                            amount: amount.min(u16::MAX as u64) as u16,
                            absorbed: absorbed.clamp(0, u16::MAX as i32) as u16,
                            at: at.to_array(),
                        });
                    }
                    if let Some(s) = sessions.get_mut(&target) {
                        s.damage_taken += amount;
                    }
                }
                // A bullet's mark (MODES.md 10.2): everyone in the zone may see the wall.
                ZoneEvent::Impact { at, normal } => {
                    for s in sessions.values() {
                        s.send_control(FromZone::Impact {
                            at: at.to_array(),
                            normal: normal.to_array(),
                        });
                    }
                }
                ZoneEvent::Killed { victim, killer } => {
                    report.kills += 1;
                    let team = zone.player(killer).map_or(0, |p| p.team()) as usize;
                    report.team_kills[team.min(2)] += 1;
                    for s in sessions.values() {
                        s.send_control(FromZone::Killed { victim, killer });
                    }
                }
                ZoneEvent::Respawned(id) => {
                    // The client's prediction switches to whatever build the respawn applied.
                    let mut wears = None;
                    if let (Some(s), Some(p)) = (sessions.get_mut(&id), zone.player(id)) {
                        s.send_control(FromZone::BuildApplied(p.sheet.build.clone()));
                        // A respec to another frame shows the mannequin; back, the model.
                        let now = s.wears(p.frame());
                        if now != s.announced {
                            s.announced = now;
                            wears = Some((s.name.clone(), p.team(), now));
                        }
                    }
                    // A respec may hold another weapon (the primary's prop, LOOK.md 6.1):
                    // everyone is told either way, in one message or the other.
                    let look = look_of(&cfg.looks, &zone, &hub_slots, id);
                    if let Some((name, team, model)) = wears {
                        for s in sessions.values() {
                            s.send_control(FromZone::PlayerInfo {
                                id,
                                name: name.clone(),
                                team,
                                model,
                                kind: BodyKind::Human,
                                look,
                            });
                        }
                    } else {
                        for s in sessions.values() {
                            s.send_control(FromZone::Look { id, look });
                        }
                    }
                }
                ZoneEvent::Parried { .. } => report.parries += 1,
                ZoneEvent::GuardBroken(_) => report.guard_breaks += 1,
                ZoneEvent::Staggered(_) => report.staggers += 1,
                ZoneEvent::StatusApplied { .. } => report.statuses_applied += 1,
                // Rounds loaded and kits used (MODES.md 11.2, 11.3): the simulation spent
                // them; the hub's books follow, and its reading after is adopted as any.
                ZoneEvent::RoundsLoaded { id, hand, rounds } => {
                    let spent = zone.player(id).and_then(|p| {
                        let kit = &p.sheet.kit;
                        let slot = if hand == 0 {
                            kit.primary
                        } else {
                            kit.secondary
                        };
                        let f = kit.abilities[slot? as usize].firearm.as_ref()?;
                        Some(f.ammo.clone())
                    });
                    if let Some(template) = spent {
                        consume_stack(
                            &cfg,
                            &mut hub_slots,
                            &event_tx,
                            id,
                            |s| s.template == template,
                            rounds as u32,
                        );
                    }
                }
                ZoneEvent::ItemUsed { id, cell } => {
                    // One of the cell's stack (LOOK.md 3.2), by the bar the slot holds.
                    // Said in the log (MODES.md 11.3): a reload is not, an item is rarer
                    // and what a player asks about.
                    let Some(template) = hub_slots
                        .get(&id)
                        .and_then(|s| s.bar.get(cell as usize).cloned().flatten())
                    else {
                        warn!(
                            body = id,
                            cell, "item used from a cell the bar has nothing on"
                        );
                        continue;
                    };
                    let left = hub_slots.get(&id).map_or(0, |s| {
                        s.stacks
                            .iter()
                            .filter(|s| s.template == template)
                            .map(|s| s.quantity)
                            .sum::<u32>()
                    });
                    info!(
                        body = id,
                        character = hub_slots.get(&id).map(|s| s.character),
                        cell = cell + 1,
                        template,
                        left = left.saturating_sub(1),
                        "item used"
                    );
                    consume_stack(
                        &cfg,
                        &mut hub_slots,
                        &event_tx,
                        id,
                        |s| s.template == template,
                        1,
                    );
                }
                ZoneEvent::Healed {
                    target,
                    source,
                    amount,
                } => {
                    // The healer is told what its Regen gave another body: the green
                    // number over it (LOOK.md 13.8). The own healings are read from the
                    // own health, like the own hurts.
                    if source != target
                        && let Some(s) = sessions.get_mut(&source)
                    {
                        s.send_control(FromZone::Healed {
                            target,
                            amount: amount.clamp(0, u16::MAX as i32) as u16,
                        });
                    }
                }
                ZoneEvent::ProjectileSpawned { .. }
                | ZoneEvent::ProjectileRemoved(_)
                | ZoneEvent::AreaSpawned { .. }
                | ZoneEvent::AreaRemoved(_) => {}
            }
        }

        let tick = zone.tick;
        let t_snap = Instant::now();
        table.rebuild(&zone, &world, &|id| wire_kind(director.kind_of(id)));
        for s in sessions.values() {
            if let Some(leaf) = s.eye_leaf(&zone, &world) {
                pvs.prepare(&world.bsp, leaf);
            }
            // Squad sight: the rows of a commanding client's companions (COMPANIONS.md 5.2).
            if zone.player(s.id).is_some_and(crate::session::commanding) {
                for m in director.squad(s.id) {
                    if let Some(p) = zone.player(m.id).filter(|p| p.alive) {
                        pvs.prepare(&world.bsp, world.bsp.leaf_for_point(p.mover.eye()));
                    }
                }
            }
        }
        // Each session's snapshot only reads the zone, the table and the PVS rows; the
        // per-session history is its own. Spread them over the pool (rayon).
        let outgoing: Vec<(EntityId, Vec<u8>)> = {
            let zone = &zone;
            let world = &*world;
            let table = &table;
            let pvs = &pvs;
            let mut slots: Vec<&mut Session> = sessions.values_mut().collect();
            slots
                .par_iter_mut()
                .filter_map(|s| {
                    s.build_snapshot(zone, world, table, pvs, tick)
                        .map(|bytes| (s.id, bytes))
                })
                .collect()
        };
        phases.snapshot += t_snap.elapsed();
        let t_send = Instant::now();
        for (id, bytes) in outgoing {
            if let Some(s) = sessions.get_mut(&id)
                && s.conn.send_datagram(Bytes::from(bytes)).is_err()
            {
                s.send_failures += 1;
            }
        }
        phases.send += t_send.elapsed();

        // Record the tick: the frame, the reactions the zone can see, the aim analysis.
        if let Some(rec) = &mut recorder {
            let t_sight = Instant::now();
            sight.tick(
                &zone,
                &world.bsp,
                !cfg.wild,
                &|id| sessions.contains_key(&id),
                &mut recorded,
            );
            phases.sight += t_sight.elapsed();
            let t_record = Instant::now();
            let describe = |id: EntityId| -> Option<RosterEntry> {
                let p = zone.player(id)?;
                let (name, kind) = match sessions.get(&id) {
                    Some(s) => (s.name.clone(), BodyKind::Human),
                    None => director
                        .driven()
                        .into_iter()
                        .find(|(body, _, _)| *body == id)
                        .map(|(_, name, kind)| (name, wire_kind(kind)))?,
                };
                Some(RosterEntry {
                    id,
                    name,
                    team: p.team(),
                    kind,
                    party: p.party,
                    build: cfg
                        .content
                        .builds
                        .iter()
                        .find(|b| b.build == p.sheet.build)
                        .map_or_else(|| "custom".to_string(), |b| b.name.clone()),
                    character: hub_slots.get(&id).map_or(0, |h| h.character),
                    mode: p.sheet.build.mode as u8,
                })
            };
            // How far behind this tick each client's frames say they look.
            let view_lag = |id: EntityId| -> Option<u8> {
                sessions.contains_key(&id).then_some(())?;
                let p = zone.player(id)?;
                Some(tick.wrapping_sub(p.view_claimed).min(MAX_CLAIMED_VIEW_LAG) as u8)
            };
            let done = rec.tick(
                tick,
                &table,
                std::mem::take(&mut recorded),
                &describe,
                &view_lag,
            );
            phases.record += t_record.elapsed();
            for finished in done {
                let tx = written_tx.clone();
                // Compressing and writing a file is not the tick loop's work.
                writing.retain(|t| !t.is_finished());
                writing.push(tokio::task::spawn_blocking(move || {
                    match finished.write() {
                        Ok(w) => {
                            let _ = tx.send(w);
                        }
                        Err(e) => warn!("writing a replay: {e}"),
                    }
                }));
            }
        }
        while let Ok(w) = written_rx.try_recv() {
            replay_written(&w);
            if let Some(link) = &cfg.hub {
                let link = link.clone();
                tokio::spawn(async move { link.replay(w).await });
            }
        }
        phases.ticks += 1;

        metrics.record(
            started.saturating_duration_since(deadline),
            started.elapsed(),
        );
        scheduler.advance();

        if last_report.elapsed() >= cfg.report_every {
            let secs = last_report.elapsed().as_secs_f64();
            fill_report(
                &mut report,
                &mut sessions,
                &zone,
                &metrics,
                secs,
                scheduler.tick(),
            );
            phases.fill(&mut report);
            report.minds = director.minds();
            if let Some(rec) = &recorder {
                report.replay_frames = rec.stats.frames;
                report.replay_frame_bytes = rec.stats.frame_bytes;
                report.replays_written = rec.stats.fights_written + rec.stats.reports_written;
                report.replay_ring_bytes = rec.stats.ring_bytes;
            }
            phases = Phases::default();
            *hub_stats.lock().unwrap() = (report.players as u32, report.tick_mean_us as f32);
            info!(
                tick = report.tick,
                players = report.players,
                tx_bps = format_args!("{:.0}", report.tx_bytes_per_player_s),
                rx_bps = format_args!("{:.0}", report.rx_bytes_per_player_s),
                snap_bps = format_args!("{:.0}", report.snapshot_payload_per_player_s),
                tick_us_mean = format_args!("{:.1}", report.tick_mean_us),
                tick_us_p99 = format_args!("{:.1}", report.tick_p99_us),
                tick_us_max = format_args!("{:.1}", report.tick_max_us),
                late_us_max = format_args!("{:.1}", report.late_max_us),
                overruns = report.overruns,
                events_us = format_args!("{:.0}", report.events_us_mean),
                sim_us = format_args!("{:.0}", report.sim_us_mean),
                snapshot_us = format_args!("{:.0}", report.snapshot_us_mean),
                send_us = format_args!("{:.0}", report.send_us_mean),
                minds = report.minds,
                minds_us = format_args!("{:.0}", report.minds_us_mean),
                record_us = format_args!("{:.0}", report.record_us_mean),
                sight_us = format_args!("{:.0}", report.sight_us_mean),
                replay_ring_bytes = report.replay_ring_bytes,
                replay_frame_bytes = report.replay_frame_bytes,
                replays = report.replays_written,
                starved = report.starved_ticks + report.live_starved_ticks,
                executed = report.executed_frames + report.live_executed_frames,
                hits = report.hits_melee + report.hits_projectile + report.hits_area,
                kills = report.kills,
                "zone report"
            );
            if let Some(w) = &cfg.report_tx {
                let _ = w.send(report.clone());
            }
            metrics.reset_window();
            last_report = Instant::now();
        }
        if cfg.max_ticks.is_some_and(|n| scheduler.tick() >= n) {
            break;
        }
    }

    let secs = last_report.elapsed().as_secs_f64().max(1e-3);
    fill_report(
        &mut report,
        &mut sessions,
        &zone,
        &metrics,
        secs,
        scheduler.tick(),
    );
    phases.fill(&mut report);
    report.minds = director.minds();
    // Save everyone before the lights go out (HUB.md 3.2).
    if let Some(link) = &cfg.hub {
        for (&id, slot) in &hub_slots {
            if !slot.claimed_elsewhere
                && let Some(state) = character_state(&zone, link, id, slot)
            {
                let _ = link.save(slot.character, state, true).await;
            }
        }
    }
    // What the recorder still holds is written before the zone goes, and everybody still
    // here gets the line a leaver gets.
    // The hub gets one try at each: a zone that stops does not wait an hour for a hub that
    // is away, and what it could not send stays on disk for the next zone.
    if let Some(rec) = &mut recorder {
        // Files still being written when the loop ended.
        for task in writing.drain(..) {
            let _ = task.await;
        }
        let mut last: Vec<Written> = Vec::new();
        while let Ok(w) = written_rx.try_recv() {
            last.push(w);
        }
        for finished in rec.flush() {
            match finished.write() {
                Ok(w) => last.push(w),
                Err(e) => warn!("writing a replay: {e}"),
            }
        }
        // Sent side by side: each has its own five seconds, and the zone waits for the
        // slowest of them, not for their sum.
        let mut last_words = Vec::new();
        for w in last {
            replay_written(&w);
            if let Some(link) = &cfg.hub {
                let link = link.clone();
                last_words.push(tokio::spawn(async move { link.replay_once(w).await }));
            }
        }
        // Everybody still here, and the bodies waiting for another zone to claim them.
        let present: std::collections::BTreeSet<EntityId> =
            sessions.keys().chain(hub_slots.keys()).copied().collect();
        for id in present {
            let name = rec.entry(id).map_or_else(String::new, |e| e.name.clone());
            let aim = rec.analyser.take(id);
            info!(entity = id, name = %name, "aim at leave: {}", aim.line());
            if let (Some(link), Some(slot)) = (&cfg.hub, hub_slots.get(&id))
                && !aim.is_empty()
            {
                let (link, character) = (link.clone(), slot.character);
                last_words.push(tokio::spawn(
                    async move { link.aim_once(character, aim).await },
                ));
            }
        }
        for task in last_words {
            let _ = task.await;
        }
    }
    for s in sessions.values() {
        s.send_control(FromZone::Kick("zone stopped".into()));
        s.conn.close(0, b"zone stopped");
    }
    // Fold the live sessions' counters into the totals.
    for p in zone.players() {
        report.executed_frames += p.executed_frames;
        report.starved_ticks += p.starved_ticks;
        report.dropped_frames += p.dropped_frames;
    }
    for s in sessions.values() {
        report.oversize_drops += s.oversize_drops;
        report.send_failures += s.send_failures;
    }
    if let Some(w) = &cfg.report_tx {
        let _ = w.send(report.clone());
    }
    endpoint.close(0u32.into(), b"zone stopped");
    acceptor.abort();
    // The web listener closes with its task: the sessions were closed one by one above.
    if let Some(task) = web_acceptor {
        task.abort();
    }
    info!(ticks = report.tick, "zone stopped");
    Ok(report)
}

fn fill_report(
    report: &mut ZoneReport,
    sessions: &mut BTreeMap<EntityId, Session>,
    zone: &Zone,
    metrics: &TickMetrics,
    secs: f64,
    tick: u64,
) {
    let mut tx = 0u64;
    let mut rx = 0u64;
    let mut payload = 0u64;
    for s in sessions.values_mut() {
        let st = s.conn.stats();
        tx += st.udp_tx.bytes.saturating_sub(s.last_udp_tx);
        rx += st.udp_rx.bytes.saturating_sub(s.last_udp_rx);
        s.last_udp_tx = st.udp_tx.bytes;
        s.last_udp_rx = st.udp_rx.bytes;
        payload += s.snapshot_bytes;
        s.snapshot_bytes = 0;
    }
    let n = sessions.len().max(1) as f64;
    let secs = secs.max(1e-3);
    let sm = metrics.summary();
    report.tick = tick;
    report.players = sessions.len();
    report.window_secs = secs;
    report.tx_bytes_per_player_s = tx as f64 / n / secs;
    report.rx_bytes_per_player_s = rx as f64 / n / secs;
    report.snapshot_payload_per_player_s = payload as f64 / n / secs;
    if !sessions.is_empty() {
        report.max_tx_bytes_per_player_s = report
            .max_tx_bytes_per_player_s
            .max(report.tx_bytes_per_player_s);
        report.max_rx_bytes_per_player_s = report
            .max_rx_bytes_per_player_s
            .max(report.rx_bytes_per_player_s);
    }
    report.tick_mean_us = sm.mean_us;
    report.tick_p99_us = sm.p99_us;
    report.tick_max_us = sm.max_us;
    report.late_max_us = sm.late_max_us;
    report.overruns = sm.overruns;
    // Live players' counters (folded into the totals when they leave).
    report.live_executed_frames = zone.players().map(|p| p.executed_frames).sum();
    report.live_starved_ticks = zone.players().map(|p| p.starved_ticks).sum();
}

/// Where the ticks of a report window went.
#[derive(Default)]
struct Phases {
    ticks: u64,
    events: Duration,
    minds: Duration,
    sim: Duration,
    snapshot: Duration,
    send: Duration,
    record: Duration,
    sight: Duration,
}

impl Phases {
    fn fill(&self, report: &mut ZoneReport) {
        let n = self.ticks.max(1) as f64;
        report.events_us_mean = self.events.as_secs_f64() * 1e6 / n;
        report.sim_us_mean = self.sim.as_secs_f64() * 1e6 / n;
        report.snapshot_us_mean = self.snapshot.as_secs_f64() * 1e6 / n;
        report.send_us_mean = self.send.as_secs_f64() * 1e6 / n;
        report.minds_us_mean = self.minds.as_secs_f64() * 1e6 / n;
        report.record_us_mean = (self.record + self.sight).as_secs_f64() * 1e6 / n;
        report.sight_us_mean = self.sight.as_secs_f64() * 1e6 / n;
    }
}

/// A future that stays pending after it first resolved, so `select!` can poll it repeatedly.
trait FuseOnce: Future<Output = ()> + Sized {
    fn fuse_once(self) -> Fused<Self> {
        Fused { inner: Some(self) }
    }
}

impl<F: Future<Output = ()>> FuseOnce for F {}

struct Fused<F> {
    inner: Option<F>,
}

impl<F: Future<Output = ()> + Unpin> Future for Fused<F> {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        match &mut self.inner {
            Some(f) => match std::pin::Pin::new(f).poll(cx) {
                std::task::Poll::Ready(()) => {
                    self.inner = None;
                    std::task::Poll::Ready(())
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
            None => std::task::Poll::Pending,
        }
    }
}
