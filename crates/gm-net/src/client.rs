//! Client-side netcode shared by `gm-client` and `gm-bot` (PROTOCOL.md 7): prediction ring,
//! reconciliation, reconstructed snapshot store, interpolation tracks and input datagrams.
//! Pure state machine: no sockets, no clocks. The caller feeds ticks and datagrams.

use std::collections::{BTreeMap, VecDeque};

use glam::Vec3;
use gm_core::build::Sheet;
use gm_core::collide::{Aabb, Composite};
use gm_core::sim::{Action, Company, GuardState, Input, Mover, Nearby, step_mover, tick_delta};
use gm_core::status::{StatusSlot, Statuses};
use gm_core::tick::TickRate;
use gm_core::trace::{CollisionWorld, Hull};
use gm_core::vocab::{EntityId, Status};

use crate::NetError;
use crate::input::{InputDatagram, InputFrame, MAX_FRAMES};
use crate::quant;
use crate::snapshot::{EntityKind, EntityState, OwnState, Snapshot, SpawnInfo, flags};

pub const PREDICTION_RING: usize = 128;
pub const SNAPSHOT_RING: usize = 64;
/// Reconciliation tolerances (PROTOCOL.md 7.2).
pub const POS_TOLERANCE: f32 = 2.0;
pub const VEL_TOLERANCE: f32 = 16.0;
/// Interpolation delay (PROTOCOL.md 7.3).
pub const BASE_DELAY_TICKS: u32 = 6;
pub const MAX_DELAY_TICKS: u32 = 13;

/// The interpolation delay floor at `rate`. The constants above are ticks of the combat
/// rate; another rate keeps the same time, not the same count (six ticks are 94 ms at 64 Hz
/// and would be 300 ms in a 20 Hz town), and never less than two ticks, since interpolation
/// needs a snapshot on each side.
pub fn base_delay(rate: TickRate) -> u32 {
    (BASE_DELAY_TICKS * rate.hz())
        .div_ceil(TickRate::COMBAT.hz())
        .max(2)
}

/// The most the delay grows to under loss, at `rate`.
pub fn max_delay(rate: TickRate) -> u32 {
    (MAX_DELAY_TICKS * rate.hz())
        .div_ceil(TickRate::COMBAT.hz())
        .max(base_delay(rate) + 2)
}
const TRACK_SAMPLES: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClientStats {
    pub inputs_sent: u64,
    pub snapshots: u64,
    pub stale_snapshots: u64,
    pub unknown_baseline: u64,
    pub decode_errors: u64,
    /// Reconciliations above tolerance after the initial sync (any cause).
    pub corrections: u64,
    /// Corrections with no visible cause: no health, status or guard change, no death or
    /// respawn, no body within blocking distance, no positional ability, and a real position
    /// disagreement. These are the ones that indicate a prediction bug.
    pub corrections_unexplained: u64,
    /// Corrections where only the velocity disagreed: wall contact resolved on the server a
    /// frame earlier or later than on the client because of the quarter-unit rounding.
    pub corrections_velocity_only: u64,
    pub max_correction: f32,
    pub gaps: u64,
    pub max_gap: u32,
    pub predicted_ticks: u64,
    pub replayed_ticks: u64,
}

/// Samples of one other entity (PROTOCOL.md 7.3): the record at each tick it changed on the
/// wire, never the carry-forward of a tick its distance band skipped. Interpolating between
/// two real updates is what makes a far body glide; between a real update and its five
/// carried copies it would stand for five ticks and cross the whole gap in one.
#[derive(Clone, Debug, Default)]
pub struct Track {
    pub samples: VecDeque<(u32, EntityState)>,
    /// The newest server tick whose table held the entity, changed or not. A body standing
    /// still gets no samples, and this is what keeps it shown.
    pub seen: u32,
    /// Server tick at which the entity was removed for us (leaves at render time).
    pub removed_at: Option<u32>,
}

/// An interpolated view of another entity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderEntity {
    pub id: EntityId,
    pub kind: EntityKind,
    pub spawn: SpawnInfo,
    pub pos: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub anim: u8,
    /// The ability the stance belongs to (`AbilityId`; 0: none).
    pub acting: u16,
    pub flags: u16,
    /// Active statuses, a bit per `Status` index.
    pub status: u32,
    /// Health, for the bodies whose health the zone sends: the own party and creatures.
    pub health: Option<u16>,
    /// Velocity as the two snapshots around the render time show it.
    pub vel: Vec3,
}

impl RenderEntity {
    pub fn alive(&self) -> bool {
        self.flags & flags::ALIVE != 0
    }

    pub fn team(&self) -> u8 {
        match self.spawn {
            SpawnInfo::Player { team, .. } => team,
            _ => 0,
        }
    }

    pub fn has_status(&self, s: Status) -> bool {
        self.status & (1 << s.index()) != 0
    }
}

pub struct ClientState {
    pub my_id: EntityId,
    pub rate: TickRate,
    /// The own character (kit and derived stats), from `FromZone::Content`.
    pub sheet: Sheet,
    /// Client input tick counter.
    pub tick: u32,
    pub mover: Mover,
    ring: VecDeque<(u32, Input, Mover)>,
    frames: VecDeque<InputFrame>,
    snapshots: VecDeque<Snapshot>,
    pub newest_tick: u32,
    pub last_acked_input: u32,
    tracks: BTreeMap<EntityId, Track>,
    gap_ticks: VecDeque<u32>,
    pub delay_ticks: u32,
    last_shrink: u32,
    pub own_health: i32,
    pub own_alive: bool,
    pub own_anim: u8,
    /// The own animation state of every snapshot since this was last taken, in order,
    /// with the snapshot's server tick: what the zone said the body did, and when (a
    /// client that reads it once a frame would miss a state between two snapshots of one
    /// frame, and would time a state by its frames rather than the zone's clock).
    pub own_anims: Vec<(u32, u8)>,
    pub stats: ClientStats,
    synced: bool,
    /// Server tick until which corrections count as explained by a hit, death or respawn.
    explained_until: u32,
    /// Actions the local prediction produced this tick (for cosmetic previews).
    pub actions: Vec<Action>,
}

impl ClientState {
    pub fn new(my_id: EntityId, rate: TickRate, sheet: Sheet) -> ClientState {
        let mover = Mover::spawn(Vec3::ZERO, 0.0, &sheet);
        ClientState {
            my_id,
            rate,
            sheet,
            tick: 0,
            mover,
            ring: VecDeque::with_capacity(PREDICTION_RING),
            frames: VecDeque::with_capacity(MAX_FRAMES),
            snapshots: VecDeque::with_capacity(SNAPSHOT_RING),
            newest_tick: 0,
            last_acked_input: 0,
            tracks: BTreeMap::new(),
            gap_ticks: VecDeque::new(),
            delay_ticks: base_delay(rate),
            last_shrink: 0,
            own_health: 0,
            own_alive: false,
            own_anim: 0,
            own_anims: Vec::new(),
            stats: ClientStats::default(),
            synced: false,
            explained_until: 0,
            actions: Vec::new(),
        }
    }

    pub fn synced(&self) -> bool {
        self.synced
    }

    /// Switch to a new build (a respec took effect at the respawn). Only the sheet changes:
    /// the respawn snapshot resets the pools and the ring replays against the new numbers.
    /// Dropping the ring here would re-synchronise a round trip behind the server.
    pub fn set_sheet(&mut self, sheet: Sheet) {
        self.sheet = sheet;
    }

    /// Server tick the client displays for other entities, `extra_ticks` past the newest
    /// snapshot (the caller's sub-tick progress since it arrived, at most about one tick).
    pub fn render_tick(&self, extra_ticks: f32) -> f32 {
        if self.newest_tick == 0 {
            return 0.0;
        }
        self.newest_tick as f32 + extra_ticks.clamp(0.0, 1.5) - self.delay_ticks as f32
    }

    /// `view_tick` for the next input datagram (PROTOCOL.md 4).
    pub fn view_tick(&self) -> u32 {
        let t = self.render_tick(0.0);
        if t < 1.0 { 0 } else { t.floor() as u32 }
    }

    /// Advance the own entity by one client tick and build the input datagram to send.
    /// Prediction runs on the **dequantized wire input**, exactly what the server will run
    /// (PROTOCOL.md 2); predicting with the raw float yaw drifts about a unit every few seconds.
    pub fn local_tick(&mut self, world: &dyn CollisionWorld, input: Input) -> InputDatagram {
        let frame = InputFrame::from_sim(&input);
        let input = frame.to_sim();
        self.tick = self.tick.wrapping_add(1);
        if self.tick == 0 {
            // Tick 0 means "none" on the wire; skip it and start a fresh consecutive run of
            // frames so the datagram's `first_tick + i` labelling stays exact across the wrap.
            self.tick = 1;
            self.frames.clear();
        }
        let dt = self.rate.dt();
        let solids = self.latest_boxes();
        let composite = Composite {
            world,
            solids: &solids,
            own: Some(self.mover.aabb()),
        };
        self.actions.clear();
        if self.own_alive || !self.synced {
            let nearby = self.latest_nearby();
            step_mover(
                &composite,
                &self.sheet,
                &mut self.mover,
                &input,
                self.tick,
                dt,
                Company {
                    bodies: &nearby,
                    team: self.sheet.team,
                    party: 0,
                },
                &mut self.actions,
            );
        } else {
            self.mover.yaw = input.yaw;
            self.mover.pitch = input.pitch;
            self.mover.buttons_prev = input.buttons;
        }
        self.stats.predicted_ticks += 1;
        self.ring.push_back((self.tick, input, self.mover));
        while self.ring.len() > PREDICTION_RING {
            self.ring.pop_front();
        }
        self.frames.push_back(frame);
        while self.frames.len() > MAX_FRAMES {
            self.frames.pop_front();
        }
        let first = self.tick.wrapping_sub(self.frames.len() as u32 - 1);
        let mut d = InputDatagram::new(self.newest_tick, self.view_tick(), first);
        for f in &self.frames {
            d.push(*f);
        }
        self.stats.inputs_sent += 1;
        d
    }

    /// Decode a snapshot datagram, store it, reconcile the own entity, update tracks.
    pub fn on_snapshot(
        &mut self,
        world: &dyn CollisionWorld,
        bytes: &[u8],
    ) -> Result<(), NetError> {
        let snap = match Snapshot::decode(bytes, |t| {
            self.snapshots.iter().find(|s| s.server_tick == t)
        }) {
            Ok(s) => s,
            Err(NetError::UnknownBaseline(t)) => {
                self.stats.unknown_baseline += 1;
                return Err(NetError::UnknownBaseline(t));
            }
            Err(e) => {
                self.stats.decode_errors += 1;
                return Err(e);
            }
        };
        if self.newest_tick != 0 && tick_delta(snap.server_tick, self.newest_tick) <= 0 {
            self.stats.stale_snapshots += 1;
            return Ok(());
        }
        self.stats.snapshots += 1;
        if self.newest_tick != 0 {
            let gap = tick_delta(snap.server_tick, self.newest_tick) - 1;
            if gap > 0 {
                self.stats.gaps += 1;
                self.stats.max_gap = self.stats.max_gap.max(gap as u32);
                self.gap_ticks.push_back(snap.server_tick);
            }
        }
        self.newest_tick = snap.server_tick;
        self.adapt_delay();

        let my_eye = self.mover.eye();
        for e in &snap.entities {
            if e.id == self.my_id {
                continue;
            }
            let track = self.tracks.entry(e.id).or_default();
            track.removed_at = None;
            track.seen = snap.server_tick;
            match track.samples.back() {
                // Carried forward, or truly unchanged: either way nothing new is known.
                Some(&(_, last)) if last == *e => {}
                Some(&(t_last, last)) => {
                    // The first change in a while. The body was where it was through the
                    // last tick its band listed it, which is at most one interval back:
                    // a rest sample there makes the move start late and slow rather than
                    // snap to where it already is.
                    let dist = (Vec3::from(quant::dequantize_pos3(last.pos)) - my_eye).length();
                    let interval = crate::bands::band_interval(self.rate, dist);
                    if tick_delta(snap.server_tick, t_last) > interval as i32 {
                        track
                            .samples
                            .push_back((snap.server_tick.wrapping_sub(interval), last));
                    }
                    track.samples.push_back((snap.server_tick, *e));
                }
                None => track.samples.push_back((snap.server_tick, *e)),
            }
            while track.samples.len() > TRACK_SAMPLES {
                track.samples.pop_front();
            }
        }
        for &id in &snap.removed {
            if let Some(track) = self.tracks.get_mut(&id) {
                track.removed_at = Some(snap.server_tick);
            }
        }
        if let Some(own) = snap.find(self.my_id) {
            let own = *own;
            let own_state = snap.own.clone();
            self.reconcile(
                world,
                snap.server_tick,
                snap.last_input_tick,
                &own,
                &own_state,
            );
        }
        self.snapshots.push_back(snap);
        while self.snapshots.len() > SNAPSHOT_RING {
            self.snapshots.pop_front();
        }
        Ok(())
    }

    fn adapt_delay(&mut self) {
        let hz = self.rate.hz();
        while self
            .gap_ticks
            .front()
            .is_some_and(|&t| tick_delta(self.newest_tick, t) > hz as i32)
        {
            self.gap_ticks.pop_front();
        }
        let target =
            (base_delay(self.rate) + self.gap_ticks.len() as u32).min(max_delay(self.rate));
        if target > self.delay_ticks {
            self.delay_ticks = target;
            self.last_shrink = self.newest_tick;
        } else if self.delay_ticks > target
            && tick_delta(self.newest_tick, self.last_shrink) >= hz as i32
        {
            self.delay_ticks -= 1;
            self.last_shrink = self.newest_tick;
        }
    }

    fn reconcile(
        &mut self,
        world: &dyn CollisionWorld,
        server_tick: u32,
        last_input_tick: u32,
        own: &EntityState,
        own_state: &OwnState,
    ) {
        let was_alive = self.own_alive;
        let prev_health = self.own_health;
        self.own_health = own.health.map_or(self.own_health, |h| h as i32);
        self.own_alive = own.flags & flags::ALIVE != 0;
        self.own_anim = own.anim;
        if self.own_anims.len() < 64 {
            self.own_anims.push((server_tick, own.anim));
        }
        let respawned = !was_alive && self.own_alive && self.synced;
        // A hit (knockback), a death or a status the server put on us (a parry's stagger, a
        // freeze) lands on a server tick; the frame whose comparison reveals it may only run a
        // few ticks later (dejitter reserve, starvation), so the explanation stays valid for a
        // short window.
        let acked = self.ring.iter().find(|(t, _, _)| *t == last_input_tick);
        let status_changed = acked.is_some_and(|(_, _, m)| m.statuses.mask() != own.status);
        let guard_changed = acked.is_some_and(|(_, _, m)| {
            let server_parry = own.flags & flags::PARRY != 0;
            let server_block = own.flags & flags::GUARDING != 0;
            let client_parry =
                matches!(m.guard, GuardState::Parry { .. } | GuardState::Whiff { .. });
            let client_block = m.guard == GuardState::Block;
            server_parry != client_parry || server_block != client_block
        });
        if self.own_health != prev_health
            || was_alive != self.own_alive
            || status_changed
            || guard_changed
        {
            self.explained_until = server_tick.wrapping_add(8);
        }
        let explained = tick_delta(self.explained_until, server_tick) >= 0;
        let dt = self.rate.dt();
        let server_pos = Vec3::from(quant::dequantize_pos3(own.pos));
        let server_vel = own.vel.map(|v| Vec3::from(quant::dequantize_vel3(v)));
        // Statuses arrive relative to the acknowledged frame; rebuild them in our frame clock.
        let server_statuses = {
            let mut st = Statuses::default();
            for (slot, w) in st.slots.iter_mut().zip(&own_state.statuses) {
                *slot = StatusSlot {
                    status: Status::from_index(w.status),
                    until: last_input_tick.wrapping_add(w.remaining),
                    magnitude: w.magnitude,
                    stacks: w.stacks,
                    source: w.source,
                };
            }
            st
        };
        let apply = |m: &mut Mover| {
            // The wire position is rounded to 1/4 u and can sit exactly on a clip plane, where
            // the tracer would report solid and freeze the mover; nudge out first.
            m.mv.origin = gm_core::movement::nudge_position(world, m.mv.hull, server_pos);
            if let Some(v) = server_vel {
                m.mv.velocity = v;
            }
            m.mv.on_ground = own.flags & flags::ON_GROUND != 0;
            m.mv.jump_held = own.flags & flags::JUMP_HELD != 0;
            // Resources and statuses are the server's (block costs, enemy debuffs); a script
            // the server interrupted is dropped; a broken block is released.
            m.stamina = own_state.stamina as f32;
            m.focus = own_state.focus as f32;
            // The firearms' rounds are the zone's too (MODES.md 3.8); a reload the zone
            // does not see is dropped, one it sees and we do not runs on its word.
            if let Some(g) = own_state.guns {
                for i in 0..2 {
                    m.guns[i].magazine = g.magazine[i];
                    m.guns[i].reserve = g.reserve[i];
                }
                let held = m.held.min(1) as usize;
                if !g.reloading {
                    m.guns[held].reload_until = None;
                }
            }
            // The item bar likewise (MODES.md 11.3): a use the zone refused (full health)
            // or does not see is dropped.
            m.bar = own_state.bar;
            if own_state.using == 0 {
                m.use_until = None;
            }
            let immune = (
                m.statuses.chill_immune_until,
                m.statuses.stagger_immune_until,
            );
            m.statuses = server_statuses;
            m.statuses.chill_immune_until = immune.0;
            m.statuses.stagger_immune_until = immune.1;
            if own.flags & flags::SCRIPT == 0 {
                m.script = None;
            }
            if own.flags & flags::GUARDING == 0 && m.guard == GuardState::Block {
                m.guard = GuardState::None;
            }
            // A parry that landed: the server closed the window, no whiff recovery follows.
            if own.flags & flags::PARRY == 0
                && matches!(m.guard, GuardState::Parry { .. } | GuardState::Whiff { .. })
            {
                m.guard = GuardState::None;
            }
            // The command stance: the server's word when the prediction disagrees (a stagger
            // the client had not seen kept a script alive a frame longer, or the reverse).
            let commanding = own.flags & flags::COMMANDING != 0;
            if commanding != m.commanding(last_input_tick) {
                m.command_until = if commanding {
                    last_input_tick
                        .wrapping_add(gm_core::sim::command_exit_ticks(dt))
                        .wrapping_add(1)
                } else {
                    last_input_tick
                };
            }
        };
        // A respawn resets the ability state exactly as the server does (cooldowns survive).
        let derived = self.sheet.derived;
        let reset_abilities = |m: &mut Mover| {
            m.reset_actions();
            m.stamina = derived.stamina;
            m.focus = derived.focus;
        };
        if last_input_tick != 0 {
            self.last_acked_input = last_input_tick;
        }
        let idx = if last_input_tick == 0 {
            None
        } else {
            self.ring.iter().position(|(t, _, _)| *t == last_input_tick)
        };

        if !self.own_alive && self.synced {
            // Dead: the server owns the body and has dropped everything running.
            apply(&mut self.mover);
            self.mover.reset_actions();
            self.drop_acked(last_input_tick);
            return;
        }

        let (m, replay_from) = match idx {
            Some(i) => {
                let predicted = self.ring[i].2;
                let dpos = (server_pos - predicted.mv.origin).length();
                let dvel = server_vel.map_or(0.0, |v| (v - predicted.mv.velocity).length());
                let mismatch = dpos > POS_TOLERANCE || dvel > VEL_TOLERANCE;
                // Resources, statuses, scripts and guards the server changed without moving
                // us (a blocked hit's stamina, a Bleed, a landed parry) are adopted and
                // replayed too; they are not position corrections and are not counted.
                let soft = (own_state.stamina as f32 - predicted.stamina).abs() > 1.0
                    || (own_state.focus as f32 - predicted.focus).abs() > 1.0
                    || predicted.statuses.mask() != own.status
                    || (own.flags & flags::SCRIPT == 0) != predicted.script.is_none()
                    || (own.flags & flags::GUARDING != 0) != (predicted.guard == GuardState::Block)
                    || (own.flags & flags::PARRY != 0)
                        != matches!(
                            predicted.guard,
                            GuardState::Parry { .. } | GuardState::Whiff { .. }
                        )
                    || (own.flags & flags::COMMANDING != 0)
                        != predicted.commanding(last_input_tick)
                    // The stacks (MODES.md 11): kits bought at a stall, rounds the zone
                    // read after a buy, a use it refused at full health; the HUD counts
                    // them and the prediction begins a use only with a kit in hand.
                    || own_state.bar != predicted.bar
                    || (own_state.using == 0 && predicted.using_item(last_input_tick).is_some())
                    || own_state.guns.is_some_and(|g| {
                        (0..2).any(|i| {
                            g.magazine[i] != predicted.guns[i].magazine
                                || g.reserve[i] != predicted.guns[i].reserve
                        })
                    });
                if self.synced && !respawned {
                    if !mismatch && !soft {
                        self.drop_acked(last_input_tick);
                        return;
                    }
                    if !mismatch {
                        let mut m = predicted;
                        apply(&mut m);
                        self.ring[i].2 = m;
                        self.replay_from(world, i + 1, m);
                        self.drop_acked(last_input_tick);
                        return;
                    }
                    self.stats.corrections += 1;
                    self.stats.max_correction = self.stats.max_correction.max(dpos);
                    // Another body within reach of a block, given a few ticks of staleness
                    // (bodies within 128 u are always sent, PROTOCOL.md 5).
                    let near_other = self
                        .latest_boxes()
                        .iter()
                        .any(|b| (b.center() - predicted.mv.origin).truncate().length() < 128.0);
                    // A dash, charge or blink resolves against bodies the client only had
                    // interpolated; its landing spot is the server's call.
                    let moving_self = predicted.evading(last_input_tick);
                    // Same place, different velocity: wall contact landed on the clip plane
                    // on one side and a quarter unit short on the other; harmless.
                    let velocity_only = dpos <= POS_TOLERANCE;
                    if velocity_only {
                        self.stats.corrections_velocity_only += 1;
                    }
                    if !explained && !near_other && !moving_self && !velocity_only {
                        self.stats.corrections_unexplained += 1;
                        tracing::debug!(
                            me = self.my_id,
                            server_tick,
                            last_input_tick,
                            dpos = format_args!("{dpos:.2}"),
                            dvel = format_args!("{dvel:.1}"),
                            predicted = ?predicted.mv.origin,
                            server = ?server_pos,
                            pred_vel = ?predicted.mv.velocity,
                            server_vel = ?server_vel,
                            pred_ground = predicted.mv.on_ground,
                            server_ground = own.flags & flags::ON_GROUND != 0,
                            script = ?predicted.script,
                            dash = predicted.dash.is_some(),
                            guard = ?predicted.guard,
                            pred_stamina = format_args!("{:.0}", predicted.stamina),
                            server_stamina = own_state.stamina,
                            pred_focus = format_args!("{:.0}", predicted.focus),
                            server_focus = own_state.focus,
                            pred_status = predicted.statuses.mask(),
                            server_status = own.status,
                            server_flags = own.flags,
                            speed_scale = format_args!("{:.2}", predicted.statuses.speed_scale()),
                            anim = own.anim,
                            nearest = format_args!("{:.0}", self
                                .latest_boxes()
                                .iter()
                                .map(|b| (b.center() - predicted.mv.origin).truncate().length())
                                .fold(f32::INFINITY, f32::min)),
                            ring = self.ring.len(),
                            "unexplained correction"
                        );
                    }
                }
                let mut m = predicted;
                if respawned {
                    reset_abilities(&mut m);
                }
                apply(&mut m);
                self.ring[i].2 = m;
                (m, i + 1)
            }
            None if last_input_tick == 0 || !self.synced || respawned => {
                // The server has run none of our frames yet (first contact, respawn, or a lost
                // reference): rebuild from its state and replay everything still in flight.
                let mut m = self.mover;
                if respawned || !self.synced {
                    reset_abilities(&mut m);
                }
                apply(&mut m);
                (m, 0)
            }
            None => {
                // Acked tick older than the ring: drop what we cannot compare.
                self.drop_acked(last_input_tick);
                return;
            }
        };
        self.replay_from(world, replay_from, m);
        self.synced = true;
        self.drop_acked(last_input_tick);
    }

    /// Re-run the ring from index `from` starting at state `m`; the result is the new mover.
    fn replay_from(&mut self, world: &dyn CollisionWorld, from: usize, mut m: Mover) {
        let solids = self.latest_boxes();
        let nearby = self.latest_nearby();
        let dt = self.rate.dt();
        let mut sink = Vec::new();
        for j in from..self.ring.len() {
            let (tick, input, _) = self.ring[j];
            let composite = Composite {
                world,
                solids: &solids,
                own: Some(m.aabb()),
            };
            step_mover(
                &composite,
                &self.sheet,
                &mut m,
                &input,
                tick,
                dt,
                Company {
                    bodies: &nearby,
                    team: self.sheet.team,
                    party: 0,
                },
                &mut sink,
            );
            self.ring[j].2 = m;
            self.stats.replayed_ticks += 1;
        }
        self.mover = m;
    }

    /// Everything before the acknowledged frame is settled. The acknowledged frame itself stays:
    /// a later snapshot with the same `last_input_tick` (a tick in which none of our frames ran)
    /// can still change our state there (knockback), and must be compared again.
    fn drop_acked(&mut self, last_input_tick: u32) {
        if last_input_tick == 0 {
            return;
        }
        while self
            .ring
            .front()
            .is_some_and(|(t, _, _)| tick_delta(*t, last_input_tick) < 0)
        {
            self.ring.pop_front();
        }
    }

    /// Other entities interpolated at `t` (a value from `render_tick`).
    pub fn others_at(&self, t: f32) -> Vec<RenderEntity> {
        let mut out = Vec::with_capacity(self.tracks.len());
        for (&id, track) in &self.tracks {
            if track.removed_at.is_some_and(|r| t >= r as f32) {
                continue;
            }
            let Some(&(_, last)) = track.samples.back() else {
                continue;
            };
            if (track.seen as f32) < t - 4.0 * max_delay(self.rate) as f32 {
                continue; // long stale, nothing to show
            }
            // Bracket t: a = newest sample at or before t, b = oldest sample after t.
            let mut a: Option<&(u32, EntityState)> = None;
            let mut b: Option<&(u32, EntityState)> = None;
            for s in &track.samples {
                if s.0 as f32 <= t {
                    a = Some(s);
                } else {
                    b = Some(s);
                    break;
                }
            }
            let mut vel = Vec3::ZERO;
            let (pos, yaw, pitch, state) = match (a, b) {
                (Some(&(ta, sa)), Some(&(tb, sb))) => {
                    let alpha =
                        ((t - ta as f32) / (tb as f32 - ta as f32).max(1.0)).clamp(0.0, 1.0);
                    let pa = Vec3::from(quant::dequantize_pos3(sa.pos));
                    let pb = Vec3::from(quant::dequantize_pos3(sb.pos));
                    vel = (pb - pa) / ((tb as f32 - ta as f32).max(1.0) * self.rate.dt());
                    (
                        pa.lerp(pb, alpha),
                        lerp_angle(
                            quant::wire_to_yaw(sa.yaw),
                            quant::wire_to_yaw(sb.yaw),
                            alpha,
                        ),
                        quant::wire_to_pitch(sa.pitch) * (1.0 - alpha)
                            + quant::wire_to_pitch(sb.pitch) * alpha,
                        if alpha < 0.5 { sa } else { sb },
                    )
                }
                (Some(&(_, sa)), None) => (
                    Vec3::from(quant::dequantize_pos3(sa.pos)),
                    quant::wire_to_yaw(sa.yaw),
                    quant::wire_to_pitch(sa.pitch),
                    sa,
                ),
                (None, Some(&(_, sb))) => {
                    // Not yet in the past: it spawned after our render time; show it where it is.
                    let _ = last;
                    (
                        Vec3::from(quant::dequantize_pos3(sb.pos)),
                        quant::wire_to_yaw(sb.yaw),
                        quant::wire_to_pitch(sb.pitch),
                        sb,
                    )
                }
                (None, None) => continue,
            };
            out.push(RenderEntity {
                id,
                kind: state.spawn.kind(),
                spawn: state.spawn,
                pos,
                yaw,
                pitch,
                anim: state.anim,
                acting: state.acting,
                flags: state.flags,
                status: state.status,
                health: state.health,
                vel,
            });
        }
        out
    }

    /// Boxes of living other players at `t`, for drawing.
    pub fn other_boxes(&self, t: f32) -> Vec<Aabb> {
        self.others_at(t)
            .into_iter()
            .filter(|e| e.kind == EntityKind::Player && e.alive())
            .map(|e| Aabb::around(e.pos, Hull::Player))
            .collect()
    }

    /// Boxes of living other players at their newest known positions, for predicting
    /// body-blocks: fresher than the render time by the whole interpolation delay.
    pub fn latest_boxes(&self) -> Vec<Aabb> {
        self.tracks
            .values()
            .filter(|t| t.removed_at.is_none())
            .filter_map(|t| t.samples.back())
            .filter(|(_, e)| e.spawn.kind() == EntityKind::Player && e.flags & flags::ALIVE != 0)
            .map(|(_, e)| Aabb::around(Vec3::from(quant::dequantize_pos3(e.pos)), Hull::Player))
            .collect()
    }

    /// The living other bodies at their newest known positions, as the magnet and a
    /// target-action read them (MODES.md 4.2, 5.3). The party is not on the wire: in the
    /// wild everybody is read as an enemy here, and the zone's own reading decides.
    pub fn latest_nearby(&self) -> Vec<Nearby> {
        self.tracks
            .iter()
            .filter(|(_, t)| t.removed_at.is_none())
            .filter_map(|(id, t)| t.samples.back().map(|(_, e)| (*id, e)))
            .filter(|(_, e)| e.spawn.kind() == EntityKind::Player && e.flags & flags::ALIVE != 0)
            .map(|(id, e)| {
                let team = match e.spawn {
                    SpawnInfo::Player { team, .. } => team,
                    _ => 0,
                };
                let pos = Vec3::from(quant::dequantize_pos3(e.pos));
                Nearby {
                    id,
                    centre: pos + Vec3::Z * (Hull::Player.mins().z + Hull::Player.maxs().z) * 0.5,
                    velocity: Vec3::ZERO,
                    team,
                    party: 0,
                }
            })
            .collect()
    }

    /// Forget tracks that left before `t` and have nothing newer.
    pub fn prune(&mut self, t: f32) {
        self.tracks.retain(|_, tr| {
            !tr.removed_at
                .is_some_and(|r| t >= r as f32 && tr.samples.back().is_none_or(|(ts, _)| *ts < r))
        });
    }

    pub fn tracks(&self) -> &BTreeMap<EntityId, Track> {
        &self.tracks
    }

    pub fn snapshots(&self) -> &VecDeque<Snapshot> {
        &self.snapshots
    }
}

/// Shortest-arc interpolation of angles in degrees.
pub fn lerp_angle(a: f32, b: f32, alpha: f32) -> f32 {
    let mut d = (b - a).rem_euclid(360.0);
    if d > 180.0 {
        d -= 360.0;
    }
    (a + d * alpha).rem_euclid(360.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::collide::BoxWorld;
    use gm_core::sim::{buttons, test_content};

    fn client(id: EntityId) -> ClientState {
        ClientState::new(
            id,
            TickRate::COMBAT,
            test_content::phase2_sheet(TickRate::COMBAT),
        )
    }

    fn own_state(pos: Vec3, vel: Vec3, alive: bool, on_ground: bool) -> EntityState {
        let mut f = flags::ON_GROUND * on_ground as u16;
        if alive {
            f |= flags::ALIVE;
        }
        EntityState {
            id: 1,
            spawn: SpawnInfo::Player {
                frame: 1,
                team: 1,
                aspects: 1,
                armour: 0,
            },
            pos: quant::quantize_pos3(pos.into()),
            yaw: 0,
            pitch: 900,
            vel: Some(quant::quantize_vel3(vel.into())),
            anim: 0,
            acting: 0,
            health: Some(100),
            flags: f,
            status: 0,
        }
    }

    fn snap(tick: u32, baseline: u32, last_input: u32, entities: Vec<EntityState>) -> Snapshot {
        let mut s = Snapshot::new(tick);
        s.baseline_tick = baseline;
        s.last_input_tick = last_input;
        // The Phase 2 test character's full pools (MATRIX.md 6 v2 on flat attributes of
        // 10: stamina 60 + 3·20, focus 40 + 5·10).
        s.own = OwnState {
            stamina: 120,
            focus: 90,
            statuses: Vec::new(),
            guns: None,
            bar: [0; gm_core::sim::BAR_CELLS],
            using: 0,
        };
        s.entities = entities;
        s.normalize(None);
        s
    }

    #[test]
    fn first_snapshot_adopts_the_server_state_then_predicts_locally() {
        let world = BoxWorld::floor();
        let mut c = client(1);
        let s = snap(
            10,
            0,
            0,
            vec![own_state(
                Vec3::new(100.0, 0.0, 24.0),
                Vec3::ZERO,
                true,
                true,
            )],
        );
        c.on_snapshot(&world, &s.encode(None)).unwrap();
        assert!(c.synced());
        assert_eq!(c.mover.mv.origin, Vec3::new(100.0, 0.0, 24.0));
        let input = Input {
            forward: 1.0,
            ..Default::default()
        };
        for _ in 0..32 {
            let d = c.local_tick(&world, input);
            assert_eq!(d.ack_tick, 10);
        }
        assert!(c.mover.mv.origin.x > 130.0, "{:?}", c.mover.mv.origin);
        assert_eq!(c.stats.inputs_sent, 32);
    }

    #[test]
    fn matching_server_state_causes_no_correction_and_mismatch_replays() {
        let world = BoxWorld::floor();
        let mut c = client(1);
        let s = snap(
            10,
            0,
            0,
            vec![own_state(Vec3::new(0.0, 0.0, 24.0), Vec3::ZERO, true, true)],
        );
        c.on_snapshot(&world, &s.encode(None)).unwrap();
        let input = Input {
            forward: 1.0,
            ..Default::default()
        };
        for _ in 0..20 {
            c.local_tick(&world, input);
        }
        // The server agrees with our prediction at tick 10: replay the same physics to get it.
        let mut shadow = client(1);
        shadow.on_snapshot(&world, &s.encode(None)).unwrap();
        for _ in 0..10 {
            shadow.local_tick(&world, input);
        }
        let agreed = own_state(shadow.mover.mv.origin, shadow.mover.mv.velocity, true, true);
        let s2 = snap(11, 0, 10, vec![agreed]);
        let before = c.mover;
        c.on_snapshot(&world, &s2.encode(None)).unwrap();
        assert_eq!(c.stats.corrections, 0);
        assert_eq!(c.mover, before);
        // A server that disagrees by more than the tolerance pulls us back and replays.
        let moved = own_state(
            shadow.mover.mv.origin + Vec3::new(-50.0, 0.0, 0.0),
            shadow.mover.mv.velocity,
            true,
            true,
        );
        let s3 = snap(12, 0, 10, vec![moved]);
        // Ring entries up to tick 10 were dropped; feed a replayable ring first.
        for _ in 0..5 {
            c.local_tick(&world, input);
        }
        let s3 = Snapshot {
            last_input_tick: 21,
            ..s3
        };
        let x_before = c.mover.mv.origin.x;
        c.on_snapshot(&world, &s3.encode(None)).unwrap();
        assert_eq!(c.stats.corrections, 1);
        assert!(
            c.mover.mv.origin.x < x_before - 30.0,
            "{} vs {}",
            c.mover.mv.origin.x,
            x_before
        );
        assert!(c.stats.replayed_ticks >= 4);
    }

    #[test]
    fn others_are_interpolated_and_removed_at_render_time() {
        let world = BoxWorld::floor();
        let mut c = client(1);
        let mk = |tick: u32, x: f32| {
            let mut other = own_state(Vec3::new(x, 0.0, 24.0), Vec3::ZERO, true, true);
            other.id = 2;
            other.vel = None;
            other.health = None;
            snap(
                tick,
                0,
                0,
                vec![own_state(Vec3::ZERO, Vec3::ZERO, true, true), other],
            )
        };
        c.on_snapshot(&world, &mk(100, 0.0).encode(None)).unwrap();
        c.on_snapshot(&world, &mk(101, 10.0).encode(None)).unwrap();
        c.on_snapshot(&world, &mk(102, 20.0).encode(None)).unwrap();
        let t = c.render_tick(0.0);
        assert_eq!(t, 96.0, "6 ticks behind");
        // Before the first sample: shown at the first sample.
        let e = &c.others_at(t)[0];
        assert_eq!(e.pos.x, 0.0);
        let e = &c.others_at(100.5)[0];
        assert!((e.pos.x - 5.0).abs() < 1e-3);
        let e = &c.others_at(101.75)[0];
        assert!((e.pos.x - 17.5).abs() < 1e-3);
        // Removal takes effect at render time, not on arrival.
        let mut gone = snap(
            103,
            0,
            0,
            vec![own_state(Vec3::ZERO, Vec3::ZERO, true, true)],
        );
        gone.removed = vec![2];
        c.on_snapshot(&world, &gone.encode(None)).unwrap();
        assert_eq!(c.others_at(101.0).len(), 1);
        assert_eq!(c.others_at(103.0).len(), 0);
        c.prune(103.0);
        assert!(c.tracks().is_empty());
    }

    #[test]
    fn far_bodies_glide_between_their_real_updates() {
        // A body beyond 1,536 u is listed every sixth tick; the snapshots between carry its
        // last record forward. The track keeps only the real updates and glides between them.
        let world = BoxWorld::floor();
        let mut c = client(1);
        let mk = |tick: u32, x: f32| {
            let mut other = own_state(Vec3::new(x, 0.0, 24.0), Vec3::ZERO, true, true);
            other.id = 2;
            other.vel = None;
            other.health = None;
            snap(
                tick,
                0,
                0,
                vec![own_state(Vec3::ZERO, Vec3::ZERO, true, true), other],
            )
        };
        let feed = |c: &mut ClientState, tick: u32, x: f32| {
            c.on_snapshot(&world, &mk(tick, x).encode(None)).unwrap();
        };
        feed(&mut c, 100, 3000.0);
        for t in 101..106 {
            feed(&mut c, t, 3000.0);
        }
        feed(&mut c, 106, 3060.0);
        for t in 107..112 {
            feed(&mut c, t, 3060.0);
        }
        feed(&mut c, 112, 3120.0);
        assert_eq!(
            c.tracks()[&2].samples.len(),
            3,
            "one sample per real update"
        );
        let e = &c.others_at(103.0)[0];
        assert!((e.pos.x - 3030.0).abs() < 0.1, "{}", e.pos.x);
        assert!(e.vel.x > 0.0, "walking, not standing");
        let e = &c.others_at(109.0)[0];
        assert!((e.pos.x - 3090.0).abs() < 0.1, "{}", e.pos.x);

        // Standing still for a long while: no samples, but still shown where it is.
        for t in 113..=300 {
            feed(&mut c, t, 3120.0);
        }
        assert_eq!(c.tracks()[&2].samples.len(), 3);
        let e = &c.others_at(290.0)[0];
        assert!((e.pos.x - 3120.0).abs() < 0.1);
        // The first move after the rest starts from a rest sample one interval back (six
        // ticks in the far band), not from the sample of the stop long ago.
        for t in 301..306 {
            feed(&mut c, t, 3120.0);
        }
        feed(&mut c, 306, 3180.0);
        let samples: Vec<u32> = c.tracks()[&2].samples.iter().map(|s| s.0).collect();
        assert_eq!(samples, [100, 106, 112, 300, 306]);
        let e = &c.others_at(303.0)[0];
        assert!((e.pos.x - 3150.0).abs() < 0.1, "{}", e.pos.x);
    }

    #[test]
    fn the_delay_is_a_time_not_a_tick_count() {
        // The combat rate keeps its numbers exactly.
        assert_eq!(base_delay(TickRate::COMBAT), BASE_DELAY_TICKS);
        assert_eq!(max_delay(TickRate::COMBAT), MAX_DELAY_TICKS);
        // A 20 Hz town shows others 100 ms behind (not 300), at most 250 ms under loss.
        assert_eq!(base_delay(TickRate::TOWN), 2);
        assert_eq!(max_delay(TickRate::TOWN), 5);
        for hz in [1, 10, 20, 30, 64, 128] {
            let rate = TickRate::new(hz);
            assert!(base_delay(rate) >= 2 && max_delay(rate) >= base_delay(rate) + 2);
        }
    }

    #[test]
    fn gaps_widen_the_delay_and_it_shrinks_back() {
        let world = BoxWorld::floor();
        let mut c = client(1);
        let own = own_state(Vec3::ZERO, Vec3::ZERO, true, true);
        c.on_snapshot(&world, &snap(1, 0, 0, vec![own]).encode(None))
            .unwrap();
        c.on_snapshot(&world, &snap(4, 0, 0, vec![own]).encode(None))
            .unwrap();
        assert_eq!(c.stats.gaps, 1);
        assert_eq!(c.stats.max_gap, 2);
        assert_eq!(c.delay_ticks, 7);
        for t in 5..200 {
            c.on_snapshot(&world, &snap(t, 0, 0, vec![own]).encode(None))
                .unwrap();
        }
        assert_eq!(c.delay_ticks, BASE_DELAY_TICKS);
        // Stale and duplicate snapshots are ignored.
        c.on_snapshot(&world, &snap(150, 0, 0, vec![own]).encode(None))
            .unwrap();
        assert_eq!(c.stats.stale_snapshots, 1);
    }

    #[test]
    fn input_datagrams_carry_the_last_four_frames() {
        let world = BoxWorld::floor();
        let mut c = client(1);
        let d = c.local_tick(&world, Input::default());
        assert_eq!((d.first_tick, d.count), (1, 1));
        for i in 0..6 {
            let d = c.local_tick(
                &world,
                Input {
                    buttons: if i == 5 { buttons::JUMP } else { 0 },
                    ..Default::default()
                },
            );
            assert_eq!(d.last_tick(), c.tick);
        }
        let d = c.local_tick(&world, Input::default());
        assert_eq!(d.count, 4);
        assert_eq!(d.first_tick, 5);
        assert_eq!(d.frames[2].buttons, buttons::JUMP);
    }

    #[test]
    fn angle_lerp_takes_the_short_way() {
        assert!((lerp_angle(350.0, 10.0, 0.5) - 0.0).abs() < 1e-4);
        assert!((lerp_angle(10.0, 350.0, 0.5) - 0.0).abs() < 1e-4);
        assert!((lerp_angle(0.0, 180.0, 0.25) - 45.0).abs() < 1e-4);
    }
}
