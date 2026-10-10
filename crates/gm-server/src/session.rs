//! Per-client state on the server: acked baselines, reconstructed snapshot history, PVS and
//! distance-band scheduling, delta encoding within the datagram budget (PROTOCOL.md 5).
//!
//! Everything that does not depend on the client is computed once per tick into a
//! [`TickTable`] (wire records, leaves, stealth radii) and shared by every session; a session
//! then does one PVS bit test and one record copy per entity.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use glam::Vec3;
use gm_bsp::Bsp;
use gm_core::sim::{Area, GuardState, Player, Projectile, Zone};
use gm_core::vocab::{ArchetypeFrame, EntityId, Status};
use gm_hub_proto::protocol::{ModelId, ModelRef};
use gm_net::MAX_DATAGRAM_PAYLOAD;
use gm_net::control::{BodyKind, FromZone};
use gm_net::quant;
use gm_net::snapshot::{EntityState, GunsWire, OwnState, Snapshot, SpawnInfo, StatusWire, flags};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::world::ZoneWorld;

/// Snapshots kept per client; acks older than this get a full snapshot.
pub const HISTORY: usize = 64;
pub const MAX_BASELINE_AGE: i32 = 60;
pub use gm_net::bands::{FULL_RATE_DIST, HALF_RATE_DIST, band_scheduled};
/// Bodies this close are always sent, PVS or not: they can block the client's movement and
/// its prediction must know about them (PROTOCOL.md 5).
pub const TOUCH_DIST: f32 = 128.0;

/// Decompressed PVS rows by leaf, shared by all sessions within a tick loop. Rows are filled
/// before the parallel snapshot pass (`prepare`) so the pass only reads.
#[derive(Default)]
pub struct PvsCache {
    rows: HashMap<usize, Vec<u8>>,
}

impl PvsCache {
    pub fn prepare(&mut self, bsp: &Bsp, leaf: usize) {
        self.rows
            .entry(leaf)
            .or_insert_with(|| bsp.decompress_pvs(leaf));
    }

    pub fn row(&self, leaf: usize) -> Option<&[u8]> {
        self.rows.get(&leaf).map(|r| r.as_slice())
    }
}

/// One entity as every session sees it this tick, plus what the visibility test needs.
#[derive(Clone, Copy, Debug)]
pub struct TableEntry {
    pub id: EntityId,
    pub origin: Vec3,
    /// Leaves of the origin and of the eye (or the origin again for things without eyes).
    pub leaf: usize,
    pub leaf_top: usize,
    /// The non-own wire record.
    pub state: EntityState,
    /// Stealth radius (MATRIX.md 8); `f32::INFINITY` when not stealthed.
    pub stealth: f32,
    pub is_player: bool,
    /// For bodies: health, party, whether it is a creature, and the commander of a
    /// companion (0 for anybody else). Health goes to the body's own party and, for
    /// creatures, to everyone (COMPANIONS.md 13).
    pub health: u16,
    pub party: u32,
    pub creature: bool,
    pub owner: EntityId,
}

/// The shared per-tick view of the zone, ascending by entity id.
#[derive(Default)]
pub struct TickTable {
    pub entries: Vec<TableEntry>,
}

impl TickTable {
    /// Rebuild from the zone after its step; one leaf lookup per point, once per tick.
    /// `kind` says who drives a body (the director knows).
    pub fn rebuild(&mut self, zone: &Zone, world: &ZoneWorld, kind: &dyn Fn(EntityId) -> BodyKind) {
        self.entries.clear();
        let bsp = &world.bsp;
        for p in zone.players() {
            let origin = p.mover.mv.origin;
            let kind = kind(p.id);
            self.entries.push(TableEntry {
                health: p.health.clamp(0, u16::MAX as i32) as u16,
                party: p.party,
                creature: matches!(kind, BodyKind::Creature { .. }),
                owner: match kind {
                    BodyKind::Companion { owner } => owner,
                    _ => 0,
                },
                id: p.id,
                origin,
                leaf: bsp.leaf_for_point(origin),
                leaf_top: bsp.leaf_for_point(p.mover.eye()),
                state: player_state(p, false),
                stealth: if p.mover.statuses.has(Status::Stealth) {
                    p.mover.statuses.magnitude(Status::Stealth)
                } else {
                    f32::INFINITY
                },
                is_player: true,
            });
        }
        for pr in zone.projectiles() {
            let leaf = bsp.leaf_for_point(pr.pos);
            self.entries.push(TableEntry {
                id: pr.id,
                origin: pr.pos,
                leaf,
                leaf_top: leaf,
                state: projectile_state(pr),
                stealth: f32::INFINITY,
                is_player: false,
                health: 0,
                party: 0,
                creature: false,
                owner: 0,
            });
        }
        for ar in zone.areas() {
            self.entries.push(TableEntry {
                id: ar.id,
                origin: ar.origin,
                leaf: bsp.leaf_for_point(ar.origin),
                leaf_top: bsp.leaf_for_point(ar.origin + Vec3::new(0.0, 0.0, 32.0)),
                state: area_state(ar),
                stealth: f32::INFINITY,
                is_player: false,
                health: 0,
                party: 0,
                creature: false,
                owner: 0,
            });
        }
        // Players, projectiles and areas each come out ascending; ids are monotonic across
        // kinds, so a merge would do, but a sort of a nearly sorted list is just as cheap.
        self.entries.sort_unstable_by_key(|e| e.id);
    }
}

pub struct Session {
    pub id: EntityId,
    pub name: String,
    /// A game master (GM.md 1): the hub's word at the claim, or the zone's own list.
    pub gm: bool,
    /// The avatar model the hub named at the claim, and the id last announced to clients
    /// (present only while the player's frame is the model's, MODELS.md 7).
    pub model: Option<ModelRef>,
    pub announced: Option<ModelId>,
    pub conn: gm_net::link::Link,
    /// Bounded: a client that stops reading its control stream is kicked, not buffered forever.
    pub control: mpsc::Sender<FromZone>,
    history: VecDeque<Snapshot>,
    pub acked: u32,
    pub snapshots_sent: u64,
    pub snapshot_bytes: u64,
    pub oversize_drops: u64,
    pub send_failures: u64,
    pub malformed: u32,
    pub last_udp_tx: u64,
    pub last_udp_rx: u64,
    stall_gate: RequestGate,
    gear_gate: RequestGate,
    party_gate: RequestGate,
    travel_gate: RequestGate,
    report_gate: RequestGate,
    /// Squad entries last told to this client, so an unchanged squad is not sent again.
    pub squad_told: Vec<gm_net::control::SquadEntry>,
    /// The zone tick at which the last input datagram arrived (or the join), and whether
    /// any has arrived yet.
    pub last_input_at: u64,
    pub had_input: bool,
    /// Damage this client's body dealt and took, and the hits it landed.
    pub damage_dealt: u64,
    pub damage_taken: u64,
    pub hits_landed: u32,
}

/// A client that sends no input for this long is kicked (WEB.md 3.3): a QUIC connection
/// stays alive on keep-alives alone, so a hidden browser tab or a frozen client would
/// otherwise stand in the world for good.
pub const INPUT_IDLE_SECS: u64 = 10;
/// A client that has joined may take this long to send its first input: a browser fetches
/// the zone's map after `Welcome` and predicts nothing until it has it.
pub const FIRST_INPUT_SECS: u64 = 60;
/// How long a kicked client has to read its `Kick` before the connection is closed.
pub const KICK_GRACE: Duration = Duration::from_millis(500);

/// A player's stall and travel requests go to the hub: one of a kind at a time, and at most
/// one in this long.
pub const STALL_REQUEST_GAP: Duration = Duration::from_secs(1);
pub const TRAVEL_REQUEST_GAP: Duration = Duration::from_secs(1);
/// A report writes a replay: one per client in this long (ANTICHEAT.md 5).
pub const REPORT_GAP: Duration = Duration::from_secs(30);

/// What a client may ask of the hub through the zone: one request at a time and at most one
/// per gap, so that a flood of messages costs the hub one request per gap and not one each.
#[derive(Default)]
pub struct RequestGate {
    busy: bool,
    last: Option<Instant>,
    /// What `last` was before the request that holds the gate.
    before: Option<Instant>,
}

impl RequestGate {
    /// `true` reserves the gate until `end`.
    pub fn begin(&mut self, now: Instant, gap: Duration) -> bool {
        if self.busy || self.last.is_some_and(|t| now.duration_since(t) < gap) {
            return false;
        }
        self.busy = true;
        self.before = self.last;
        self.last = Some(now);
        true
    }

    /// The answer is in. `false`: nothing was being waited for (it was answered already).
    pub fn end(&mut self) -> bool {
        std::mem::take(&mut self.busy)
    }

    /// The zone refused the request itself and the hub was never asked: it does not count
    /// against the next one (a person told to walk up to the stall may buy as soon as
    /// they have).
    pub fn cancel(&mut self) {
        if self.busy {
            self.busy = false;
            self.last = self.before;
        }
    }
}

impl Session {
    pub fn new(
        id: EntityId,
        name: String,
        conn: gm_net::link::Link,
        control: mpsc::Sender<FromZone>,
    ) -> Session {
        Session {
            id,
            name,
            gm: false,
            model: None,
            announced: None,
            conn,
            control,
            history: VecDeque::with_capacity(HISTORY),
            acked: 0,
            snapshots_sent: 0,
            snapshot_bytes: 0,
            oversize_drops: 0,
            send_failures: 0,
            malformed: 0,
            last_udp_tx: 0,
            last_udp_rx: 0,
            stall_gate: RequestGate::default(),
            gear_gate: RequestGate::default(),
            party_gate: RequestGate::default(),
            travel_gate: RequestGate::default(),
            report_gate: RequestGate::default(),
            squad_told: Vec::new(),
            last_input_at: 0,
            had_input: false,
            damage_dealt: 0,
            damage_taken: 0,
            hits_landed: 0,
        }
    }

    /// Tell the client why, then close: the close waits a moment, because a connection
    /// closed in the same instant can take the unread `Kick` with it (a browser discards
    /// what its stream had queued when the session errors).
    pub fn kick(&self, reason: &str, code: u32) {
        self.send_control(FromZone::Kick(reason.to_string()));
        let conn = self.conn.clone();
        let why = reason.as_bytes().to_vec();
        tokio::spawn(async move {
            tokio::time::sleep(KICK_GRACE).await;
            conn.close(code, &why);
        });
    }

    /// Whether the client is being sent `id` (it was in the last snapshot built for it):
    /// an `Attack` order may only name such a body (COMPANIONS.md 5.3).
    pub fn sees(&self, id: EntityId) -> bool {
        self.history.back().is_some_and(|s| s.find(id).is_some())
    }

    /// May this player make a stall request now? `true` reserves it: `end_stall_request`
    /// releases it when the answer is in.
    pub fn begin_stall_request(&mut self) -> bool {
        self.stall_gate.begin(Instant::now(), STALL_REQUEST_GAP)
    }

    pub fn end_stall_request(&mut self) -> bool {
        self.stall_gate.end()
    }

    pub fn cancel_stall_request(&mut self) {
        self.stall_gate.cancel();
    }

    /// The same for putting an item on or taking it off: a gate of its own, so that a
    /// buy and what follows it do not wait for each other.
    pub fn begin_gear_request(&mut self) -> bool {
        self.gear_gate.begin(Instant::now(), STALL_REQUEST_GAP)
    }

    pub fn end_gear_request(&mut self) -> bool {
        self.gear_gate.end()
    }

    pub fn cancel_gear_request(&mut self) {
        self.gear_gate.cancel();
    }

    /// And for what a player asks about its party, and for asking somebody to trade
    /// (PARTY.md 4): a third gate of the same size.
    pub fn begin_party_request(&mut self) -> bool {
        self.party_gate.begin(Instant::now(), STALL_REQUEST_GAP)
    }

    pub fn end_party_request(&mut self) -> bool {
        self.party_gate.end()
    }

    pub fn cancel_party_request(&mut self) {
        self.party_gate.cancel();
    }

    /// May this player report somebody now (one report in `REPORT_GAP`)?
    pub fn may_report(&mut self) -> bool {
        let ok = self.report_gate.begin(Instant::now(), REPORT_GAP);
        self.report_gate.end();
        ok
    }

    /// The same for `Travel`: a handoff is a hub transaction.
    pub fn begin_travel_request(&mut self) -> bool {
        self.travel_gate.begin(Instant::now(), TRAVEL_REQUEST_GAP)
    }

    pub fn end_travel_request(&mut self) {
        self.travel_gate.end();
    }

    /// The model id to announce for a player whose current frame is `frame`: a model is shown
    /// only on the frame it was ingested for.
    pub fn wears(&self, frame: ArchetypeFrame) -> Option<ModelId> {
        self.model
            .filter(|m| m.frame == frame_index(frame))
            .map(|m| m.id)
    }

    /// Queue a reliable message. A client whose queue is full is not draining its stream:
    /// it is disconnected, because a reliable message is never silently lost (a missed
    /// `PlayerInfo` or `ModelRevoked` would leave it drawing the wrong thing for good).
    pub fn send_control(&self, msg: FromZone) -> bool {
        match self.control.try_send(msg) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.conn.close(3, b"control stream not read");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Queue a message that may be lost: a chat line. A client whose queue is half full
    /// misses it and stays connected: the room that is left is for what must arrive.
    pub fn send_droppable(&self, msg: FromZone) -> bool {
        self.control.capacity() > self.control.max_capacity() / 2
            && self.control.try_send(msg).is_ok()
    }

    /// Record an ack; anything we did not send (or no longer hold) counts as no ack.
    pub fn on_ack(&mut self, ack: u32) {
        self.acked = if ack != 0 && self.history.iter().any(|s| s.server_tick == ack) {
            ack
        } else {
            0
        };
    }

    fn baseline_index(&self, tick: u32) -> Option<usize> {
        if self.acked == 0 {
            return None;
        }
        self.history
            .iter()
            .position(|s| s.server_tick == self.acked)
            .filter(|&i| tick.wrapping_sub(self.history[i].server_tick) as i32 <= MAX_BASELINE_AGE)
    }

    /// Build this tick's snapshot for the client: PVS filter, distance bands, carry-forward of
    /// unscheduled entities, delta encoding, and the datagram budget. `None` when the client's
    /// entity is gone.
    /// The leaf whose PVS row this session needs (so the cache can be filled before the
    /// parallel pass).
    pub fn eye_leaf(&self, zone: &Zone, world: &ZoneWorld) -> Option<usize> {
        let me = zone.player(self.id)?;
        Some(world.bsp.leaf_for_point(me.mover.eye()))
    }

    pub fn build_snapshot(
        &mut self,
        zone: &Zone,
        world: &ZoneWorld,
        table: &TickTable,
        pvs: &PvsCache,
        tick: u32,
    ) -> Option<Vec<u8>> {
        let me = zone.player(self.id)?;
        let eye = me.mover.eye();
        let my_leaf = world.bsp.leaf_for_point(eye);
        let see_all = my_leaf == 0;
        // A missing row (not prepared) is treated as "see everything": never hide a body by
        // accident.
        let row: &[u8] = if see_all {
            &[]
        } else {
            pvs.row(my_leaf).unwrap_or(&[])
        };
        let see_all = see_all || row.is_empty();
        let my_party = me.party;
        let my_target = me.target;
        // Squad sight (COMPANIONS.md 5.2): in the command stance with the button held, the
        // client also sees through each living companion's eyes.
        let commanding = commanding(me);
        let mut squad_eyes: Vec<(Vec3, &[u8])> = Vec::new();
        if commanding {
            for e in table.entries.iter().filter(|e| e.owner == self.id) {
                if e.state.flags & flags::ALIVE != 0
                    && let Some(r) = pvs.row(e.leaf_top)
                {
                    squad_eyes.push((e.origin, r));
                }
            }
        }

        let base_idx = self.baseline_index(tick);
        let baseline_tick = base_idx.map_or(0, |i| self.history[i].server_tick);
        let mut snap = Snapshot::new(tick);
        snap.baseline_tick = baseline_tick;
        snap.last_input_tick = me.last_input_tick;
        snap.own = own_state(me);
        snap.entities.reserve(table.entries.len());
        // (id, distance) of droppable (other player) records for the size budget.
        let mut droppable: Vec<(EntityId, f32)> = Vec::new();

        {
            let base = base_idx.map(|i| &self.history[i]);
            let base_entities: &[EntityState] = base.map_or(&[], |b| &b.entities);
            // The table and the baseline both ascend by id: one merged walk pairs them.
            let mut j = 0;
            for e in &table.entries {
                while j < base_entities.len() && base_entities[j].id < e.id {
                    j += 1;
                }
                let base_rec = (j < base_entities.len() && base_entities[j].id == e.id)
                    .then(|| &base_entities[j]);
                if e.id == self.id {
                    snap.entities.push(player_state(me, true));
                    continue;
                }
                let mut dist = (e.origin - eye).length();
                let mut visible =
                    see_all || Bsp::leaf_in_pvs(row, e.leaf) || Bsp::leaf_in_pvs(row, e.leaf_top);
                for (at, squad_row) in &squad_eyes {
                    if Bsp::leaf_in_pvs(squad_row, e.leaf)
                        || Bsp::leaf_in_pvs(squad_row, e.leaf_top)
                    {
                        visible = true;
                        // The band is taken from the nearest squad member.
                        dist = dist.min((e.origin - *at).length());
                    }
                }
                if e.is_player {
                    // The client's own companions are always sent, wherever they are.
                    let mine = e.owner == self.id;
                    if !mine && dist > TOUCH_DIST && (!visible || dist > e.stealth) {
                        continue;
                    }
                    let scheduled =
                        base_rec.is_none() || band_scheduled(zone.rate, tick, e.id, dist);
                    let rec = match base_rec {
                        Some(b) if !scheduled => *b,
                        // Health rides along for the client's own party, for creatures,
                        // and for the body it targets (MODES.md 5.2).
                        _ if e.creature
                            || (my_party != 0 && e.party == my_party)
                            || e.id == my_target =>
                        {
                            EntityState {
                                health: Some(e.health),
                                ..e.state
                            }
                        }
                        _ => e.state,
                    };
                    droppable.push((e.id, dist));
                    snap.entities.push(rec);
                } else {
                    if !visible {
                        continue;
                    }
                    snap.entities.push(e.state);
                }
            }
            snap.normalize(base);
        }

        let base_tick = baseline_tick;
        let mut bytes = {
            let base = base_idx.map(|i| &self.history[i]);
            snap.encode(base)
        };
        droppable.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut drop_iter = droppable.into_iter();
        while bytes.len() > MAX_DATAGRAM_PAYLOAD {
            // Drop enough of the farthest records at once (about 6 bytes each) to fit, then
            // re-encode once; loop only if the estimate was short.
            let excess = bytes.len() - MAX_DATAGRAM_PAYLOAD;
            let mut to_drop = excess / 6 + 1;
            let mut dropped_any = false;
            let base = base_idx.map(|i| &self.history[i]);
            while to_drop > 0 {
                let Some((id, _)) = drop_iter.next() else {
                    break;
                };
                let idx = snap
                    .entities
                    .iter()
                    .position(|e| e.id == id)
                    .expect("droppable entity is listed");
                match base.and_then(|b| b.find(id)) {
                    // Carry the stale record forward: costs nothing on the wire.
                    Some(rec) => snap.entities[idx] = *rec,
                    // Never seen: leave it out entirely; it is first-sight again next tick.
                    None => {
                        snap.entities.remove(idx);
                    }
                }
                self.oversize_drops += 1;
                to_drop -= 1;
                dropped_any = true;
            }
            if !dropped_any {
                break;
            }
            debug_assert_eq!(snap.baseline_tick, base_tick);
            bytes = snap.encode(base);
        }

        self.history.push_back(snap);
        while self.history.len() > HISTORY {
            self.history.pop_front();
        }
        self.snapshots_sent += 1;
        self.snapshot_bytes += bytes.len() as u64;
        Some(bytes)
    }
}

/// In the command stance with the button held: what grants squad sight and lets orders
/// through (COMPANIONS.md 5.1).
pub fn commanding(p: &Player) -> bool {
    p.alive
        && p.mover.commanding(p.last_input_tick)
        && p.mover.buttons_prev & gm_core::sim::buttons::COMMAND != 0
}

pub fn frame_index(frame: ArchetypeFrame) -> u8 {
    match frame {
        ArchetypeFrame::Colossus => 0,
        ArchetypeFrame::Striker => 1,
        ArchetypeFrame::Caster => 2,
        ArchetypeFrame::Infiltrator => 3,
    }
}

pub fn frame_from_index(i: u8) -> ArchetypeFrame {
    match i {
        0 => ArchetypeFrame::Colossus,
        2 => ArchetypeFrame::Caster,
        3 => ArchetypeFrame::Infiltrator,
        _ => ArchetypeFrame::Striker,
    }
}

/// Wire state of a player; `own` adds velocity, health and the jump and script flags
/// (PROTOCOL.md 5).
pub fn player_state(p: &Player, own: bool) -> EntityState {
    let mut f = 0u16;
    if p.alive {
        f |= flags::ALIVE;
    }
    if p.mover.mv.on_ground {
        f |= flags::ON_GROUND;
    }
    if p.mover.dash.is_some() {
        f |= flags::DASHING;
    }
    if p.mover.guard == GuardState::Block {
        f |= flags::GUARDING;
    }
    if matches!(
        p.mover.guard,
        GuardState::Parry { .. } | GuardState::Whiff { .. }
    ) {
        f |= flags::PARRY;
    }
    if own && p.mover.mv.jump_held {
        f |= flags::JUMP_HELD;
    }
    if own && p.mover.script.is_some() {
        f |= flags::SCRIPT;
    }
    if own && p.mover.commanding(p.last_input_tick) {
        f |= flags::COMMANDING;
    }
    // Everyone sees a crouch (MODES.md 3.5): the body squats and its hitbox is shorter.
    if p.mover.crouched {
        f |= flags::CROUCHED;
    }
    // Everyone sees an RPG body stand the way it was left (MODES.md 5.1): its frames'
    // yaw is its camera's, which is nothing to draw it by.
    if p.sheet.kit.mode == gm_core::vocab::Mode::Rpg {
        f |= flags::RPG;
    }
    EntityState {
        id: p.id,
        spawn: SpawnInfo::Player {
            frame: frame_index(p.frame()),
            team: p.team(),
            aspects: p.sheet.build.aspects.0,
            armour: p.sheet.build.armour as u8,
        },
        pos: quant::quantize_pos3(p.mover.mv.origin.into()),
        yaw: quant::yaw_to_wire(p.mover.yaw),
        pitch: quant::pitch_to_wire(p.mover.pitch),
        vel: own.then(|| quant::quantize_vel3(p.mover.mv.velocity.into())),
        anim: p.anim,
        acting: p.acting,
        health: own.then_some(p.health.clamp(0, u16::MAX as i32) as u16),
        flags: f,
        status: p.mover.statuses.mask(),
    }
}

/// The own block (PROTOCOL.md 5): resources and statuses relative to the acknowledged frame.
pub fn own_state(p: &Player) -> OwnState {
    let now = p.last_input_tick;
    let guns = (p.sheet.kit.mode == gm_core::vocab::Mode::Gun).then(|| GunsWire {
        magazine: [p.mover.guns[0].magazine, p.mover.guns[1].magazine],
        reserve: [p.mover.guns[0].reserve, p.mover.guns[1].reserve],
        reloading: p.mover.reloading(now),
    });
    OwnState {
        guns,
        kits: p.mover.kits,
        using_kit: p.mover.using_kit(now),
        stamina: p.mover.stamina.round().clamp(0.0, u16::MAX as f32) as u16,
        focus: p.mover.focus.round().clamp(0.0, u16::MAX as f32) as u16,
        statuses: p
            .mover
            .statuses
            .active()
            .filter_map(|s| {
                Some(StatusWire {
                    status: s.status?.index(),
                    remaining: s.until.wrapping_sub(now),
                    magnitude: s.magnitude,
                    stacks: s.stacks,
                    source: s.source,
                })
            })
            .collect(),
    }
}

pub fn area_state(a: &Area) -> EntityState {
    EntityState {
        id: a.id,
        spawn: SpawnInfo::Area {
            owner: a.owner,
            def: a.ability as u32,
            radius: a.radius().round() as u32,
            harmful: a.def.harmful(),
        },
        pos: quant::quantize_pos3(a.origin.into()),
        yaw: quant::yaw_to_wire(a.dir.y.atan2(a.dir.x).to_degrees()),
        pitch: 900,
        vel: None,
        anim: 0,
        acting: 0,
        health: None,
        flags: flags::ALIVE,
        status: 0,
    }
}

pub fn projectile_state(pr: &Projectile) -> EntityState {
    let dir = pr.vel.normalize_or_zero();
    let yaw = dir.y.atan2(dir.x).to_degrees();
    let pitch = (-dir.z).asin().to_degrees();
    EntityState {
        id: pr.id,
        spawn: SpawnInfo::Projectile {
            owner: pr.owner,
            def: pr.ability as u32,
            input_tick: pr.input_tick,
        },
        pos: quant::quantize_pos3(pr.pos.into()),
        yaw: quant::yaw_to_wire(yaw),
        pitch: quant::pitch_to_wire(pitch),
        vel: None,
        anim: 0,
        acting: 0,
        health: None,
        flags: flags::ALIVE,
        status: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_the_zone_refused_itself_does_not_cost_the_second() {
        let gap = Duration::from_secs(1);
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut gate = RequestGate::default();
        // Told to walk up to the stall: the hub was never asked, and the next request
        // (a moment later, at the counter) passes.
        assert!(gate.begin(t0, gap));
        gate.cancel();
        assert!(gate.begin(at(100), gap));
        // That one went to the hub: its second counts, whatever is cancelled after it.
        assert!(gate.end());
        assert!(!gate.begin(at(600), gap));
        gate.cancel();
        assert!(!gate.begin(at(900), gap));
        assert!(gate.begin(at(1100), gap));
        // Cancelled after one that counted: the earlier second still stands.
        gate.cancel();
        assert!(!gate.begin(at(1099), gap));
        // An answer nobody is waiting for any more is known as such.
        assert!(!gate.end());
    }

    #[test]
    fn a_flood_of_requests_reaches_the_hub_once_per_gap() {
        let gap = Duration::from_secs(1);
        let t0 = Instant::now();
        let mut gate = RequestGate::default();
        assert!(gate.begin(t0, gap));
        // In flight: nothing else passes, however long it takes.
        assert!(!gate.begin(t0, gap));
        assert!(!gate.begin(t0 + Duration::from_secs(5), gap));
        gate.end();
        // Answered at once: the next one still waits out the gap.
        assert!(!gate.begin(t0 + Duration::from_millis(999), gap));
        assert!(gate.begin(t0 + gap, gap));
        // A thousand messages in the following second: none passes.
        let passed = (0..1000)
            .filter(|i| {
                gate.end();
                gate.begin(t0 + gap + Duration::from_micros(999 * i), gap)
            })
            .count();
        assert_eq!(passed, 0);
    }
}
