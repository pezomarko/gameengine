//! The authoritative simulation of one zone (server only): the frame ledger, melee with lag
//! compensation, projectiles, areas, statuses, guards, deaths, respawns and respecs.

use std::collections::{BTreeMap, VecDeque};

use glam::Vec3;

use crate::build::{Build, BuildError, ContentPack, Kit, Sheet};
use crate::collide::{Aabb, BodyGrid, EntityWorld};
use crate::geom::{Capsule, ray_capsule, sweep_sphere_capsule};
use crate::matrix::{
    AttackerStats, DOT_PULSES_PER_S, DefenderStats, Gear, STAGGER_DECAY_PER_S, STAGGER_IMMUNITY_MS,
    STAGGER_MS, resolve_damage,
};
use crate::movement::{MoveVars, yaw_vectors};
use crate::rng::Rng;
use crate::sim::mover::{
    Action, Company, GuardState, Input, Mover, Nearby, anim, capsule_at, melee_hit_point,
    step_mover,
};
use crate::sim::{
    CONTROL_WINDOW_MS, CREDIT_BURST, DRAIN_DEPTH, HISTORY_TICKS, MAX_FRAMES_PER_TICK,
    MAX_QUEUED_FRAMES, MAX_REWIND_TICKS, PROJECTILE_OWNER_GRACE, RESERVE_FRAMES, RESPAWN_MS,
    REWIND_ALLOWANCE_TICKS, TEAM_WILD, tick_delta,
};
use crate::tick::{Tick, TickRate};
use crate::trace::{CollisionWorld, Contents, Hull};
use crate::vocab::{
    ApplyStatus, ArchetypeFrame, AreaEffect, Bypass, DamagePacket, DamageType, EntityId, Falloff,
    Guard, Interrupt, MeleeArc, Origin, Projectile as ProjectileDef, Riposte, Shape, StackRule,
    Status, StatusTarget, Trigger, Verb,
};

/// A bolt this fast is a bullet (MODES.md 3.6): it leaves a mark where it meets the world.
pub const BULLET_SPEED: f32 = 10_000.0;

/// Damage- and heal-over-time pulse every this many server ticks (MATRIX.md 8: 4 per second).
pub const DOT_INTERVAL_TICKS: Tick = 64 / DOT_PULSES_PER_S;
/// From this many living bodies on, a tick keeps a grid over them for its sweeps.
const GRID_FROM: usize = 24;

/// A spawn point: hull origin, facing, team (0 = any).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spawn {
    pub origin: Vec3,
    pub yaw: f32,
    pub team: u8,
}

/// Who produces a body's frames (COMPANIONS.md 2.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Driver {
    /// A client, through the frame ledger (PROTOCOL.md 4).
    #[default]
    Client,
    /// A mind: exactly one frame per server tick, handed over with [`Zone::drive`].
    Mind,
}

/// The furthest back a frame's claimed view is believed for statistics (half a second).
pub const MAX_CLAIMED_VIEW_LAG: Tick = 32;

/// A body as the server sees it: a player, a companion or a creature.
#[derive(Clone, Debug)]
pub struct Player {
    pub id: EntityId,
    pub sheet: Sheet,
    pub mover: Mover,
    pub health: i32,
    pub alive: bool,
    pub respawn_at: Tick,
    pub anim: u8,
    /// The ability whose script the stance shows (`AbilityId`: the pack's index and one),
    /// while the stance is a windup, a swing, a recovery or a cast; 0 otherwise. Everybody
    /// who sees the body is told (PROTOCOL.md 5), so that a swing can be drawn where it
    /// lands.
    pub acting: u16,
    /// Clamped rewind target for melee (PROTOCOL.md 7.4).
    pub view_tick: Tick,
    /// `min(13, half_rtt_ticks + 8)`; the server sets it from the measured RTT.
    pub max_rewind: Tick,
    /// The client's one-way latency in ticks as the server measured it (0 until told).
    pub half_rtt_ticks: Tick,
    pub last_input_tick: u32,
    credits: f32,
    /// `(client tick, input, clamped view tick, claimed view tick)` waiting to run,
    /// ascending by tick.
    queue: VecDeque<(u32, Input, Tick, Tick)>,
    /// The world tick the last executed frame says it was looking at, bounded to
    /// `MAX_CLAIMED_VIEW_LAG` but not to `max_rewind`: hits are resolved against the clamped
    /// tick (a client cannot buy stale targets), statistics about where a client aimed read
    /// this one (ANTICHEAT.md 4.1).
    pub view_claimed: Tick,
    pub starved_ticks: u64,
    pub executed_frames: u64,
    pub dropped_frames: u64,
    pub kills: u32,
    pub deaths: u32,
    /// Stagger build-up (MATRIX.md 7), server only.
    pub stagger: f32,
    /// Build applied at the next respawn.
    pub pending_build: Option<Build>,
    /// Server tick of the last hit taken (statistics, diagnostics).
    pub last_hit_tick: Tick,
    /// Server tick at which it last dealt or took damage; `None`: never. The zone reads
    /// it (what a body wears does not change in a fight, ITEMS.md 5); the sim does not.
    pub fought_at: Option<Tick>,
    /// In transit to another zone (HUB.md 3.3): the body stays, visible and hittable, but no
    /// frames run.
    pub ghost: bool,
    pub driver: Driver,
    /// What it wears, as the damage pipeline reads it (ITEMS.md 3): nothing, until the zone
    /// is told otherwise. It stays through a respawn and a change of build.
    pub gear: Gear,
    /// The party the body belongs to (COMPANIONS.md 3.1): its own id until it joins another,
    /// its commander's for a companion, 0 for a creature. Damage never reads it.
    pub party: u32,
    /// No timed respawn: the body stays down until [`Zone::revive`] (an encounter holds its
    /// dead, a creature belongs to its encounter).
    pub hold: bool,
    /// Nothing hurts it (COMPANIONS.md 8.1 `npc`): a packet lands for nothing, not even
    /// its triggers. The town's trainer.
    pub unhurt: bool,
    /// A mind's frame for the next tick.
    next: Option<Input>,
    /// Diminishing returns on controls (MODES.md 4.5), per kind (knockdown, launched,
    /// root, taunt): how many landed in a row, and the server tick of the last.
    pub controls: [(u8, Tick); 4],
    /// The body the last executed frame aimed at (MODES.md 5.2): its health goes on the
    /// wire to this one.
    pub target: EntityId,
    /// What one of each item cell's stack heals (MODES.md 11.3), from the zone's reading
    /// of the inventory and the bar; 0 until told, and for a cell that heals nothing.
    pub bar_heals: [i32; crate::sim::BAR_CELLS],
}

impl Player {
    pub fn frame(&self) -> ArchetypeFrame {
        self.sheet.build.frame
    }

    pub fn team(&self) -> u8 {
        self.sheet.team
    }

    pub fn capsule(&self) -> Capsule {
        self.mover.capsule(self.frame())
    }

    pub fn aabb(&self) -> Aabb {
        self.mover.aabb()
    }

    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }

    pub fn max_health(&self) -> i32 {
        self.sheet.derived.health
    }

    /// Whether it dealt or took damage within the last `ticks` ticks.
    pub fn fought_within(&self, now: Tick, ticks: Tick) -> bool {
        self.fought_at
            .is_some_and(|at| (tick_delta(now, at).max(0) as u32) < ticks)
    }

    fn attacker_stats(&self) -> AttackerStats {
        AttackerStats::from_derived(
            &self.sheet.derived,
            self.mover.statuses.magnitude(Status::Weaken),
            &self.gear,
        )
    }
}

/// A swing in progress (server only).
#[derive(Clone, Debug)]
pub struct Swing {
    pub attacker: EntityId,
    pub ability: u8,
    pub arc: MeleeArc,
    pub stats: AttackerStats,
    pub active_from: Tick,
    pub active_until: Tick,
    pub view_tick: Tick,
    pub hit: Vec<EntityId>,
    /// `Hit`-targeted statuses of the steps that follow the arc, applied with each hit.
    pub on_hit: Vec<ApplyStatus>,
    /// A parry riposte: not tied to a running script.
    pub riposte: bool,
}

#[derive(Clone, Debug)]
pub struct Projectile {
    pub id: EntityId,
    pub owner: EntityId,
    pub ability: u8,
    pub input_tick: u32,
    pub def: ProjectileDef,
    pub stats: AttackerStats,
    pub pos: Vec3,
    pub vel: Vec3,
    pub spawned: Tick,
    pub dies: Tick,
    pub pierce_left: u8,
    pub bounces_left: u8,
    pub hit: Vec<EntityId>,
    /// The multiplier in the head band (MODES.md 3.5): a firearm's, 1 for anything else.
    pub headshot: f32,
}

/// How long an instant area (one pulse, no duration) stays on the wire after its pulse,
/// so that it is in a snapshot and the shockwave can be seen where it struck.
pub const INSTANT_AREA_ECHO_MS: u32 = 100;

/// A pulsing volume (VOCABULARY.md 5.3), server only.
#[derive(Clone, Debug)]
pub struct Area {
    pub id: EntityId,
    pub owner: EntityId,
    pub ability: u8,
    pub def: AreaEffect,
    pub stats: AttackerStats,
    pub origin: Vec3,
    /// Facing for cones.
    pub dir: Vec3,
    pub next_pulse: Tick,
    pub ends: Tick,
    pub pulses: u32,
}

impl Area {
    /// Largest extent, for the wire and for drawing.
    pub fn radius(&self) -> f32 {
        match self.def.shape {
            Shape::Sphere { radius } | Shape::Cylinder { radius, .. } => radius,
            Shape::Cone { length, .. } => length,
            Shape::Box { half_extents } => half_extents.iter().cloned().fold(0.0, f32::max),
        }
    }
}

/// Recent origins per entity for melee rewinds.
#[derive(Clone, Debug, Default)]
pub struct History {
    /// Per recorded tick: each living body's hull origin and whether it was crouched
    /// (MODES.md 3.5: the rewound capsule is as short as the body was).
    frames: VecDeque<(Tick, Vec<Recorded>)>,
}

/// One body in a tick of the history: its id, its hull origin, whether it crouched.
pub type Recorded = (EntityId, Vec3, bool);

impl History {
    pub fn record(&mut self, tick: Tick, bodies: Vec<Recorded>) {
        self.frames.push_back((tick, bodies));
        while self.frames.len() > HISTORY_TICKS {
            self.frames.pop_front();
        }
    }

    /// Origin and posture of `id` at `tick`, or at the nearest later recorded tick.
    pub fn body_at(&self, tick: Tick, id: EntityId) -> Option<(Vec3, bool)> {
        self.frames
            .iter()
            .filter(|(t, _)| tick_delta(*t, tick) >= 0)
            .min_by_key(|(t, _)| tick_delta(*t, tick))
            .and_then(|(_, bodies)| {
                bodies
                    .iter()
                    .find(|(e, _, _)| *e == id)
                    .map(|(_, o, c)| (*o, *c))
            })
    }

    /// Origin of `id` at `tick`, or at the nearest later recorded tick.
    pub fn origin_at(&self, tick: Tick, id: EntityId) -> Option<Vec3> {
        self.body_at(tick, id).map(|(o, _)| o)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HitKind {
    Melee,
    Projectile,
    Area,
    /// Damage over time (Bleed, Burn).
    Dot,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoneEvent {
    Hit {
        attacker: EntityId,
        target: EntityId,
        amount: i32,
        kind: HitKind,
        /// What the target's block took off the hit (0 when it was not blocked).
        absorbed: i32,
        /// Where the blow landed (LOOK.md 13.11): a bolt's or a blade's point on the hull,
        /// the body's centre for an area or a pulse.
        at: Vec3,
    },
    /// A bullet met the world (MODES.md 10.2, the director 2026-10-07): where, and the
    /// surface's normal; the mark it leaves is the client's.
    Impact {
        at: Vec3,
        normal: Vec3,
    },
    Healed {
        target: EntityId,
        /// Who put the Regen there (0 = nobody).
        source: EntityId,
        /// Health actually restored: healing a full body is not healing.
        amount: i32,
    },
    /// A reload took rounds off the stack carried (MODES.md 11.2): `hand` 0 the primary,
    /// 1 the secondary. The zone's hub is told.
    RoundsLoaded {
        id: EntityId,
        hand: u8,
        rounds: u16,
    },
    /// One of an item cell's stack was used up (MODES.md 11.3, LOOK.md 3.2): the cell,
    /// 0-based; the heal is a `Healed` of its own.
    ItemUsed {
        id: EntityId,
        cell: u8,
    },
    Killed {
        victim: EntityId,
        /// 0 = the world.
        killer: EntityId,
    },
    Respawned(EntityId),
    ProjectileSpawned {
        id: EntityId,
        owner: EntityId,
        input_tick: u32,
        /// Ticks the projectile was stepped forward at once: how far behind the present
        /// the shooter's view of the world was, as far as the zone honours it (PROTOCOL.md
        /// 7.4).
        lag: Tick,
        /// How far behind the present the shooter's frame says its view was, bounded only
        /// by `MAX_CLAIMED_VIEW_LAG`: what its aim is judged against.
        view_lag: Tick,
        speed: f32,
        gravity: f32,
        /// Ticks it flies at most.
        lifetime: Tick,
        /// Where it left from: the muzzle, or the eye when the muzzle was in a wall.
        origin: Vec3,
    },
    ProjectileRemoved(EntityId),
    AreaSpawned {
        id: EntityId,
        owner: EntityId,
    },
    AreaRemoved(EntityId),
    Parried {
        defender: EntityId,
        attacker: EntityId,
    },
    GuardBroken(EntityId),
    Staggered(EntityId),
    StatusApplied {
        target: EntityId,
        status: Status,
        source: EntityId,
    },
}

/// The head band (MODES.md 3.5): a bolt entering the hull within this many units of its top
/// is a headshot.
pub const HEAD_BAND: f32 = 12.0;

/// What a firearm's shot adds to a bolt (MODES.md 3.3, 3.4), and the body it is aimed at.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Shot {
    /// Where the spray has turned the gun before this shot (MODES.md 3.3).
    pub turn: (f32, f32),
    pub cone_deg: f32,
    pub headshot: f32,
    pub target: u32,
}

/// The largest id a body, a projectile or an area gets: ids above it are the numbers a
/// zone gives parties of people (gm-server's `PARTY_BASE`), and a party is told apart
/// from a body by it.
pub const MAX_ENTITY_ID: EntityId = 0x7FFF_FFFF;

/// How far the eye ray is followed for the point a free-aimed bolt converges on
/// (MODES.md 3.3); past it the bolt flies along the look.
pub const CONVERGE_RANGE: f32 = 16_384.0;

/// The authoritative simulation of one zone.
pub struct Zone {
    pub rate: TickRate,
    pub tick: Tick,
    pub content: ContentPack,
    players: BTreeMap<EntityId, Player>,
    projectiles: Vec<Projectile>,
    areas: Vec<Area>,
    swings: Vec<Swing>,
    history: History,
    next_id: EntityId,
    rng: Rng,
    spawns: Vec<Spawn>,
    respawn_ticks: Tick,
    pub events: Vec<ZoneEvent>,
}

impl Zone {
    /// `spawns` are hull-origin spawn points; an empty list spawns at the origin.
    pub fn new(rate: TickRate, seed: u64, spawns: Vec<Spawn>, content: ContentPack) -> Zone {
        Zone {
            rate,
            tick: 0,
            content,
            players: BTreeMap::new(),
            projectiles: Vec::new(),
            areas: Vec::new(),
            swings: Vec::new(),
            history: History::default(),
            next_id: 1,
            rng: Rng::new(seed),
            spawns: if spawns.is_empty() {
                vec![Spawn {
                    origin: Vec3::new(0.0, 0.0, 24.0),
                    yaw: 0.0,
                    team: 0,
                }]
            } else {
                spawns
            },
            respawn_ticks: rate.ms_to_ticks(RESPAWN_MS),
            events: Vec::new(),
        }
    }

    pub fn players(&self) -> impl Iterator<Item = &Player> {
        self.players.values()
    }

    pub fn player(&self, id: EntityId) -> Option<&Player> {
        self.players.get(&id)
    }

    pub fn player_mut(&mut self, id: EntityId) -> Option<&mut Player> {
        self.players.get_mut(&id)
    }

    pub fn projectiles(&self) -> &[Projectile] {
        &self.projectiles
    }

    pub fn areas(&self) -> &[Area] {
        &self.areas
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn spawns(&self) -> &[Spawn] {
        &self.spawns
    }

    /// The team with fewer living or dead players (1 or 2); for auto-assignment.
    pub fn smallest_team(&self) -> u8 {
        let count = |t: u8| self.players.values().filter(|p| p.team() == t).count();
        if count(2) < count(1) { 2 } else { 1 }
    }

    fn alloc_id(&mut self) -> EntityId {
        let id = self.next_id;
        assert!(id <= MAX_ENTITY_ID, "entity ids exhausted");
        self.next_id += 1;
        id
    }

    /// Add a player at a free spawn point of its team. Ids are monotonic and never reused.
    /// A build that fails MATRIX.md 9 is refused.
    pub fn add_player(
        &mut self,
        world: &dyn CollisionWorld,
        build: Build,
        team: u8,
    ) -> Result<EntityId, BuildError> {
        build.validate(&self.content)?;
        let (origin, yaw) = self.free_spawn(world, Hull::Player, team);
        Ok(self.add_player_at(build, team, origin, yaw))
    }

    /// Add a player at an exact hull origin (zone handoff arrivals, tests). The build is
    /// trusted (validate it first).
    pub fn add_player_at(&mut self, build: Build, team: u8, origin: Vec3, yaw: f32) -> EntityId {
        let sheet = Sheet::new(build, &self.content, team);
        self.add_body(sheet, origin, yaw, Driver::Client)
    }

    /// Add a body with a ready sheet (a player's, a companion's or a creature's) at an exact
    /// hull origin. Its party is itself; see [`Zone::set_party`].
    pub fn add_body(&mut self, sheet: Sheet, origin: Vec3, yaw: f32, driver: Driver) -> EntityId {
        let id = self.alloc_id();
        let mover = Mover::spawn(origin, yaw, &sheet);
        let health = sheet.derived.health;
        let p = Player {
            id,
            sheet,
            mover,
            health,
            alive: true,
            respawn_at: 0,
            anim: anim::IDLE,
            acting: 0,
            view_tick: self.tick,
            view_claimed: self.tick,
            max_rewind: MAX_REWIND_TICKS,
            half_rtt_ticks: 0,
            last_input_tick: 0,
            credits: CREDIT_BURST,
            queue: VecDeque::new(),
            starved_ticks: 0,
            executed_frames: 0,
            dropped_frames: 0,
            kills: 0,
            deaths: 0,
            stagger: 0.0,
            pending_build: None,
            last_hit_tick: 0,
            fought_at: None,
            ghost: false,
            driver,
            gear: Gear::NONE,
            bar_heals: [0; crate::sim::BAR_CELLS],
            party: id,
            hold: false,
            unhurt: false,
            next: None,
            controls: [(0, 0); 4],
            target: 0,
        };
        self.players.insert(id, p);
        id
    }

    /// Ticks between the pulses of a status at this zone's rate: four pulses a second
    /// (`DOT_INTERVAL_TICKS` is this at 64 Hz; a zone at 20 Hz pulsed every 0.8 s by it,
    /// and its statuses did a third of what they say).
    fn dot_interval(&self) -> Tick {
        (self.rate.hz() / DOT_PULSES_PER_S).max(1)
    }

    /// What a body wears from now on (ITEMS.md 3.3): at once, for every packet made or
    /// landing after this. Kept within the cap whatever was told.
    pub fn set_gear(&mut self, id: EntityId, gear: Gear) {
        if let Some(p) = self.players.get_mut(&id) {
            p.gear = gear.clamped();
        }
    }

    /// The stacks a body carries (MODES.md 11), as the hub reads them: `(template,
    /// quantity, heals)`, and its item bar (LOOK.md 3.2): the template on each cell. A
    /// firearm's reserve is the quantity of the stack its `ammo` names; a cell counts the
    /// stack it names and heals what that template says. Told at the claim and after
    /// every change the hub knows of.
    pub fn set_stacks(
        &mut self,
        id: EntityId,
        stacks: &[(String, u32, Option<i32>)],
        bar: &[Option<String>],
    ) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let kit = &p.sheet.kit;
        for (g, slot) in p.mover.guns.iter_mut().zip([kit.primary, kit.secondary]) {
            let Some(f) = slot.and_then(|i| kit.abilities[i as usize].firearm.as_ref()) else {
                continue;
            };
            g.reserve = if f.ammo.is_empty() {
                0
            } else {
                stacks
                    .iter()
                    .filter(|(t, _, _)| *t == f.ammo)
                    .map(|(_, q, _)| *q)
                    .sum::<u32>()
                    .min(u16::MAX as u32) as u16
            };
        }
        for cell in 0..crate::sim::BAR_CELLS {
            let template = bar.get(cell).and_then(|t| t.as_deref());
            let of_cell = stacks
                .iter()
                .filter(|(t, _, _)| Some(t.as_str()) == template);
            p.mover.bar[cell] = of_cell
                .clone()
                .map(|(_, q, _)| *q)
                .sum::<u32>()
                .min(u16::MAX as u32) as u16;
            p.bar_heals[cell] = of_cell.filter_map(|(_, _, h)| *h).max().unwrap_or(0);
        }
    }

    pub fn set_party(&mut self, id: EntityId, party: u32) {
        if let Some(p) = self.players.get_mut(&id) {
            p.party = party;
        }
    }

    /// Hold a body's respawn (or release it): a held body stays down until [`Zone::revive`].
    /// Releasing a dead body lets the timed respawn run from now.
    /// Whether anything hurts the body (the town's people: never).
    pub fn set_unhurt(&mut self, id: EntityId, unhurt: bool) {
        if let Some(p) = self.players.get_mut(&id) {
            p.unhurt = unhurt;
        }
    }

    pub fn set_hold(&mut self, id: EntityId, hold: bool) {
        let now = self.tick;
        let respawn_ticks = self.respawn_ticks;
        if let Some(p) = self.players.get_mut(&id) {
            if p.hold && !hold && !p.alive {
                p.respawn_at = now.wrapping_add(respawn_ticks);
            }
            p.hold = hold;
        }
    }

    /// A mind's frame for the next tick (COMPANIONS.md 2.1). Ignored for client-driven bodies.
    pub fn drive(&mut self, id: EntityId, input: Input) {
        if let Some(p) = self.players.get_mut(&id)
            && p.driver == Driver::Mind
        {
            p.next = Some(input);
        }
    }

    /// Put a body back on its feet at `origin` with full pools and nothing running: a held
    /// body's respawn, or a creature restored by its encounter. Cooldowns are cleared too
    /// when `fresh` (a reset creature starts over; a respawning player keeps them).
    pub fn revive(&mut self, id: EntityId, origin: Vec3, yaw: f32, fresh: bool) {
        self.swings.retain(|s| s.attacker != id);
        let content = &self.content;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        let was_dead = !p.alive;
        if let Some(build) = p.pending_build.take() {
            p.sheet = Sheet::new(build, content, p.team());
        }
        let cooldowns = p.mover.cooldowns;
        p.mover = Mover::spawn(origin, yaw, &p.sheet);
        if !fresh {
            p.mover.cooldowns = cooldowns;
        } else {
            // Ready at once in the body's own frame clock.
            p.mover.cooldowns = [p.last_input_tick; crate::sim::MAX_ABILITIES];
        }
        p.health = p.sheet.derived.health;
        p.alive = true;
        p.stagger = 0.0;
        p.next = None;
        if was_dead {
            self.events.push(ZoneEvent::Respawned(id));
        }
    }

    /// A free place to stand near `near`: the point itself, then rings around it, where the
    /// hull is in open space, has ground under it and overlaps no living body. Falls back to
    /// `near`.
    pub fn spot_near(&self, world: &dyn CollisionWorld, near: Vec3, hull: Hull) -> Vec3 {
        let free = |origin: Vec3| -> Option<Vec3> {
            if world.point_contents(hull, origin) != Contents::Empty {
                return None;
            }
            // Reachable in a straight line from the point (not through a wall).
            if world.trace(Hull::Point, near, origin).fraction < 1.0 {
                return None;
            }
            let down = world.trace(hull, origin, origin - Vec3::Z * 96.0);
            if down.start_solid || down.fraction >= 1.0 {
                return None;
            }
            let bb = Aabb::around(down.end, hull);
            (!self
                .players
                .values()
                .any(|p| p.alive && p.aabb().overlaps(&bb)))
            .then_some(down.end)
        };
        for radius in [0.0f32, 48.0, 80.0, 112.0, 160.0] {
            let steps = if radius == 0.0 { 1 } else { 8 };
            for i in 0..steps {
                let a = i as f32 * core::f32::consts::TAU / steps as f32;
                let p = near + Vec3::new(a.cos(), a.sin(), 0.0) * radius;
                if let Some(spot) = free(p) {
                    return spot;
                }
            }
        }
        near
    }

    /// Mark a player as a ghost (or back); a ghost's frames are consumed but never run.
    pub fn set_ghost(&mut self, id: EntityId, ghost: bool) {
        if let Some(p) = self.players.get_mut(&id) {
            p.ghost = ghost;
            if ghost {
                p.mover.reset_actions();
            }
        }
    }

    pub fn remove_player(&mut self, id: EntityId) -> Option<Player> {
        self.swings.retain(|s| s.attacker != id);
        self.players.remove(&id)
    }

    /// Content changed under a running zone (GM.md 3): every body's kit is compiled
    /// against the new pack, in place. Builds, health, cooldowns and a script under way
    /// are kept; a script reads the new numbers from its next step on.
    pub fn retune(&mut self, pack: ContentPack) {
        self.content = pack;
        for p in self.players.values_mut() {
            p.sheet.kit = Kit::from_build(&p.sheet.build, &self.content);
        }
    }

    /// Validate a new build and apply it at the player's next respawn (MATRIX.md 9).
    pub fn request_respec(&mut self, id: EntityId, build: Build) -> Result<(), BuildError> {
        build.validate(&self.content)?;
        if let Some(p) = self.players.get_mut(&id) {
            p.pending_build = Some(build);
        }
        Ok(())
    }

    /// A game master's respec (GM.md 2): the build now, where the body stands, with full
    /// pools and nothing running. Told as a `Respawned` so the client's prediction and
    /// everyone's picture of the body switch as they do at a respawn.
    pub fn respec_now(&mut self, id: EntityId, build: Build) -> Result<(), BuildError> {
        build.validate(&self.content)?;
        let Some(p) = self.players.get(&id) else {
            return Ok(());
        };
        let (origin, yaw) = (p.mover.mv.origin, p.mover.yaw);
        self.request_respec(id, build)?;
        self.revive(id, origin, yaw, true);
        self.events.push(ZoneEvent::Respawned(id));
        Ok(())
    }

    /// A game master's healing (GM.md 2): full health, stamina and focus, every cooldown
    /// ready, every status gone; where the body stands. Nothing for a dead body: that is
    /// what a respawn is for.
    pub fn make_whole(&mut self, id: EntityId) {
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if !p.alive {
            return;
        }
        p.health = p.sheet.derived.health;
        p.mover.stamina = p.sheet.derived.stamina;
        p.mover.focus = p.sheet.derived.focus;
        p.mover.cooldowns = [p.last_input_tick; crate::sim::MAX_ABILITIES];
        p.mover.statuses = Default::default();
        p.stagger = 0.0;
    }

    /// Queue a frame for execution (PROTOCOL.md 4). `view_tick` 0 means "no rewind". Frames
    /// already executed or already queued are ignored; a late datagram's frames still slot in
    /// ahead of newer ones (UDP reorders), so the redundancy is never wasted.
    pub fn queue_input(&mut self, id: EntityId, input_tick: u32, input: Input, view_tick: Tick) {
        let now = self.tick;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if (p.executed_frames > 0 || !p.queue.is_empty())
            && tick_delta(input_tick, p.last_input_tick) <= 0
        {
            return;
        }
        let max_rewind = p.max_rewind.min(MAX_REWIND_TICKS);
        let clamped_view = if view_tick == 0 {
            now
        } else {
            let lag = tick_delta(now, view_tick);
            if lag < 0 {
                now
            } else {
                now.wrapping_sub((lag as Tick).min(max_rewind))
            }
        };
        let pos = p
            .queue
            .partition_point(|(t, _, _, _)| tick_delta(*t, input_tick) < 0);
        if p.queue
            .get(pos)
            .is_some_and(|(t, _, _, _)| *t == input_tick)
        {
            return;
        }
        let claimed_view = if view_tick == 0 {
            now
        } else {
            let lag = tick_delta(now, view_tick).clamp(0, MAX_CLAIMED_VIEW_LAG as i32);
            now.wrapping_sub(lag as Tick)
        };
        p.queue
            .insert(pos, (input_tick, input, clamped_view, claimed_view));
        p.view_tick = clamped_view;
        while p.queue.len() > MAX_QUEUED_FRAMES {
            p.queue.pop_front();
            p.dropped_frames += 1;
        }
    }

    /// Tell the zone a client's one-way latency so `view_tick` clamps honestly.
    pub fn set_half_rtt_ticks(&mut self, id: EntityId, half_rtt_ticks: Tick) {
        if let Some(p) = self.players.get_mut(&id) {
            p.max_rewind = (half_rtt_ticks + REWIND_ALLOWANCE_TICKS).min(MAX_REWIND_TICKS);
            p.half_rtt_ticks = half_rtt_ticks;
        }
    }

    /// One server tick in the order of VOCABULARY.md 7 (steps 1–7; snapshots are the caller's).
    pub fn step(&mut self, world: &dyn CollisionWorld) {
        self.tick = self.tick.wrapping_add(1);
        let now = self.tick;
        let dt = self.rate.dt();
        let ids: Vec<EntityId> = self.players.keys().copied().collect();

        // 1 + 2: inputs and movement, players blocking each other. One shared box list per
        // tick; each mover ignores its own entry.
        let mut fires: Vec<(EntityId, u8, u8, u32, Tick, Shot)> = Vec::new();
        let mut area_spawns: Vec<(EntityId, u8, u8, u32)> = Vec::new();
        let mut solids: Vec<(EntityId, Aabb)> = self
            .players
            .values()
            .filter(|o| o.alive)
            .map(|o| (o.id, o.aabb()))
            .collect();
        // With a crowd, a grid over the boxes: each sweep looks at its neighbours only.
        let mut grid = (solids.len() >= GRID_FROM && solids.len() <= u16::MAX as usize)
            .then(|| BodyGrid::build(&solids));
        // What every mover sees of the others this tick (MODES.md 4.2, 5.3): where the
        // tick began; a swing's own rewind is the zone's.
        let nearby: Vec<Nearby> = self
            .players
            .values()
            .filter(|o| o.alive)
            .map(|o| Nearby {
                id: o.id,
                centre: o.capsule().center(),
                velocity: o.mover.mv.velocity,
                team: o.team(),
                party: o.party,
            })
            .collect();
        for &id in &ids {
            let p = self.players.get_mut(&id).expect("id from keys");
            p.credits = (p.credits + 1.0).min(CREDIT_BURST);
            // The stacks before the frames (MODES.md 11): what the frames took off them
            // is told below.
            let stacks_before = (
                p.mover.guns[0].reserve,
                p.mover.guns[1].reserve,
                p.mover.use_until,
            );
            p.stagger = (p.stagger - STAGGER_DECAY_PER_S * dt).max(0.0);
            let depth = p.queue.len();
            let mut allowed: u32 = if depth >= DRAIN_DEPTH {
                2
            } else if depth > RESERVE_FRAMES {
                1
            } else {
                0
            };
            let mut executed = 0;
            let mut actions: Vec<(u32, Tick, Action)> = Vec::new();
            let mut sink = Vec::new();
            if p.driver == Driver::Mind {
                // A mind's body runs exactly one frame per tick, in its own frame clock, and
                // its swings are not rewound: it has no latency to compensate.
                allowed = 0;
                if let Some(input) = p.next.take() {
                    let t = p.last_input_tick.wrapping_add(1);
                    p.last_input_tick = t;
                    p.executed_frames += 1;
                    p.view_tick = now;
                    p.view_claimed = now;
                    executed = 1;
                    if p.alive {
                        let sheet = &p.sheet;
                        let composite = EntityWorld {
                            world,
                            solids: &solids,
                            ignore: id,
                            own: Some(p.mover.aabb()),
                            grid: grid.as_ref(),
                        };
                        let company = Company {
                            bodies: &nearby,
                            team: p.team(),
                            party: p.party,
                        };
                        p.target = input.target;
                        step_mover(
                            &composite,
                            sheet,
                            &mut p.mover,
                            &input,
                            t,
                            dt,
                            company,
                            &mut sink,
                        );
                        actions.extend(sink.drain(..).map(|a| (t, now, a)));
                    }
                }
            }
            while allowed > 0 && executed < MAX_FRAMES_PER_TICK && p.credits >= 1.0 {
                let Some((t, input, view, claimed)) = p.queue.pop_front() else {
                    break;
                };
                p.view_claimed = claimed;
                allowed -= 1;
                p.credits -= 1.0;
                executed += 1;
                p.last_input_tick = t;
                p.executed_frames += 1;
                if p.alive && !p.ghost {
                    let sheet = &p.sheet;
                    let composite = EntityWorld {
                        world,
                        solids: &solids,
                        ignore: id,
                        own: Some(p.mover.aabb()),
                        grid: grid.as_ref(),
                    };
                    let company = Company {
                        bodies: &nearby,
                        team: p.team(),
                        party: p.party,
                    };
                    p.target = input.target;
                    step_mover(
                        &composite,
                        sheet,
                        &mut p.mover,
                        &input,
                        t,
                        dt,
                        company,
                        &mut sink,
                    );
                    actions.extend(sink.drain(..).map(|a| (t, view, a)));
                } else {
                    p.mover.yaw = input.yaw;
                    p.mover.pitch = input.pitch;
                    p.mover.buttons_prev = input.buttons;
                }
            }
            // The stacks after (MODES.md 11.2, 11.3): a reload's rounds, a kit used, and
            // a kit use begun at full health refused before it heals nothing.
            for hand in 0..2u8 {
                let before = if hand == 0 {
                    stacks_before.0
                } else {
                    stacks_before.1
                };
                let now_reserve = p.mover.guns[hand as usize].reserve;
                if now_reserve < before {
                    self.events.push(ZoneEvent::RoundsLoaded {
                        id,
                        hand,
                        rounds: before - now_reserve,
                    });
                }
            }
            // An item's use that ended in the frames (MODES.md 11.3): what its stack does,
            // a heal for now, and the word to the server (which tells the hub).
            if let Some(cell) = p.mover.used.take() {
                if p.alive {
                    let before = p.health;
                    let heals = p.bar_heals.get(cell as usize).copied().unwrap_or(0);
                    p.health = (p.health + heals.max(0)).min(p.max_health());
                    let healed = p.health - before;
                    self.events.push(ZoneEvent::ItemUsed { id, cell });
                    if healed > 0 {
                        self.events.push(ZoneEvent::Healed {
                            target: id,
                            source: id,
                            amount: healed,
                        });
                    }
                }
            }
            // A heal's use begun this step at full health is cleared (MODES.md 11.3): a
            // new one, whether or not the frames ended an earlier one first (a press as
            // one ends began the next at full health and spent a kit for nothing).
            if p.mover.use_until.is_some()
                && p.mover.use_until != stacks_before.2
                && p.bar_heals
                    .get(p.mover.use_cell as usize)
                    .copied()
                    .unwrap_or(0)
                    > 0
                && p.health >= p.max_health()
            {
                p.mover.use_until = None;
            }
            if executed == 0 && p.driver == Driver::Client {
                p.starved_ticks += 1;
            }
            if p.alive
                && let Ok(i) = solids.binary_search_by_key(&id, |(e, _)| *e)
            {
                let (old, new) = (solids[i].1, p.aabb());
                solids[i].1 = new;
                if let Some(g) = grid.as_mut() {
                    g.moved(i as u16, &old, &new);
                }
            }
            let stats = p.attacker_stats();
            for (t, view_tick, a) in actions {
                match a {
                    // Script times are in the frame's tick space; re-anchor them to server ticks.
                    Action::Swing {
                        ability,
                        step,
                        active_from,
                        active_until,
                    } => {
                        let ab = &p.sheet.kit.abilities[ability as usize];
                        let Verb::MeleeArc(arc) = &ab.steps[step as usize].verb else {
                            continue;
                        };
                        let on_hit = hit_statuses(&ab.steps[step as usize + 1..]);
                        self.swings.push(Swing {
                            attacker: id,
                            ability,
                            arc: arc.clone(),
                            stats,
                            active_from: now
                                .wrapping_add(tick_delta(active_from, t).max(0) as Tick),
                            active_until: now
                                .wrapping_add(tick_delta(active_until, t).max(0) as Tick),
                            view_tick,
                            hit: Vec::new(),
                            on_hit,
                            riposte: false,
                        });
                    }
                    Action::Fire {
                        ability,
                        step,
                        turn,
                        cone_deg,
                        headshot,
                        target,
                        ..
                    } => {
                        fires.push((
                            id,
                            ability,
                            step,
                            t,
                            view_tick,
                            Shot {
                                turn,
                                cone_deg,
                                headshot,
                                target,
                            },
                        ));
                    }
                    Action::Area {
                        ability,
                        step,
                        target,
                    } => area_spawns.push((id, ability, step, target)),
                    Action::ParryOpened => {}
                }
            }
        }
        self.history.record(
            now,
            self.players
                .values()
                .filter(|p| p.alive)
                .map(|p| (p.id, p.mover.mv.origin, p.mover.crouched))
                .collect(),
        );

        // 3: melee resolution with lag compensation, then projectile and area spawns.
        self.resolve_swings(world);
        for (owner, ability, step, input_tick, view_tick, shot) in fires {
            self.fire(world, owner, ability, step, input_tick, view_tick, shot);
        }
        for (owner, ability, step, target) in area_spawns {
            self.spawn_area_from_step(world, owner, ability, step, target);
        }

        // 4: projectiles.
        self.step_projectiles(world);

        // 5: areas pulse.
        self.pulse_areas(world);

        // 6: statuses tick (damage and healing over time).
        if now.is_multiple_of(self.dot_interval()) {
            self.pulse_dots();
        }

        // 7: respawns (a held body waits for whoever holds it).
        for &id in &ids {
            let due = self
                .players
                .get(&id)
                .is_some_and(|p| !p.alive && !p.hold && tick_delta(now, p.respawn_at) >= 0);
            if due {
                self.respawn(world, id);
            }
        }
        for p in self.players.values_mut() {
            p.anim = compute_anim(p);
            p.acting = compute_acting(p);
        }
    }

    fn resolve_swings(&mut self, world: &dyn CollisionWorld) {
        let now = self.tick;
        // Everything a landed hit needs, captured before any damage is applied: applying
        // damage can remove swings (a death) or add them (a riposte), so indices do not hold.
        struct Landed {
            target: EntityId,
            attacker: EntityId,
            packet: DamagePacket,
            stats: AttackerStats,
            parryable: bool,
            on_hit: Vec<ApplyStatus>,
            dir: Vec3,
            point: Vec3,
        }
        let mut hits: Vec<Landed> = Vec::new();
        for s in self.swings.iter_mut() {
            if tick_delta(now, s.active_from) < 0 || tick_delta(now, s.active_until) >= 0 {
                continue;
            }
            let Some(attacker) = self.players.get(&s.attacker) else {
                continue;
            };
            if !attacker.alive {
                continue;
            }
            // A stagger or shock cleared the script: the swing never lands.
            if !s.riposte
                && !attacker
                    .mover
                    .script
                    .is_some_and(|sc| sc.ability == s.ability)
            {
                continue;
            }
            let eye = attacker.mover.eye();
            let yaw = attacker.mover.yaw;
            for target in self.players.values() {
                if target.id == s.attacker || !target.alive || s.hit.contains(&target.id) {
                    continue;
                }
                if s.hit.len() >= s.arc.max_targets as usize {
                    break;
                }
                let (origin, crouched) = self
                    .history
                    .body_at(s.view_tick, target.id)
                    .unwrap_or((target.mover.mv.origin, target.mover.crouched));
                let cap = capsule_at(origin, target.mover.mv.hull, target.frame(), crouched);
                let Some(point) = melee_hit_point(eye, yaw, &s.arc, &cap) else {
                    continue;
                };
                if world.trace(Hull::Point, eye, point).fraction < 1.0 {
                    continue;
                }
                let dir = (origin - attacker.mover.mv.origin)
                    .truncate()
                    .normalize_or_zero()
                    .extend(0.0);
                let scale = s.arc.cleave_falloff.powi(s.hit.len() as i32);
                s.hit.push(target.id);
                let mut packet = s.arc.damage;
                packet.amount = ((packet.amount as f32) * scale).round() as u16;
                hits.push(Landed {
                    target: target.id,
                    attacker: s.attacker,
                    packet,
                    stats: s.stats,
                    parryable: s.arc.parryable,
                    on_hit: s.on_hit.clone(),
                    dir,
                    point,
                });
            }
        }
        for h in hits {
            let landed = self.apply_damage(
                h.target,
                h.attacker,
                &h.packet,
                h.stats,
                h.dir,
                HitKind::Melee,
                Some(h.parryable),
                Some(h.point),
            );
            if landed {
                for st in &h.on_hit {
                    self.apply_status(h.target, h.attacker, st);
                }
            }
        }
        // Keep finished swings around briefly so late diagnostics can see them.
        self.swings.retain(|s| {
            tick_delta(now, s.active_until) < 8 && self.players.contains_key(&s.attacker)
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn fire(
        &mut self,
        world: &dyn CollisionWorld,
        owner: EntityId,
        ability: u8,
        step: u8,
        input_tick: u32,
        view_tick: Tick,
        shot: Shot,
    ) {
        let Some(p) = self.players.get(&owner) else {
            return;
        };
        if !p.alive {
            return;
        }
        let ab = &p.sheet.kit.abilities[ability as usize];
        let Verb::Projectile(def) = &ab.steps[step as usize].verb else {
            return;
        };
        let def = def.clone();
        let stats = p.attacker_stats();
        let eye = p.mover.eye();
        let origin = resolve_origin(p, def.spawn, None);
        // Never spawn inside a wall: fall back to the eye when the muzzle is blocked.
        let origin = if world.trace(Hull::Point, eye, origin).fraction < 1.0 {
            eye
        } else {
            origin
        };
        let lag = tick_delta(self.tick, view_tick).clamp(0, p.max_rewind as i32) as Tick;
        // Where the bolt goes: at a target, led as a mind leads (MODES.md 5.3), when the
        // target is within the ability's range and in sight; else at the point the eye
        // ray meets, turned by the spray so far (3.3): the bolt leaves the muzzle, beside
        // and below the eye, toward the crosshair's point, so it lands there and not a
        // hand's width to its side (the director 2026-10-07, 10.2). The lead is taken
        // from where the target was at the tick the attacker saw, as the forward step
        // below spawns the bolt there: led from where the target stands now, the bolt
        // got to the point `lag` ticks before the target did and missed every walker
        // (the director 2026-10-08, 10.3).
        let led = self.lead_at(
            world,
            p,
            shot.target,
            ab.range,
            origin,
            def.speed,
            def.gravity_scale,
            self.tick.wrapping_sub(lag),
        );
        let dir = match led {
            Some(d) => d,
            None => {
                let (ky, kp) = shot.turn;
                let look =
                    crate::sim::view_dir(p.mover.yaw + ky, (p.mover.pitch - kp).clamp(-89.0, 89.0));
                let point = self.eye_ray_point(world, p, eye, look, CONVERGE_RANGE);
                (point - origin).normalize_or_zero()
            }
        };
        let dir = if dir.length_squared() < 0.5 {
            p.mover.view_dir()
        } else {
            dir
        };
        let owner_vel = p.mover.mv.velocity;
        let view_lag =
            tick_delta(self.tick, p.view_claimed).clamp(0, MAX_CLAIMED_VIEW_LAG as i32) as Tick;
        let spread_deg = def.spread_deg + shot.cone_deg;
        for _ in 0..def.count.max(1) {
            let shot_dir = if spread_deg > 0.0 {
                let a = self.rng.range_f32(0.0, core::f32::consts::TAU);
                let r = self.rng.next_f32().sqrt() * spread_deg.to_radians();
                let side = dir.cross(Vec3::Z).normalize_or_zero();
                let side = if side.length_squared() < 0.5 {
                    Vec3::X
                } else {
                    side
                };
                let up2 = side.cross(dir).normalize_or_zero();
                (dir + (side * a.cos() + up2 * a.sin()) * r.tan()).normalize()
            } else {
                dir
            };
            let id = self.alloc_id();
            let mut proj = Projectile {
                id,
                owner,
                ability,
                input_tick,
                stats,
                pos: origin,
                vel: shot_dir * def.speed + owner_vel * def.inherit_velocity,
                spawned: self.tick,
                // The forward step below consumes `lag` ticks of the lifetime up front.
                dies: self
                    .tick
                    .wrapping_add(def.lifetime.saturating_sub(lag).max(1)),
                pierce_left: def.pierce,
                bounces_left: def.bounce.count,
                hit: Vec::new(),
                headshot: shot.headshot,
                def: def.clone(),
            };
            self.events.push(ZoneEvent::ProjectileSpawned {
                id,
                owner,
                input_tick,
                lag,
                view_lag,
                speed: def.speed,
                gravity: def.gravity_scale,
                lifetime: def.lifetime,
                origin,
            });
            // Forward step (PROTOCOL.md 7.4): the projectile exists at the time the attacker saw,
            // and each caught-up tick is swept against where the targets *were* at that tick, so
            // faking latency buys nothing but stale targets.
            let mut alive = true;
            for i in 0..lag {
                let at = self.tick.wrapping_sub(lag - i);
                if !self.step_projectile(world, &mut proj, Some(at)) {
                    alive = false;
                    break;
                }
            }
            if alive {
                self.projectiles.push(proj);
            } else {
                self.events.push(ZoneEvent::ProjectileRemoved(id));
            }
        }
    }

    fn step_projectiles(&mut self, world: &dyn CollisionWorld) {
        let mut projectiles = std::mem::take(&mut self.projectiles);
        let mut keep = Vec::with_capacity(projectiles.len());
        for mut proj in projectiles.drain(..) {
            if self.step_projectile(world, &mut proj, None) {
                keep.push(proj);
            } else {
                self.events.push(ZoneEvent::ProjectileRemoved(proj.id));
            }
        }
        // Projectiles spawned by expiries during the loop (none in v1) would go here.
        self.projectiles = keep;
    }

    /// One tick of one projectile. Returns `false` when it is gone. With `rewind_to`, target
    /// capsules come from the position history at that tick (forward steps at spawn).
    fn step_projectile(
        &mut self,
        world: &dyn CollisionWorld,
        proj: &mut Projectile,
        rewind_to: Option<Tick>,
    ) -> bool {
        let now = self.tick;
        if tick_delta(now, proj.dies) >= 0 {
            let at = proj.pos;
            let def = proj.def.clone();
            self.run_triggered(&def.on_expire, proj.owner, proj.stats, at, None);
            return false;
        }
        let dt = self.rate.dt();
        proj.vel.z -= MoveVars::QUAKE.gravity * proj.def.gravity_scale * dt;
        if proj.def.drag > 0.0 {
            proj.vel *= (1.0 - proj.def.drag * dt).max(0.0);
        }
        let start = proj.pos;
        let end = start + proj.vel * dt;
        let world_hit = world.trace(Hull::Point, start, end);
        let mut best: Option<(f32, EntityId)> = None;
        let grace = tick_delta(now, proj.spawned) < PROJECTILE_OWNER_GRACE as i32;
        for target in self.players.values() {
            if !target.alive || proj.hit.contains(&target.id) {
                continue;
            }
            if target.id == proj.owner && grace {
                continue;
            }
            let cap = match rewind_to.and_then(|at| self.history.body_at(at, target.id)) {
                Some((origin, crouched)) => {
                    capsule_at(origin, target.mover.mv.hull, target.frame(), crouched)
                }
                None => target.capsule(),
            };
            if let Some(t) = sweep_sphere_capsule(start, end, proj.def.radius, &cap)
                && t <= world_hit.fraction
                && best.is_none_or(|(bt, _)| t < bt)
            {
                best = Some((t, target.id));
            }
        }
        match best {
            Some((t, target)) => {
                proj.pos = start + (end - start) * t;
                proj.hit.push(target);
                let dir = proj.vel.normalize_or_zero();
                let owner = proj.owner;
                let stats = proj.stats;
                let def = proj.def.clone();
                // The head band (MODES.md 3.5): a bolt that enters the hull within the top
                // of it is multiplied before armour.
                let mut packet = def.damage;
                if proj.headshot > 1.0
                    && let Some(tp) = self.players.get(&target)
                {
                    let cap = match rewind_to.and_then(|at| self.history.body_at(at, target)) {
                        Some((origin, crouched)) => {
                            capsule_at(origin, tp.mover.mv.hull, tp.frame(), crouched)
                        }
                        None => tp.capsule(),
                    };
                    let top = cap.b.z.max(cap.a.z) + cap.radius;
                    if proj.pos.z >= top - HEAD_BAND {
                        packet.amount = ((packet.amount as f32) * proj.headshot)
                            .round()
                            .min(u16::MAX as f32) as u16;
                    }
                }
                let landed = self.apply_damage(
                    target,
                    owner,
                    &packet,
                    stats,
                    dir,
                    HitKind::Projectile,
                    None,
                    Some(proj.pos),
                );
                if landed {
                    self.run_triggered(&def.on_hit, owner, stats, proj.pos, Some(target));
                }
                if proj.pierce_left == 0 {
                    return false;
                }
                proj.pierce_left -= 1;
                proj.pos = end;
                true
            }
            None => {
                if world_hit.start_solid {
                    return false;
                }
                if world_hit.fraction < 1.0 {
                    if proj.bounces_left > 0 && proj.def.bounce.restitution > 0.0 {
                        proj.bounces_left -= 1;
                        proj.pos = world_hit.end;
                        let n = world_hit.plane_normal;
                        proj.vel =
                            (proj.vel - n * (2.0 * proj.vel.dot(n))) * proj.def.bounce.restitution;
                        return true;
                    }
                    let at = world_hit.end;
                    let def = proj.def.clone();
                    if def.speed >= BULLET_SPEED {
                        self.events.push(ZoneEvent::Impact {
                            at,
                            normal: world_hit.plane_normal,
                        });
                    }
                    self.run_triggered(&def.on_hit, proj.owner, proj.stats, at, None);
                    return false;
                }
                proj.pos = end;
                true
            }
        }
    }

    /// `on_hit` / `on_expire` triggers: statuses on the hit entity or the actor, areas at
    /// the impact point.
    fn run_triggered(
        &mut self,
        triggers: &[Trigger],
        owner: EntityId,
        stats: AttackerStats,
        impact: Vec3,
        hit: Option<EntityId>,
    ) {
        for t in triggers {
            match t {
                Trigger::Status(s) => match s.target {
                    StatusTarget::Actor => self.apply_status(owner, owner, s),
                    StatusTarget::Hit | StatusTarget::Area | StatusTarget::Allies => {
                        if let Some(h) = hit {
                            self.apply_status(h, owner, s);
                        }
                    }
                },
                Trigger::Area(ae) => {
                    let (origin, dir) = match self.players.get(&owner) {
                        Some(p) => (
                            resolve_origin(p, ae.origin, Some(impact)),
                            p.mover.view_dir(),
                        ),
                        None => (impact, Vec3::X),
                    };
                    self.spawn_area(owner, 0, ae.clone(), stats, origin, dir);
                }
            }
        }
    }

    fn spawn_area_from_step(
        &mut self,
        world: &dyn CollisionWorld,
        owner: EntityId,
        ability: u8,
        step: u8,
        target: u32,
    ) {
        let Some(p) = self.players.get(&owner) else {
            return;
        };
        if !p.alive {
            return;
        }
        let ab = &p.sheet.kit.abilities[ability as usize];
        let Verb::AreaEffect(ae) = &ab.steps[step as usize].verb else {
            return;
        };
        let ae = ae.clone();
        let stats = p.attacker_stats();
        // An aimed area goes under its target (MODES.md 5.3) when the target is within
        // the ability's range and in sight; else where the actor looks.
        let under = match (ae.origin, self.target_in_range(world, p, target, ab.range)) {
            (Origin::Aim { .. }, Some(t)) => {
                let feet = t.mover.mv.origin + Vec3::new(0.0, 0.0, t.mover.mv.hull.mins().z);
                let down = world.trace(Hull::Point, feet + Vec3::Z * 8.0, feet - Vec3::Z * 1024.0);
                Some(if down.start_solid { feet } else { down.end })
            }
            _ => None,
        };
        let origin = match (ae.origin, under) {
            (_, Some(at)) => at,
            (Origin::Aim { range }, None) => self.aim_point(world, p, range),
            (other, None) => resolve_origin(p, other, None),
        };
        let dir = p.mover.view_dir();
        self.spawn_area(owner, ability, ae, stats, origin, dir);
    }

    /// The body `target` if it is alive, another than `p`, within `range` of `p` and in
    /// `p`'s sight (MODES.md 5.3).
    fn target_in_range(
        &self,
        world: &dyn CollisionWorld,
        p: &Player,
        target: EntityId,
        range: f32,
    ) -> Option<&Player> {
        if target == 0 || target == p.id || range <= 0.0 {
            return None;
        }
        let t = self.players.get(&target)?;
        if !t.alive {
            return None;
        }
        let centre = t.capsule().center();
        if (centre - p.mover.mv.origin).truncate().length() > range {
            return None;
        }
        (world.trace(Hull::Point, p.mover.eye(), centre).fraction >= 1.0).then_some(t)
    }

    /// The direction from `origin` to where `target` will be when a bolt of `speed` and
    /// `gravity` let go at tick `at` gets there (MODES.md 5.3: the lead a mind takes), or
    /// `None` without a target in range. The target is taken where the history has it at
    /// `at` (where it stands now when the history does not reach), moving as it does now.
    #[allow(clippy::too_many_arguments)]
    fn lead_at(
        &self,
        world: &dyn CollisionWorld,
        p: &Player,
        target: EntityId,
        range: f32,
        origin: Vec3,
        speed: f32,
        gravity: f32,
        at: Tick,
    ) -> Option<Vec3> {
        let t = self.target_in_range(world, p, target, range)?;
        let centre = match self.history.body_at(at, t.id) {
            Some((o, crouched)) => capsule_at(o, t.mover.mv.hull, t.frame(), crouched).center(),
            None => t.capsule().center(),
        };
        let point = crate::sim::aim::lead(origin, centre, t.mover.mv.velocity, speed, gravity);
        Some((point - origin).normalize_or_zero())
    }

    /// `Origin::Aim` (VOCABULARY.md 4): the first body or world surface along the actor's view
    /// ray within `range`, dropped to the ground; under a body, its feet. Current positions:
    /// what is placed is a spot on the floor, and it does not follow anyone.
    /// The first point the ray from `eye` along `dir` meets within `range`: the world or
    /// another living body; `range` out when it meets nothing (MODES.md 3.3).
    fn eye_ray_point(
        &self,
        world: &dyn CollisionWorld,
        p: &Player,
        eye: Vec3,
        dir: Vec3,
        range: f32,
    ) -> Vec3 {
        let tr = world.trace(Hull::Point, eye, eye + dir * range);
        let mut reach = range * tr.fraction;
        for o in self.players.values() {
            if o.id == p.id || !o.alive {
                continue;
            }
            if let Some(t) = ray_capsule(eye, dir, reach, &o.capsule()) {
                reach = t;
            }
        }
        eye + dir * reach
    }

    pub fn aim_point(&self, world: &dyn CollisionWorld, p: &Player, range: f32) -> Vec3 {
        let eye = p.mover.eye();
        let dir = p.mover.view_dir();
        let tr = world.trace(Hull::Point, eye, eye + dir * range);
        let mut reach = range * tr.fraction;
        let mut body: Option<Vec3> = None;
        for o in self.players.values() {
            if o.id == p.id || !o.alive {
                continue;
            }
            if let Some(t) = ray_capsule(eye, dir, reach, &o.capsule()) {
                reach = t;
                body = Some(o.mover.mv.origin + Vec3::new(0.0, 0.0, o.mover.mv.hull.mins().z));
            }
        }
        // From just above the feet, or from a little short of the surface the ray met.
        let from = match body {
            Some(feet) => feet + Vec3::Z * 8.0,
            None => eye + dir * (reach - 4.0).max(0.0),
        };
        let down = world.trace(Hull::Point, from, from - Vec3::Z * 1024.0);
        if down.start_solid { from } else { down.end }
    }

    fn spawn_area(
        &mut self,
        owner: EntityId,
        ability: u8,
        def: AreaEffect,
        stats: AttackerStats,
        origin: Vec3,
        dir: Vec3,
    ) {
        let id = self.alloc_id();
        let now = self.tick;
        let ends = now.wrapping_add(def.delay).wrapping_add(def.duration);
        self.areas.push(Area {
            id,
            owner,
            ability,
            next_pulse: now.wrapping_add(def.delay),
            ends,
            pulses: 0,
            def,
            stats,
            origin,
            dir,
        });
        self.events.push(ZoneEvent::AreaSpawned { id, owner });
    }

    fn pulse_areas(&mut self, world: &dyn CollisionWorld) {
        let now = self.tick;
        let echo = self.rate.ms_to_ticks(INSTANT_AREA_ECHO_MS);
        let mut areas = std::mem::take(&mut self.areas);
        let mut keep = Vec::with_capacity(areas.len());
        for mut area in areas.drain(..) {
            let instant = area.def.duration == 0;
            // An instant area pulses once; it then stays, spent, for its echo (PROTOCOL.md
            // 5): spawned, pulsed and gone within one tick it would be in no snapshot, and
            // a shockwave nobody sees cannot be read. `ends` is then when the echo ends.
            if tick_delta(now, area.next_pulse) >= 0 && !(instant && area.pulses > 0) {
                self.pulse_area(world, &area);
                area.pulses += 1;
                area.next_pulse = area.next_pulse.wrapping_add(area.def.interval.max(1));
                if instant {
                    area.ends = now.wrapping_add(echo);
                }
            }
            let done = instant && area.pulses > 0 && tick_delta(now, area.ends) >= 0
                || area.def.duration > 0 && tick_delta(area.next_pulse, area.ends) > 0;
            if done {
                self.events.push(ZoneEvent::AreaRemoved(area.id));
            } else {
                keep.push(area);
            }
        }
        self.areas = keep;
    }

    fn pulse_area(&mut self, world: &dyn CollisionWorld, area: &Area) {
        // Targets: capsules overlapping the shape, nearest first, bounded by max_targets.
        let mut targets: Vec<(f32, EntityId)> = Vec::new();
        for p in self.players.values() {
            if !p.alive || (area.def.exclude_actor && p.id == area.owner) {
                continue;
            }
            let cap = p.capsule();
            let Some(norm) = shape_overlap(&area.def.shape, area.origin, area.dir, &cap) else {
                continue;
            };
            if area.def.requires_los
                && world.trace(Hull::Point, area.origin, cap.center()).fraction < 1.0
            {
                continue;
            }
            targets.push((norm, p.id));
        }
        targets.sort_by(|a, b| a.0.total_cmp(&b.0));
        if area.def.max_targets > 0 {
            targets.truncate(area.def.max_targets as usize);
        }
        for (norm, id) in targets {
            if let Some(packet) = area.def.damage {
                let falloff = match area.def.falloff {
                    Falloff::None => 1.0,
                    Falloff::Linear => (1.0 - norm).clamp(0.0, 1.0),
                    Falloff::InverseSquare => (1.0 - norm).clamp(0.0, 1.0).powi(2),
                };
                let mut packet = packet;
                packet.amount = ((packet.amount as f32) * falloff).round() as u16;
                if packet.amount > 0 {
                    let dir = (self.players[&id].mover.mv.origin - area.origin)
                        .truncate()
                        .normalize_or_zero()
                        .extend(0.0);
                    self.apply_damage(
                        id,
                        area.owner,
                        &packet,
                        area.stats,
                        dir,
                        HitKind::Area,
                        None,
                        None,
                    );
                }
            }
            for s in &area.def.effects {
                match s.target {
                    StatusTarget::Actor => self.apply_status(area.owner, area.owner, s),
                    StatusTarget::Hit | StatusTarget::Area | StatusTarget::Allies => {
                        self.apply_status(id, area.owner, s)
                    }
                }
            }
        }
    }

    /// Bleed, Burn and Regen pulses (MATRIX.md 8), four per second.
    fn pulse_dots(&mut self) {
        let ids: Vec<EntityId> = self.players.keys().copied().collect();
        for id in ids {
            let Some(p) = self.players.get(&id) else {
                continue;
            };
            if !p.alive {
                continue;
            }
            let slots: Vec<_> = p.mover.statuses.active().copied().collect();
            // Fractional magnitudes accumulate across the four pulses of a second so that a
            // magnitude of m deals exactly floor(m) per second, never 0 and never rounded up.
            let pulse = (self.tick / self.dot_interval()) % DOT_PULSES_PER_S;
            for slot in slots {
                let per = slot.magnitude / DOT_PULSES_PER_S as f32;
                let amount =
                    (per * (pulse + 1) as f32).floor() as i32 - (per * pulse as f32).floor() as i32;
                let source = slot.source;
                let stats = self
                    .players
                    .get(&source)
                    .map_or(AttackerStats::NEUTRAL, |s| s.attacker_stats());
                match slot.status {
                    Some(Status::Bleed) | Some(Status::Burn) if amount > 0 => {
                        let dtype = if slot.status == Some(Status::Bleed) {
                            DamageType::Pierce
                        } else {
                            DamageType::Fire
                        };
                        let packet = DamagePacket {
                            amount: amount as u16,
                            dtype,
                            bypass: if dtype == DamageType::Pierce {
                                Bypass::ARMOR
                            } else {
                                Bypass::NONE
                            },
                            knockback: 0.0,
                            stagger: 0,
                        };
                        self.apply_damage(
                            id,
                            source,
                            &packet,
                            stats,
                            Vec3::ZERO,
                            HitKind::Dot,
                            None,
                            None,
                        );
                    }
                    Some(Status::Regen) if amount > 0 => {
                        if let Some(p) = self.players.get_mut(&id) {
                            let before = p.health;
                            p.health = (p.health + amount).min(p.max_health());
                            let healed = p.health - before;
                            if healed > 0 {
                                self.events.push(ZoneEvent::Healed {
                                    target: id,
                                    source,
                                    amount: healed,
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Put a status on `target` (MATRIX.md 8), scaling the duration by the target's factor.
    pub fn apply_status(&mut self, target: EntityId, source: EntityId, s: &ApplyStatus) {
        // A taunt is for enemies (MATRIX.md 8): an ally in the roar is not turned by it. A
        // status for `Allies` (VOCABULARY.md 5.4) is for the source's own side, the source
        // among them.
        if s.status == Status::Taunt || s.target == StatusTarget::Allies {
            let (Some(src), Some(t)) = (self.players.get(&source), self.players.get(&target))
            else {
                return;
            };
            let enemy = src.team() != t.team() || (t.team() == TEAM_WILD && src.party != t.party);
            if enemy == (s.target == StatusTarget::Allies) {
                return;
            }
        }
        let Some(t) = self.players.get_mut(&target) else {
            return;
        };
        if !t.alive {
            return;
        }
        let factor = if s.status.fixed_duration() {
            1.0
        } else {
            t.sheet.derived.status_duration
        };
        let mut duration = ((s.duration as f32) * factor).round().max(1.0) as Tick;
        let now = t.last_input_tick;
        // Diminishing returns on controls (MODES.md 4.5): the second within ten seconds
        // lasts half, the third does nothing and the body is immune for ten seconds.
        if s.status.is_control() {
            let kind = match s.status {
                Status::Knockdown => 0,
                Status::Launched => 1,
                Status::Taunt => 3,
                _ => 2,
            };
            let window = self.rate.ms_to_ticks(CONTROL_WINDOW_MS);
            let (count, last) = t.controls[kind];
            let count = if tick_delta(self.tick, last) > window as i32 {
                0
            } else {
                count
            };
            match count {
                0 => {}
                1 => duration = (duration / 2).max(1),
                _ => {
                    t.controls[kind] = (count, self.tick);
                    return;
                }
            }
            t.controls[kind] = (count + 1, self.tick);
        }
        let outcome = t.mover.statuses.apply(s, duration, now, source);
        if matches!(
            outcome,
            crate::status::Applied::Immune | crate::status::Applied::NoRoom
        ) {
            return;
        }
        if s.status == Status::Stagger || s.status == Status::Shock || s.status.downs() {
            t.mover.script = None;
            t.mover.dash = None;
            t.mover.guard = GuardState::None;
            t.mover.lock_yaw = None;
            t.mover.chain = None;
            self.swings.retain(|sw| sw.attacker != target || sw.riposte);
            if s.status == Status::Launched {
                // The lift (MODES.md 4.5): the magnitude is the velocity up.
                t.mover.mv.velocity.z += s.magnitude.max(0.0);
                t.mover.mv.on_ground = false;
            }
        } else if t.mover.script.is_some_and(|sc| {
            t.sheet.kit.abilities[sc.ability as usize].interrupt == Interrupt::OnStagger
        }) && s.status == Status::Stagger
        {
            t.mover.script = None;
        }
        self.events.push(ZoneEvent::StatusApplied {
            target,
            status: s.status,
            source,
        });
    }

    /// The damage pipeline of MATRIX.md 7 for one packet. `parryable` is `Some` for melee
    /// (whether the swing can be parried); projectiles pass `None` (never parried, blocked only
    /// by shields); areas and DoTs ignore guards. Returns whether the packet landed.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    fn apply_damage(
        &mut self,
        target: EntityId,
        attacker: EntityId,
        packet: &DamagePacket,
        stats: AttackerStats,
        dir: Vec3,
        kind: HitKind,
        parryable: Option<bool>,
        at: Option<Vec3>,
    ) -> bool {
        let now = self.tick;
        let respawn_ticks = self.respawn_ticks;
        let attacker_origin = self.players.get(&attacker).map(|a| a.mover.mv.origin);
        let Some(t) = self.players.get_mut(&target) else {
            return false;
        };
        if !t.alive || t.unhurt {
            return false;
        }
        let frame_now = t.last_input_tick;
        if t.mover.invulnerable(frame_now) {
            return false;
        }
        // A packet of amount 0 is not an attack (MATRIX.md 7): it lands for its triggers and
        // does nothing else. Nothing guards against it and it interrupts nothing.
        if packet.amount == 0 {
            return true;
        }
        // Guard (VOCABULARY.md 5.6): facing test against the attacker for melee, against
        // where the shot came from for projectiles (the shooter may have moved since).
        let guard = t.sheet.kit.guard_verb().cloned();
        let guardable = matches!(kind, HitKind::Melee | HitKind::Projectile);
        let toward = match (kind, attacker_origin) {
            (HitKind::Melee, Some(from)) => (from - t.mover.mv.origin).truncate(),
            _ => -dir.truncate(),
        };
        let (fwd, _) = yaw_vectors(t.mover.yaw);
        let facing_deg = if toward.length_squared() > 1e-6 {
            fwd.truncate()
                .dot(toward.normalize())
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees()
        } else {
            0.0
        };
        let mut block = None;
        let mut guard_broke = false;
        if guardable {
            match (t.mover.guard, &guard) {
                (GuardState::Block, Some(Guard::Block(b)))
                    if facing_deg <= b.arc_deg * 0.5
                        && (kind == HitKind::Melee || b.stops_projectiles) =>
                {
                    let cost = b.stamina_per_hit as f32;
                    if t.mover.stamina >= cost {
                        t.mover.stamina -= cost;
                        t.mover.regen_pause_until = frame_now
                            .wrapping_add(self.rate.ms_to_ticks(crate::sim::REGEN_PAUSE_MS));
                        block = Some(b.mitigation);
                    } else {
                        t.mover.stamina = 0.0;
                        t.mover.regen_pause_until = frame_now
                            .wrapping_add(self.rate.ms_to_ticks(crate::sim::REGEN_PAUSE_MS));
                        t.mover.guard = GuardState::None;
                        guard_broke = true;
                    }
                }
                (GuardState::Parry { until }, Some(Guard::Parry(p)))
                    if tick_delta(frame_now, until) < 0
                        && facing_deg <= p.arc_deg * 0.5
                        && parryable == Some(true) =>
                {
                    t.mover.guard = GuardState::None;
                    let on_success = p.on_success.clone();
                    let defender_stats = t.attacker_stats();
                    self.events.push(ZoneEvent::Parried {
                        defender: target,
                        attacker,
                    });
                    // Riposte and statuses land on the attacker.
                    for r in on_success {
                        match r {
                            Riposte::Status(s) => self.apply_status(attacker, target, &s),
                            Riposte::Swing(arc) => {
                                self.swings.push(Swing {
                                    attacker: target,
                                    ability: 0,
                                    active_from: now.wrapping_add(arc.timing.windup),
                                    active_until: now
                                        .wrapping_add(arc.timing.windup + arc.timing.active),
                                    arc,
                                    stats: defender_stats,
                                    view_tick: now,
                                    hit: Vec::new(),
                                    on_hit: Vec::new(),
                                    riposte: true,
                                });
                            }
                        }
                    }
                    return false;
                }
                _ => {}
            }
        }
        // The hit lands: both are in a fight from now.
        if let Some(a) = self.players.get_mut(&attacker) {
            a.fought_at = Some(now);
        }
        // The pulses of a status take no gear on either side (ITEMS.md 3.1): a pulse is a
        // point or two, and a factor on that is a step, not an edge.
        let geared = kind != HitKind::Dot;
        let stats = if geared {
            stats
        } else {
            AttackerStats {
                gear: [0; 9],
                ..stats
            }
        };
        let t = self.players.get_mut(&target).expect("still there");
        let d = &t.sheet.derived;
        let defender = DefenderStats {
            armour_class: t.sheet.build.armour,
            aspects: t.sheet.build.aspects,
            armour: d.armour,
            ward: d.ward,
            evasion: d.evasion,
            fortify: t.mover.statuses.magnitude(Status::Fortify),
            evading: t.mover.evading(frame_now),
            exposed: t.mover.statuses.has(Status::Expose),
            block,
            gear: if geared { t.gear.taken } else { [0; 9] },
        };
        let amount = resolve_damage(packet, &stats, &defender);
        let absorbed = if block.is_some() {
            let unblocked = DefenderStats {
                block: None,
                ..defender
            };
            (resolve_damage(packet, &stats, &unblocked) - amount).max(0)
        } else {
            0
        };
        t.health -= amount;
        t.last_hit_tick = now;
        t.fought_at = Some(now);
        let knock = dir * packet.knockback * stats.knockback_dealt * d.knockback_taken;
        t.mover.mv.velocity += knock;
        if knock.z > 0.0 {
            t.mover.mv.on_ground = false;
        }
        if t.mover.script.is_some_and(|sc| {
            t.sheet.kit.abilities[sc.ability as usize].interrupt == Interrupt::OnDamage
        }) {
            t.mover.script = None;
        }
        self.events.push(ZoneEvent::Hit {
            attacker,
            target,
            amount,
            kind,
            absorbed,
            at: at.unwrap_or_else(|| t.capsule().center()),
        });
        // Stagger build-up (guard mitigation does not reduce it), immunity window.
        let mut stagger = guard_broke;
        if packet.stagger > 0 && tick_delta(frame_now, t.mover.statuses.stagger_immune_until) >= 0 {
            t.stagger += packet.stagger as f32;
            if t.stagger >= d.stagger_threshold {
                t.stagger = 0.0;
                stagger = true;
            }
        }
        if guard_broke {
            self.events.push(ZoneEvent::GuardBroken(target));
        }
        if t.health <= 0 {
            t.health = 0;
            t.alive = false;
            t.deaths += 1;
            t.respawn_at = now.wrapping_add(respawn_ticks);
            t.mover.reset_actions();
            t.mover.mv.velocity = Vec3::ZERO;
            t.stagger = 0.0;
            self.swings.retain(|s| s.attacker != target);
            if attacker != target
                && let Some(a) = self.players.get_mut(&attacker)
            {
                a.kills += 1;
            }
            self.events.push(ZoneEvent::Killed {
                victim: target,
                killer: attacker,
            });
            return true;
        }
        if stagger {
            let rate = self.rate;
            let verb = ApplyStatus {
                status: Status::Stagger,
                duration: rate.ms_to_ticks(STAGGER_MS),
                magnitude: 1.0,
                max_stacks: 1,
                stacking: StackRule::Refresh,
                target: StatusTarget::Hit,
                dispellable: false,
            };
            self.apply_status(target, attacker, &verb);
            if let Some(t) = self.players.get_mut(&target) {
                t.mover.statuses.stagger_immune_until = frame_now
                    .wrapping_add(verb.duration)
                    .wrapping_add(rate.ms_to_ticks(STAGGER_IMMUNITY_MS));
                self.events.push(ZoneEvent::Staggered(target));
            }
        }
        true
    }

    fn respawn(&mut self, world: &dyn CollisionWorld, id: EntityId) {
        let team = self.players.get(&id).map_or(0, |p| p.team());
        let (origin, yaw) = self.free_spawn(world, Hull::Player, team);
        let content = &self.content;
        let Some(p) = self.players.get_mut(&id) else {
            return;
        };
        if let Some(build) = p.pending_build.take() {
            p.sheet = Sheet::new(build, content, p.team());
        }
        let cooldowns = p.mover.cooldowns;
        // The rounds in the guns and the item bar's stacks come back with the body
        // (MODES.md 11.2): a respawn refills nothing.
        let (guns, bar) = (p.mover.guns, p.mover.bar);
        p.mover = Mover::spawn(origin, yaw, &p.sheet);
        p.mover.cooldowns = cooldowns;
        p.mover.bar = bar;
        for (g, kept) in p.mover.guns.iter_mut().zip(guns) {
            g.magazine = kept.magazine.min(g.magazine);
            g.reserve = kept.reserve;
        }
        p.health = p.sheet.derived.health;
        p.alive = true;
        p.stagger = 0.0;
        self.events.push(ZoneEvent::Respawned(id));
    }

    /// A spawn point of `team` (0 = any; falls back to any team's points), with small offsets
    /// when occupied, where the hull is in open space and overlaps no living player.
    fn free_spawn(&mut self, world: &dyn CollisionWorld, hull: Hull, team: u8) -> (Vec3, f32) {
        let mut candidates: Vec<Spawn> = self
            .spawns
            .iter()
            .filter(|s| team == 0 || s.team == team || s.team == 0)
            .copied()
            .collect();
        if candidates.is_empty() {
            candidates = self.spawns.clone();
        }
        let n = candidates.len();
        let start = self.rng.below(n as u32) as usize;
        // The point itself, the eight places round it, then the sixteen round those: a
        // crowd arriving at once (squads come four bodies to a player) still finds room.
        let mut offsets: Vec<Vec3> = vec![
            Vec3::ZERO,
            Vec3::new(40.0, 0.0, 0.0),
            Vec3::new(-40.0, 0.0, 0.0),
            Vec3::new(0.0, 40.0, 0.0),
            Vec3::new(0.0, -40.0, 0.0),
            Vec3::new(40.0, 40.0, 0.0),
            Vec3::new(-40.0, -40.0, 0.0),
            Vec3::new(40.0, -40.0, 0.0),
            Vec3::new(-40.0, 40.0, 0.0),
        ];
        for a in -2..=2i32 {
            for b in -2..=2i32 {
                if a.abs() == 2 || b.abs() == 2 {
                    offsets.push(Vec3::new(a as f32 * 40.0, b as f32 * 40.0, 0.0));
                }
            }
        }
        for &off in &offsets {
            for i in 0..n {
                let s = candidates[(start + i) % n];
                let origin = s.origin + off;
                if world.point_contents(hull, origin) != Contents::Empty {
                    continue;
                }
                let bb = Aabb::around(origin, hull);
                if self
                    .players
                    .values()
                    .any(|p| p.alive && p.aabb().overlaps(&bb))
                {
                    continue;
                }
                return (origin, s.yaw);
            }
        }
        let s = candidates[start];
        (s.origin, s.yaw)
    }
}

/// `Hit`-targeted statuses among the steps that follow a verb, up to the next hitting verb.
fn hit_statuses(following: &[crate::vocab::Step]) -> Vec<ApplyStatus> {
    let mut out = Vec::new();
    for step in following {
        match &step.verb {
            Verb::ApplyStatus(s) if s.target == StatusTarget::Hit => out.push(*s),
            Verb::MeleeArc(_) | Verb::Projectile(_) | Verb::AreaEffect(_) => break,
            _ => {}
        }
    }
    out
}

/// Where a verb anchors, for the actor `p` (VOCABULARY.md 4).
fn resolve_origin(p: &Player, origin: Origin, impact: Option<Vec3>) -> Vec3 {
    let eye = p.mover.eye();
    match origin {
        Origin::SelfFeet => p.mover.mv.origin + Vec3::new(0.0, 0.0, p.mover.mv.hull.mins().z),
        Origin::SelfEyes => eye,
        Origin::Weapon { offset } => {
            let (fwd, right) = yaw_vectors(p.mover.yaw);
            eye + fwd * offset[0] + right * offset[1] + Vec3::Z * offset[2]
        }
        Origin::Point(pt) => Vec3::from(pt),
        // An aim is resolved against the world and the bodies (`Zone::aim_point`); where that
        // is not possible (a trigger), the impact or the eyes stand in.
        Origin::Aim { .. } | Origin::Impact => impact.unwrap_or(eye),
    }
}

/// Whether a capsule overlaps a shape at `origin` facing `dir`; returns the normalised
/// distance (0 at the centre, 1 at the edge) for falloff.
fn shape_overlap(shape: &Shape, origin: Vec3, dir: Vec3, cap: &Capsule) -> Option<f32> {
    let center = cap.center();
    match *shape {
        Shape::Sphere { radius } => {
            let d = (cap.closest_axis_point(origin) - origin).length() - cap.radius;
            (d <= radius).then(|| ((center - origin).length() / radius.max(1e-3)).min(1.0))
        }
        Shape::Cylinder { radius, height } => {
            let horiz = (center - origin).truncate().length() - cap.radius;
            let z_lo = cap.a.z - cap.radius;
            let z_hi = cap.b.z + cap.radius;
            let overlap = horiz <= radius && z_hi >= origin.z && z_lo <= origin.z + height;
            overlap.then(|| ((center - origin).truncate().length() / radius.max(1e-3)).min(1.0))
        }
        Shape::Cone {
            length,
            half_angle_deg,
        } => {
            let to = center - origin;
            let dist = to.length();
            if dist - cap.radius > length || dist < 1e-3 {
                return (dist < 1e-3).then_some(0.0);
            }
            let angle = dir
                .normalize_or_zero()
                .dot(to / dist)
                .clamp(-1.0, 1.0)
                .acos()
                .to_degrees();
            let allowance = (cap.radius / dist).min(1.0).asin().to_degrees();
            (angle - allowance <= half_angle_deg).then(|| (dist / length.max(1e-3)).min(1.0))
        }
        Shape::Box { half_extents } => {
            let he = Vec3::from(half_extents);
            let bb = Aabb::new(origin - he, origin + he);
            let cb = Aabb::new(
                cap.a - Vec3::splat(cap.radius),
                cap.b + Vec3::splat(cap.radius),
            );
            bb.overlaps(&cb)
                .then(|| ((center - origin).length() / he.length().max(1e-3)).min(1.0))
        }
    }
}

/// The stance of a body `elapsed` ticks into the script of `ability`: a swing's windup,
/// its active ticks and its recovery, or a cast. The zone says it of every body; a client
/// says it of its own from its prediction, a round trip sooner (LOOK.md 13).
pub fn script_anim(ability: &crate::vocab::Ability, elapsed: Tick) -> u8 {
    if let Some(crate::vocab::Step {
        verb: Verb::MeleeArc(arc),
        at,
    }) = ability.steps.first()
    {
        let windup_end = at + arc.timing.windup;
        let active_end = windup_end + arc.timing.active;
        return if elapsed < windup_end {
            anim::WINDUP
        } else if elapsed < active_end {
            anim::SWING
        } else {
            anim::RECOVER
        };
    }
    anim::CAST
}

/// The ability a stance belongs to (`Player::acting`): the running script's, while the
/// stance is the script's own.
fn compute_acting(p: &Player) -> u16 {
    if !anim::acts(p.anim) {
        return 0;
    }
    p.mover
        .script
        .and_then(|s| p.sheet.kit.abilities.get(s.ability as usize))
        .map_or(0, |a| a.id.0)
}

fn compute_anim(p: &Player) -> u8 {
    if !p.alive {
        return anim::DEAD;
    }
    if p.mover.statuses.downed() {
        return anim::DOWN;
    }
    if p.mover.statuses.staggered() {
        return anim::STAGGER;
    }
    if p.mover.reloading(p.last_input_tick) {
        return anim::RELOAD;
    }
    if p.mover.using_item(p.last_input_tick).is_some() {
        return anim::USE;
    }
    if p.mover.commanding(p.last_input_tick) {
        return anim::COMMAND;
    }
    if p.mover.dash.is_some() {
        return anim::DASH;
    }
    match p.mover.guard {
        GuardState::Block => return anim::GUARD,
        GuardState::Parry { .. } | GuardState::Whiff { .. } => return anim::PARRY,
        GuardState::None => {}
    }
    if let Some(s) = p.mover.script {
        let ab = &p.sheet.kit.abilities[s.ability as usize];
        // Script times live in the client's tick space; the last executed frame is "now".
        let elapsed = tick_delta(p.last_input_tick, s.started).max(0) as Tick;
        return script_anim(ab, elapsed);
    }
    if !p.mover.mv.on_ground {
        anim::AIR
    } else if p.mover.ground_speed() > 10.0 {
        anim::RUN
    } else {
        anim::IDLE
    }
}
