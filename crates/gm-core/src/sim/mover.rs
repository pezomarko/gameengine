//! The predicted part of a player: abilities, guard, statuses and movement for one tick.
//! Runs identically on the client (prediction) and the server (authority).

use glam::Vec3;

use crate::build::Sheet;
use crate::collide::Aabb;
use crate::geom::Capsule;
use crate::matrix::EVADING_GRACE_TICKS;
use crate::movement::{MoveInput, MoveVars, PlayerState, player_move, yaw_vectors};
use crate::sim::{
    BAR_CELLS, COMMAND_EXIT_MS, KIT_USE_MS, MAX_ABILITIES, REGEN_PAUSE_MS, tick_delta,
};
use crate::status::Statuses;
use crate::tick::{Tick, TickRate};
use crate::trace::{CollisionWorld, Hull};
use crate::vocab::{ArchetypeFrame, FireMode, Guard, MeleeArc, Mode, MoveKind, StatusTarget, Verb};

/// Button bits (PROTOCOL.md 4). Movement direction is in the axes, not here.
pub mod buttons {
    pub const JUMP: u16 = 1 << 0;
    pub const CROUCH: u16 = 1 << 1;
    pub const PRIMARY: u16 = 1 << 2;
    pub const SECONDARY: u16 = 1 << 3;
    pub const GUARD: u16 = 1 << 4;
    pub const ABILITY1: u16 = 1 << 5;
    pub const ABILITY2: u16 = 1 << 6;
    pub const ABILITY3: u16 = 1 << 7;
    pub const ABILITY4: u16 = 1 << 8;
    pub const INTERACT: u16 = 1 << 9;
    pub const VIEWPORT: u16 = 1 << 10;
    /// Held: the command stance (COMPANIONS.md 5.1).
    pub const COMMAND: u16 = 1 << 11;
    /// Pressed: reload the firearm in hand (MODES.md 3.2).
    pub const RELOAD: u16 = 1 << 12;
    /// Held: the scope is up (MODES.md 3.2), on a firearm that has one.
    pub const SCOPE: u16 = 1 << 13;
    /// Pressed: use a kit (MODES.md 11.3), in every mode.
    pub const USE: u16 = 1 << 14;
    /// Bit 15 must be zero on the wire.
    pub const RESERVED: u16 = 0x8000;
}

/// Animation states carried in snapshots (`anim`). Cosmetic; the client never simulates them.
pub mod anim {
    pub const IDLE: u8 = 0;
    pub const RUN: u8 = 1;
    pub const AIR: u8 = 2;
    pub const WINDUP: u8 = 3;
    pub const SWING: u8 = 4;
    pub const RECOVER: u8 = 5;
    pub const DASH: u8 = 6;
    pub const DEAD: u8 = 7;
    pub const GUARD: u8 = 8;
    pub const PARRY: u8 = 9;
    pub const CAST: u8 = 10;
    pub const STAGGER: u8 = 11;
    /// In the command stance: everyone sees a commander is at it (COMPANIONS.md 5.1).
    pub const COMMAND: u8 = 12;
    /// On the ground, knocked down or launched (MODES.md 4.5).
    pub const DOWN: u8 = 13;
    /// Working the firearm's reload (MODES.md 3.2).
    pub const RELOAD: u8 = 14;
    /// Using a kit (MODES.md 11.3): the weapon lowered, the hands at the body.
    pub const USE: u8 = 15;

    /// The stances of a running script: the ones a body's `acting` ability goes with.
    pub fn acts(state: u8) -> bool {
        matches!(state, WINDUP | SWING | RECOVER | CAST)
    }
}

/// Marker for "no cast animation" lookups.
pub const CAST_ANIM_NONE: u8 = 0xff;

/// One tick of input in simulation units (dequantized from the wire frame).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Input {
    pub buttons: u16,
    pub yaw: f32,
    pub pitch: f32,
    pub forward: f32,
    pub side: f32,
    /// Ability slot activated this tick (1-based), 0 = none.
    pub ability: u8,
    /// The weapon in hand of a gun build (MODES.md 3.7): 0 the primary, 1 the secondary,
    /// 2 the knife. Other modes ignore it.
    pub held: u8,
    /// The body an activation this tick is aimed at (MODES.md 5.3); 0 = none.
    pub target: u32,
    /// The item cell a press of `USE` this tick uses (LOOK.md 3.2, MODES.md 11.3):
    /// 1-based, 0 = none (a `USE` without a cell is the first cell's, the kit's by
    /// default).
    pub use_slot: u8,
}

/// A body near the mover, as the magnet (MODES.md 4.2) and a target-action (5.3) read it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Nearby {
    pub id: u32,
    pub centre: Vec3,
    pub velocity: Vec3,
    pub team: u8,
    pub party: u32,
}

/// What the mover sees of the others this tick: the living bodies, and the mover's own
/// team and party to tell an enemy from a friend (an enemy is on another team, or, in the
/// wild where everybody is on one, in another party).
#[derive(Clone, Copy, Debug)]
pub struct Company<'a> {
    pub bodies: &'a [Nearby],
    pub team: u8,
    pub party: u32,
}

impl Company<'_> {
    pub const NONE: Company<'static> = Company {
        bodies: &[],
        team: 0,
        party: 0,
    };

    pub fn is_enemy(&self, b: &Nearby) -> bool {
        b.team != self.team || (b.team == crate::sim::TEAM_WILD && b.party != self.party)
    }

    pub fn find(&self, id: u32) -> Option<&Nearby> {
        self.bodies.iter().find(|b| b.id == id)
    }
}

/// A firearm's state in a hand (MODES.md 3.2), predicted like everything of the mover.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GunState {
    pub magazine: u8,
    pub reserve: u16,
    /// A reload under way ends at this frame tick.
    pub reload_until: Option<Tick>,
    /// The last shot's frame tick, and the index of the next shot in the spray.
    pub last_shot: Tick,
    pub spray: u8,
}

/// A running ability script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Script {
    pub ability: u8,
    pub started: Tick,
    pub next_step: u8,
    pub ends: Tick,
    /// A firearm's: the index of this shot in its spray (MODES.md 3.3).
    pub shot: u8,
    /// The body the activation was aimed at (MODES.md 5.3); 0 = none.
    pub target: u32,
}

/// An active `MoveSelf::Dash` or `Charge`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dash {
    pub dir: Vec3,
    pub speed: f32,
    pub until: Tick,
    /// Charge: ends when the way is blocked.
    pub stop_on_hit: bool,
}

/// Guard state (VOCABULARY.md 5.6). Times are frame ticks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GuardState {
    #[default]
    None,
    /// Block held.
    Block,
    /// Parry window open until the tick.
    Parry { until: Tick },
    /// Missed parry: recovering until the tick, no actions.
    Whiff { until: Tick },
}

/// Everything the client predicts for its own entity. Plain `Copy` data so a prediction ring
/// can store one per tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mover {
    pub mv: PlayerState,
    pub yaw: f32,
    pub pitch: f32,
    pub stamina: f32,
    pub focus: f32,
    pub buttons_prev: u16,
    pub script: Option<Script>,
    /// Tick at which each slot is ready again.
    pub cooldowns: [Tick; MAX_ABILITIES],
    pub dash: Option<Dash>,
    pub guard: GuardState,
    pub statuses: Statuses,
    /// Evading (MATRIX.md 6) until this frame tick.
    pub evade_until: Tick,
    /// Invulnerable until this frame tick (`MoveSelf.iframes`).
    pub iframes_until: Tick,
    /// Stamina regeneration resumes at this frame tick.
    pub regen_pause_until: Tick,
    /// In the command stance until this frame tick: it is pushed ahead on every frame that
    /// holds the `command` button, so it also covers standing up after the release.
    pub command_until: Tick,
    /// The yaw the body is held at until a frame tick: the magnet's turn (MODES.md 4.2)
    /// or the turn toward a target (5.3); the frame's yaw is the view meanwhile.
    pub lock_yaw: Option<(f32, Tick)>,
    /// A chain's window (MODES.md 4.3): the slot that was pressed, the kit index of the
    /// next stage, the frame tick the window closes.
    pub chain: Option<(u8, u8, Tick)>,
    /// The weapon in hand (MODES.md 3.7) and the firearms' state: the primary's, the
    /// secondary's.
    pub held: u8,
    pub guns: [GunState; 2],
    /// The item bar (LOOK.md 3.2, MODES.md 11.3): how many of each cell's stack the body
    /// carries, read from the inventory by the zone and adopted from the own block by the
    /// client; a use under way, the frame tick it ends at and the cell it is of; and the
    /// cell whose use ended this frame, for the zone (which knows what the stack does) to
    /// take.
    pub bar: [u16; BAR_CELLS],
    pub use_until: Option<Tick>,
    pub use_cell: u8,
    pub used: Option<u8>,
    /// Crouching (MODES.md 3.4, 3.5, 10.2): the button held on the ground. The eye, the
    /// top of the hitbox and the body are lower by `CROUCH_DROP`, the head band with
    /// them, and the body walks at half speed; the world hull does not change.
    pub crouched: bool,
}

impl Mover {
    pub fn new(origin: Vec3, yaw: f32) -> Mover {
        Mover {
            mv: PlayerState::new(origin),
            yaw,
            pitch: 0.0,
            stamina: 100.0,
            focus: 100.0,
            buttons_prev: 0,
            script: None,
            cooldowns: [0; MAX_ABILITIES],
            dash: None,
            guard: GuardState::None,
            statuses: Statuses::default(),
            evade_until: 0,
            iframes_until: 0,
            regen_pause_until: 0,
            command_until: 0,
            lock_yaw: None,
            chain: None,
            held: 0,
            guns: [GunState::default(); 2],
            bar: [0; BAR_CELLS],
            use_until: None,
            use_cell: 0,
            used: None,
            crouched: false,
        }
    }

    /// A fresh mover with the sheet's pools full.
    pub fn spawn(origin: Vec3, yaw: f32, sheet: &Sheet) -> Mover {
        let mut m = Mover::new(origin, yaw);
        m.stamina = sheet.derived.stamina;
        m.focus = sheet.derived.focus;
        m.fill_guns(sheet);
        m
    }

    /// Every firearm loaded once (the gun is issued full: MODES.md 11.2); the reserve is
    /// the stack carried, which the zone sets when it reads the inventory, and a respawn
    /// keeps both (`Zone::respawn`).
    pub fn fill_guns(&mut self, sheet: &Sheet) {
        let kit = &sheet.kit;
        for (g, slot) in self.guns.iter_mut().zip([kit.primary, kit.secondary]) {
            *g = GunState::default();
            if let Some(f) = slot.and_then(|i| kit.abilities[i as usize].firearm.as_ref()) {
                g.magazine = f.magazine;
            }
        }
    }

    /// The ability the primary mouse button fires (MODES.md 3.7): the weapon in hand in
    /// the gun mode, the primary elsewhere.
    pub fn in_hand(&self, kit: &crate::build::Kit) -> Option<u8> {
        if kit.mode != crate::vocab::Mode::Gun {
            return kit.primary;
        }
        match self.held {
            1 => kit.secondary,
            2 => kit.knife,
            _ => kit.primary,
        }
    }

    /// The firearm in hand and its state, if the weapon in hand is one.
    pub fn gun_in_hand<'a>(
        &self,
        kit: &'a crate::build::Kit,
    ) -> Option<(&'a crate::vocab::Firearm, &GunState)> {
        let slot = self.in_hand(kit)?;
        let f = kit.abilities[slot as usize].firearm.as_ref()?;
        Some((f, &self.guns[self.held.min(1) as usize]))
    }

    pub fn reloading(&self, now: Tick) -> bool {
        self.guns[self.held.min(1) as usize]
            .reload_until
            .is_some_and(|u| tick_delta(now, u) < 0)
    }

    /// An item in use at frame tick `now` (MODES.md 11.3): the cell it is of.
    pub fn using_item(&self, now: Tick) -> Option<u8> {
        self.use_until
            .is_some_and(|u| tick_delta(now, u) < 0)
            .then_some(self.use_cell)
    }

    /// How far a use under way has come at `now` (0 just begun, 1 done), for the cell's
    /// sweep; `None` without one.
    pub fn use_progress(&self, now: Tick, dt: f32) -> Option<f32> {
        let until = self.use_until?;
        let left = tick_delta(until, now).max(0) as f32;
        let whole = kit_use_ticks(dt).max(1) as f32;
        Some((1.0 - left / whole).clamp(0.0, 1.0))
    }

    pub fn eye(&self) -> Vec3 {
        let eye = self.mv.eye_position();
        if self.crouched {
            eye - Vec3::Z * CROUCH_DROP
        } else {
            eye
        }
    }

    /// Unit view direction from yaw and pitch (positive pitch looks down).
    pub fn view_dir(&self) -> Vec3 {
        view_dir(self.yaw, self.pitch)
    }

    pub fn aabb(&self) -> Aabb {
        Aabb::around(self.mv.origin, self.mv.hull)
    }

    /// Hitbox capsule for `frame`, standing on the hull's feet, `CROUCH_DROP` shorter
    /// while crouched (MODES.md 3.5).
    pub fn capsule(&self, frame: ArchetypeFrame) -> Capsule {
        capsule_at(self.mv.origin, self.mv.hull, frame, self.crouched)
    }

    pub fn ground_speed(&self) -> f32 {
        self.mv.ground_speed()
    }

    pub fn blocking(&self) -> bool {
        self.guard == GuardState::Block
    }

    /// Evading at frame tick `now` (MATRIX.md 6).
    pub fn evading(&self, now: Tick) -> bool {
        self.dash.is_some() || tick_delta(now, self.evade_until) < 0
    }

    pub fn invulnerable(&self, now: Tick) -> bool {
        tick_delta(now, self.iframes_until) < 0
    }

    /// In the command stance at frame tick `now` (COMPANIONS.md 5.1): no movement of its own,
    /// no activation, no guard.
    pub fn commanding(&self, now: Tick) -> bool {
        tick_delta(now, self.command_until) < 0
    }

    /// Forget every running thing (death, respawn); cooldowns survive.
    pub fn reset_actions(&mut self) {
        self.script = None;
        self.dash = None;
        self.guard = GuardState::None;
        self.statuses.clear();
        self.evade_until = 0;
        self.iframes_until = 0;
        self.command_until = 0;
        self.lock_yaw = None;
        self.chain = None;
        for g in &mut self.guns {
            g.reload_until = None;
            g.spray = 0;
        }
        self.use_until = None;
    }
}

pub fn view_dir(yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.to_radians().sin_cos();
    let (sp, cp) = pitch.to_radians().sin_cos();
    Vec3::new(cp * cy, cp * sy, -sp)
}

/// The hitbox capsule of a body of `frame` whose hull origin is `origin`: standing on the
/// hull's feet, its top `CROUCH_DROP` lower while `crouched` (MODES.md 3.5), so the head
/// band moves down with the body and a shot at a standing head passes over a crouched one.
pub fn capsule_at(origin: Vec3, hull: Hull, frame: ArchetypeFrame, crouched: bool) -> Capsule {
    let (radius, height) = frame.capsule();
    let height = if crouched {
        height - CROUCH_DROP
    } else {
        height
    };
    Capsule::upright(origin + Vec3::new(0.0, 0.0, hull.mins().z), radius, height)
}

/// What a mover asked the authoritative side to do this tick. The client ignores these (or
/// spawns cosmetic previews); the server resolves them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Swing {
        ability: u8,
        step: u8,
        active_from: Tick,
        active_until: Tick,
    },
    Fire {
        ability: u8,
        step: u8,
        /// Where the spray has turned the gun before this shot, (yaw, pitch) degrees
        /// (MODES.md 3.3): the zone turns the bolt by it.
        turn: (f32, f32),
        /// A firearm's kick after this shot, (yaw, pitch) degrees (MODES.md 3.3), and the
        /// cone it is rolled in (3.4); zero for anything but a firearm.
        kick: (f32, f32),
        cone_deg: f32,
        /// The bolt's multiplier in the head band (MODES.md 3.5); 1 for anything else.
        headshot: f32,
        /// The body it is aimed at (MODES.md 5.3); 0 = where the body looks.
        target: u32,
    },
    Area {
        ability: u8,
        step: u8,
        target: u32,
    },
    /// A parry window opened (the server resolves hits against it).
    ParryOpened,
}

const DASH_VARS: MoveVars = MoveVars {
    friction: 0.0,
    edge_friction: 1.0,
    accelerate: 0.0,
    air_accelerate: 0.0,
    ..MoveVars::QUAKE
};

/// Advance one mover by one tick: statuses, guard, ability activation, script steps, movement,
/// regeneration. `now` is the **client tick of the frame** on both sides (the server passes the
/// frame's tick, never its own), so cooldowns, scripts, statuses and dashes elapse identically
/// even when the server runs two frames in one tick. `company` is what the mover sees of the
/// others: the magnet (MODES.md 4.2) and a target-action (5.3) read it.
#[allow(clippy::too_many_arguments)]
pub fn step_mover<W: CollisionWorld + ?Sized>(
    world: &W,
    sheet: &Sheet,
    m: &mut Mover,
    input: &Input,
    now: Tick,
    dt: f32,
    company: Company<'_>,
    actions: &mut Vec<Action>,
) {
    let kit = &sheet.kit;
    let d = &sheet.derived;
    let pressed = input.buttons & !m.buttons_prev;
    m.buttons_prev = input.buttons;
    m.yaw = input.yaw;
    m.pitch = input.pitch;
    // A held yaw (the magnet's turn, the turn toward a target) stands in for the frame's
    // while it lasts; the view is the frame's still.
    match m.lock_yaw {
        Some((yaw, until)) if tick_delta(now, until) < 0 => m.yaw = yaw,
        Some(_) => m.lock_yaw = None,
        None => {}
    }
    m.statuses.expire(now);
    // Taunted (MATRIX.md 8): the body is turned to whoever taunted it and held there,
    // tick by tick, while the taunt lasts and the taunter is in sight of the wire; the
    // client's view follows the mover's yaw while the hold stands (CLIENT.md).
    if let Some(by) = m.statuses.taunted_by()
        && let Some(b) = company.find(by)
        && let Some(until) = m.statuses.get(crate::vocab::Status::Taunt).map(|s| s.until)
    {
        let yaw = yaw_toward(m.mv.origin, b.centre);
        m.lock_yaw = Some((yaw, until));
        m.yaw = yaw;
    }
    // Stagger interrupts everything (MATRIX.md 8); the server already cleared the script
    // when it applied the status, the client follows at reconciliation. A body on the
    // ground (MODES.md 4.5) is as helpless.
    let staggered = m.statuses.staggered() || m.statuses.downed();
    if staggered {
        m.script = None;
        m.guard = GuardState::None;
        m.lock_yaw = None;
        m.chain = None;
        for g in &mut m.guns {
            g.reload_until = None;
        }
        m.use_until = None;
    }

    // The command stance (COMPANIONS.md 5.1): it begins on a frame that holds the button
    // while nothing runs, lasts while the button is held and for the exit time after.
    if input.buttons & buttons::COMMAND != 0
        && !staggered
        && (m.script.is_none() || m.commanding(now))
    {
        m.command_until = now.wrapping_add(command_exit_ticks(dt)).wrapping_add(1);
    }
    let commanding = m.commanding(now);
    // In the stance the body does nothing of its own: the frame is read as if only the view
    // angles had been sent.
    let stance_input;
    let (input, pressed) = if commanding {
        m.guard = GuardState::None;
        stance_input = Input {
            buttons: 0,
            forward: 0.0,
            side: 0.0,
            ability: 0,
            ..*input
        };
        (&stance_input, 0)
    } else {
        (input, pressed)
    };

    guard_step(sheet, m, input, pressed, now, staggered, actions);

    // The weapon in hand (MODES.md 3.7): a switch cancels a reload under way.
    if kit.mode == Mode::Gun && input.held != m.held && input.held <= 2 && !staggered {
        m.guns[m.held.min(1) as usize].reload_until = None;
        m.held = input.held;
    }
    let primary = m.in_hand(kit);
    let auto = primary
        .and_then(|i| kit.abilities[i as usize].firearm.as_ref())
        .is_some_and(|f| f.fire == FireMode::Auto);
    let slot = if input.ability > 0 && (input.ability as usize) <= kit.actives.len() {
        kit.actives[input.ability as usize - 1]
    } else if pressed & buttons::PRIMARY != 0 || (auto && input.buttons & buttons::PRIMARY != 0) {
        primary
    } else if pressed & buttons::SECONDARY != 0 && kit.mode != Mode::Gun {
        kit.secondary
    } else if pressed & buttons::ABILITY1 != 0 {
        kit.actives[0]
    } else if pressed & buttons::ABILITY2 != 0 {
        kit.actives[1]
    } else if pressed & buttons::ABILITY3 != 0 {
        kit.actives[2]
    } else if pressed & buttons::ABILITY4 != 0 {
        kit.actives[3]
    } else {
        None
    };
    if let Some(slot) = slot
        && !staggered
        && m.using_item(now).is_none()
    {
        try_activate(world, sheet, m, slot as usize, now, input, company);
    }
    reload_step(sheet, m, pressed, now, staggered);
    item_step(m, pressed, input.use_slot, now, dt, staggered);

    if let Some(mut s) = m.script {
        let ab = &kit.abilities[s.ability as usize];
        let elapsed = tick_delta(now, s.started).max(0) as Tick;
        while (s.next_step as usize) < ab.steps.len()
            && ab.steps[s.next_step as usize].at <= elapsed
        {
            resolve_step(
                world,
                sheet,
                m,
                input,
                &ab.steps[s.next_step as usize].verb,
                &s,
                s.next_step,
                now,
                company,
                actions,
            );
            s.next_step += 1;
        }
        m.script = if tick_delta(now, s.ends) >= 0 {
            None
        } else {
            Some(s)
        };
    }
    // A chain's window closes by itself (MODES.md 4.3).
    if m.chain
        .is_some_and(|(_, _, until)| tick_delta(now, until) >= 0)
    {
        m.chain = None;
    }

    // Movement scale: the script, the guard, the scope, then statuses on top of the
    // sheet's speed.
    let mut scale = m
        .script
        .map_or(1.0, |s| kit.abilities[s.ability as usize].move_scale);
    match (m.guard, kit.guard_verb()) {
        (GuardState::Block, Some(Guard::Block(b))) => scale *= b.move_speed_scale,
        (GuardState::Parry { .. } | GuardState::Whiff { .. }, _) => scale *= 0.5,
        _ => {}
    }
    if input.buttons & buttons::SCOPE != 0
        && let Some((f, _)) = m.gun_in_hand(kit)
        && f.scope > 0
        && let Some(i) = primary
    {
        scale *= kit.abilities[i as usize].move_scale;
    }
    if m.reloading(now)
        && let Some(i) = primary
    {
        scale *= kit.abilities[i as usize].move_scale.max(0.5);
    }
    // An item is used walking (MODES.md 11.3).
    if m.using_item(now).is_some() {
        scale *= 0.5;
    }
    // A crouch is a half-speed creep with the eye lowered (MODES.md 3.4); the button
    // counts on the ground only.
    m.crouched = input.buttons & buttons::CROUCH != 0 && m.mv.on_ground;
    if m.crouched {
        scale *= 0.5;
    }
    let vars = MoveVars {
        max_speed: d.max_speed * m.statuses.speed_scale(),
        ..MoveVars::QUAKE
    };
    let jump = input.buttons & buttons::JUMP != 0 && !staggered;
    match m.dash {
        Some(dsh)
            if tick_delta(dsh.until, now) > 0 && !m.statuses.has(crate::vocab::Status::Root) =>
        {
            m.mv.velocity.x = dsh.dir.x * dsh.speed;
            m.mv.velocity.y = dsh.dir.y * dsh.speed;
            let mi = MoveInput {
                yaw: m.yaw,
                forward: 0.0,
                side: 0.0,
                jump: false,
            };
            player_move(&world, &DASH_VARS, &mut m.mv, &mi, dt);
            if dsh.stop_on_hit && m.mv.velocity.truncate().length() < dsh.speed * 0.5 {
                m.dash = None;
            }
        }
        _ => {
            m.dash = None;
            let mi = MoveInput {
                yaw: input.yaw,
                forward: input.forward.clamp(-1.0, 1.0) * scale,
                side: input.side.clamp(-1.0, 1.0) * scale,
                jump,
            };
            player_move(&world, &vars, &mut m.mv, &mi, dt);
        }
    }

    if tick_delta(now, m.regen_pause_until) >= 0 {
        m.stamina = (m.stamina + d.stamina_regen * dt).min(d.stamina);
    }
    m.focus = (m.focus + d.focus_regen * dt).min(d.focus);
}

/// The block/parry state machine (VOCABULARY.md 5.6).
#[allow(clippy::too_many_arguments)]
fn guard_step(
    sheet: &Sheet,
    m: &mut Mover,
    input: &Input,
    pressed: u16,
    now: Tick,
    staggered: bool,
    actions: &mut Vec<Action>,
) {
    let kit = &sheet.kit;
    let Some(gi) = kit.guard else {
        m.guard = GuardState::None;
        return;
    };
    match kit.guard_verb() {
        Some(Guard::Block(_)) => {
            let want = input.buttons & buttons::GUARD != 0
                && !staggered
                && m.script.is_none()
                && m.stamina > 0.0;
            m.guard = if want {
                GuardState::Block
            } else {
                GuardState::None
            };
        }
        Some(Guard::Parry(p)) => match m.guard {
            GuardState::Parry { until } if tick_delta(now, until) >= 0 => {
                m.guard = GuardState::Whiff {
                    until: now.wrapping_add(p.whiff_recovery),
                };
            }
            GuardState::Whiff { until } if tick_delta(now, until) >= 0 => {
                m.guard = GuardState::None;
            }
            GuardState::None | GuardState::Block => {
                let ab = &kit.abilities[gi as usize];
                let ready = tick_delta(now, m.cooldowns[gi as usize]) >= 0;
                if pressed & buttons::GUARD != 0
                    && !staggered
                    && m.script.is_none()
                    && ready
                    && m.stamina >= ab.cost.stamina as f32
                {
                    m.stamina -= ab.cost.stamina as f32;
                    if ab.cost.stamina > 0 {
                        m.regen_pause_until = now.wrapping_add(regen_pause_ticks());
                    }
                    let ready_at = now.wrapping_add(ab.cooldown.ticks.max(1));
                    m.cooldowns[gi as usize] = ready_at;
                    if let Some(g) = ab.cooldown.group {
                        for (i, other) in kit.abilities.iter().enumerate() {
                            if other.cooldown.group == Some(g) {
                                m.cooldowns[i] = ready_at;
                            }
                        }
                    }
                    m.guard = GuardState::Parry {
                        until: now.wrapping_add(p.window),
                    };
                    actions.push(Action::ParryOpened);
                } else if m.guard == GuardState::Block {
                    m.guard = GuardState::None;
                }
            }
            _ => {}
        },
        None => m.guard = GuardState::None,
    }
}

/// The reload of the firearm in hand (MODES.md 3.2): `R`, or an empty magazine with
/// rounds carried, which reloads by itself; it ends by itself, a stagger drops it and the
/// rounds are kept (and the empty magazine begins it again once the body can).
fn reload_step(sheet: &Sheet, m: &mut Mover, pressed: u16, now: Tick, staggered: bool) {
    let kit = &sheet.kit;
    if kit.mode != Mode::Gun || m.held > 1 {
        return;
    }
    let Some(f) = m
        .in_hand(kit)
        .and_then(|i| kit.abilities[i as usize].firearm.as_ref())
    else {
        return;
    };
    let g = &mut m.guns[m.held as usize];
    match g.reload_until {
        Some(until) if tick_delta(now, until) >= 0 => {
            let take =
                (f.magazine - g.magazine.min(f.magazine)).min(g.reserve.min(u8::MAX as u16) as u8);
            g.magazine += take;
            g.reserve -= take as u16;
            g.reload_until = None;
        }
        Some(_) => {}
        None => {
            // An empty magazine with rounds carried reloads by itself, trigger or not.
            let asked = pressed & buttons::RELOAD != 0 || g.magazine == 0;
            if asked
                && !staggered
                && g.magazine < f.magazine
                && g.reserve > 0
                && m.script.is_none()
                && m.use_until.is_none()
            {
                g.reload_until = Some(now.wrapping_add(f.reload.max(1)));
                g.spray = 0;
            }
        }
    }
}

/// Why a press of `USE` begins no item's use (MODES.md 11.3), in the words the HUD says.
/// The zone's own refusal, at full health, is not the mover's to know: it clears the use
/// the same tick (`Zone::step`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemRefusal {
    /// The cell is empty: nothing of its stack is carried, or nothing is set on it.
    Empty,
    /// The body is in the air.
    InTheAir,
    /// A script, a dash, a reload, the command stance or another use has the hands.
    HandsBusy,
    /// Staggered or down.
    Staggered,
}

impl ItemRefusal {
    /// The word the HUD shows for a second (MODES.md 11.3).
    pub fn word(self) -> &'static str {
        match self {
            ItemRefusal::Empty => "nothing to use",
            ItemRefusal::InTheAir => "not in the air",
            ItemRefusal::HandsBusy => "hands busy",
            ItemRefusal::Staggered => "staggered",
        }
    }
}

/// The cell a frame's `use_slot` names (1-based on the wire), 0-based; `None` for one
/// the bar does not have. A `USE` with no cell named is the first cell's.
pub fn bar_cell(use_slot: u8) -> Option<usize> {
    let cell = use_slot.max(1) as usize - 1;
    (cell < BAR_CELLS).then_some(cell)
}

/// Why the item of `cell` (0-based) pressed now would be refused, `None` when its use
/// would begin (MODES.md 11.3): what `item_step` asks before it begins one, and what the
/// client says of a press that began nothing. `staggered` is the body's stagger or
/// knockdown.
pub fn item_refusal(m: &Mover, cell: usize, now: Tick, staggered: bool) -> Option<ItemRefusal> {
    if m.bar.get(cell).is_none_or(|n| *n == 0) {
        Some(ItemRefusal::Empty)
    } else if staggered {
        Some(ItemRefusal::Staggered)
    } else if !m.mv.on_ground {
        Some(ItemRefusal::InTheAir)
    } else if m.script.is_some()
        || m.dash.is_some()
        || m.reloading(now)
        || m.commanding(now)
        || m.using_item(now).is_some()
    {
        Some(ItemRefusal::HandsBusy)
    } else {
        None
    }
}

/// An item's use (MODES.md 11.3, LOOK.md 3.2): `USE` pressed with a cell named, on the
/// ground, with something in the cell and the hands free, begins it; it ends by itself
/// after `KIT_USE_MS`, one fewer in the cell, and `used` says which cell for the zone,
/// which knows what the stack does (a kit heals) and reads it the same step. A stagger or
/// a knockdown drops it with the item kept (the stagger block above). A press that is
/// refused is not kept: `USE` is a press, not a wish (the HUD says why, and the player
/// presses again).
fn item_step(m: &mut Mover, pressed: u16, use_slot: u8, now: Tick, dt: f32, staggered: bool) {
    match m.use_until {
        Some(until) if tick_delta(now, until) >= 0 => {
            m.use_until = None;
            let cell = m.use_cell as usize;
            if let Some(n) = m.bar.get_mut(cell) {
                *n = n.saturating_sub(1);
                m.used = Some(m.use_cell);
            }
        }
        Some(_) => {}
        None => {
            if pressed & buttons::USE != 0
                && let Some(cell) = bar_cell(use_slot)
                && item_refusal(m, cell, now, staggered).is_none()
            {
                m.use_until = Some(now.wrapping_add(kit_use_ticks(dt)));
                m.use_cell = cell as u8;
            }
        }
    }
}

/// Frames an item's use takes (`KIT_USE_MS`, the kit's and every stack's for now) at the
/// tick length `dt`: 1.5 s in a town at 20 Hz as in a fight at 64.
pub fn kit_use_ticks(dt: f32) -> Tick {
    (KIT_USE_MS as f32 / 1000.0 / dt).ceil() as Tick
}

fn regen_pause_ticks() -> Tick {
    TickRate::COMBAT.ms_to_ticks(REGEN_PAUSE_MS)
}

/// Frames a body needs to stand up from the command stance, at the tick length `dt`.
pub fn command_exit_ticks(dt: f32) -> Tick {
    (COMMAND_EXIT_MS as f32 / 1000.0 / dt).ceil() as Tick
}

/// The cone a firearm's bolt is rolled in now (MODES.md 3.4), in degrees, shaped as the
/// root's: the stance's base (`scoped` while the scope is up), the move's share past a
/// walk, the air's, and the spray's growing with the square of the shots within `recover`
/// of the last. `shot` is the index of the shot in its spray (0 for the first).
#[allow(clippy::too_many_arguments)]
pub fn cone_deg(
    f: &crate::vocab::Firearm,
    max_speed: f32,
    m: &Mover,
    g: &GunState,
    crouched: bool,
    scoped: bool,
    shot: u8,
    now: Tick,
) -> f32 {
    let base = if scoped && f.scope > 0 {
        f.cone.scoped
    } else if crouched && m.mv.on_ground {
        f.cone.crouch
    } else {
        f.cone.stand
    };
    let speed = m.mv.ground_speed() / max_speed.max(1.0);
    let spray = if shot > 0 && tick_delta(now, g.last_shot) <= f.cone.recover as i32 {
        (shot as f32 / 4.0).powi(2).min(4.0)
    } else {
        0.0
    };
    base + f.cone.moving * move_share(speed)
        + if m.mv.on_ground { 0.0 } else { f.cone.air }
        + f.cone.shot * spray
}

/// The move's share of the cone at a fraction of the body's speed (MODES.md 3.4): a
/// creep up to half speed is as good as standing, a run from four fifths is all of it,
/// smooth between.
pub fn move_share(speed: f32) -> f32 {
    let t = ((speed - CREEP_SPEED) / (RUN_SPEED - CREEP_SPEED)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The fraction of the body's speed up to which the move opens no cone (Shift's walk is
/// half), and the fraction from which it opens all of it (MODES.md 3.4).
pub const CREEP_SPEED: f32 = 0.5;
pub const RUN_SPEED: f32 = 0.8;

/// The scoped cone content does not name: this fraction of the standing one
/// (MODES.md 3.4).
pub const SCOPED_CONE: f32 = 0.25;

/// How far a crouch brings the body down, in units (MODES.md 3.4, 3.5): the eye, the top
/// of the hitbox capsule (and so the head band) and the drawn body, all by the same
/// amount. Two sevenths of a striker's 56 (the root halves its 72; a capsule of our
/// radius cannot shorten that far and stay a body).
pub const CROUCH_DROP: f32 = 16.0;

/// Line of sight from `eye` to a body's `centre` through the mover's world, which holds
/// the other bodies as solids: the trace stops a little short of the body, so the body
/// itself is not in its own way.
pub fn sees<W: CollisionWorld + ?Sized>(world: &W, eye: Vec3, centre: Vec3) -> bool {
    let to = centre - eye;
    let len = to.length();
    if len <= BODY_CLEARANCE {
        return true;
    }
    let end = eye + to * ((len - BODY_CLEARANCE) / len);
    world.trace(Hull::Point, eye, end).fraction >= 1.0
}

/// How far short of a body's centre a sight line stops: past the player hull's half
/// width, within a frame's reach.
const BODY_CLEARANCE: f32 = 20.0;

/// The yaw from `from` to `to`, in degrees, as the mover's yaw is.
pub fn yaw_toward(from: Vec3, to: Vec3) -> f32 {
    let d = to - from;
    d.y.atan2(d.x).to_degrees()
}

/// The shortest turn from `a` to `b`, degrees in `-180..=180`.
fn turn_between(a: f32, b: f32) -> f32 {
    let mut d = (b - a) % 360.0;
    if d > 180.0 {
        d -= 360.0;
    } else if d < -180.0 {
        d += 360.0;
    }
    d
}

/// Start the pressed slot's ability, or what its chain or its target make of it.
fn try_activate<W: CollisionWorld + ?Sized>(
    world: &W,
    sheet: &Sheet,
    m: &mut Mover,
    pressed_slot: usize,
    now: Tick,
    input: &Input,
    company: Company<'_>,
) -> bool {
    let kit = &sheet.kit;
    // A chain's window (MODES.md 4.3): the pressed slot plays the next stage.
    let mut slot = pressed_slot;
    let mut chained = false;
    if let Some((from, next, until)) = m.chain
        && from as usize == pressed_slot
        && tick_delta(now, until) < 0
    {
        slot = next as usize;
        chained = true;
    }
    if slot >= kit.abilities.len().min(MAX_ABILITIES) {
        return false;
    }
    let ab = &kit.abilities[slot];
    if let Some(s) = m.script {
        // A script in its recovery is cut short by its chain's next stage or by a dash
        // that cancels (MODES.md 4.4); nothing else starts while one runs.
        let elapsed = tick_delta(now, s.started).max(0) as Tick;
        let running = &kit.abilities[s.ability as usize];
        let in_recovery = elapsed >= crate::build::script_commit(running);
        let cancels = matches!(
            ab.steps.first().map(|st| &st.verb),
            Some(Verb::MoveSelf(ms)) if ms.cancel_recovery
        );
        if in_recovery && (chained || cancels) {
            m.script = None;
            m.lock_yaw = None;
        } else {
            return false;
        }
    }
    if matches!(m.guard, GuardState::Parry { .. } | GuardState::Whiff { .. }) {
        return false;
    }
    if tick_delta(now, m.cooldowns[slot]) < 0 {
        return false;
    }
    if kit.elemental[slot] && m.statuses.silenced() {
        return false;
    }
    if m.stamina < ab.cost.stamina as f32 || m.focus < ab.cost.focus as f32 {
        return false;
    }
    // A firearm (MODES.md 3.2): a round in the magazine, no reload under way, and a
    // bolt-action worked standing or walking.
    let mut shot = 0;
    if let Some(f) = &ab.firearm {
        if m.held > 1 || m.in_hand(kit) != Some(slot as u8) {
            return false;
        }
        let g = &mut m.guns[m.held as usize];
        if g.magazine == 0 || g.reload_until.is_some() {
            return false;
        }
        if f.fire == FireMode::Bolt && m.mv.ground_speed() > sheet.derived.max_speed * 0.5 + 1.0 {
            return false;
        }
        if tick_delta(now, g.last_shot) > f.cone.recover as i32 {
            g.spray = 0;
        }
        shot = g.spray;
        g.spray = g.spray.saturating_add(1);
        g.last_shot = now;
        g.magazine -= 1;
    }
    m.stamina -= ab.cost.stamina as f32;
    m.focus -= ab.cost.focus as f32;
    if ab.cost.stamina > 0 {
        m.regen_pause_until = now.wrapping_add(regen_pause_ticks());
    }
    // Attacking drops a held block.
    if m.guard == GuardState::Block {
        m.guard = GuardState::None;
    }
    let ready_at = now.wrapping_add(ab.cooldown.ticks.max(1));
    m.cooldowns[slot] = ready_at;
    if let Some(g) = ab.cooldown.group {
        for (i, other) in kit.abilities.iter().enumerate() {
            if other.cooldown.group == Some(g) {
                m.cooldowns[i] = ready_at;
            }
        }
    }
    let ends = now.wrapping_add(kit.durations[slot]);
    m.script = Some(Script {
        ability: slot as u8,
        started: now,
        next_step: 0,
        ends,
        shot,
        target: input.target,
    });
    m.chain = match (ab.chain, kit.chain_next.get(slot).copied().flatten()) {
        (Some(c), Some(next)) => Some((pressed_slot as u8, next, ends.wrapping_add(c.window))),
        _ => None,
    };
    // A target-action (MODES.md 5.3): the body turns to its target for the script, when
    // the target is within the ability's range and in sight.
    if input.target != 0
        && ab.range > 0.0
        && let Some(b) = company.find(input.target)
        && (b.centre - m.mv.origin).truncate().length() <= ab.range
        && sees(world, m.eye(), b.centre)
    {
        let yaw = yaw_toward(m.mv.origin, b.centre);
        m.lock_yaw = Some((yaw, ends));
        m.yaw = yaw;
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn resolve_step<W: CollisionWorld + ?Sized>(
    world: &W,
    sheet: &Sheet,
    m: &mut Mover,
    input: &Input,
    verb: &Verb,
    script: &Script,
    step: u8,
    now: Tick,
    company: Company<'_>,
    actions: &mut Vec<Action>,
) {
    let ability = script.ability;
    match verb {
        Verb::MeleeArc(arc) => {
            // The magnet (MODES.md 4.2): at the swing's start the body turns, up to the
            // arc's assist, toward the nearest enemy within reach and a half that it sees.
            if arc.assist_deg > 0.0 && m.lock_yaw.is_none() {
                let eye = m.eye();
                let near = company
                    .bodies
                    .iter()
                    .filter(|b| company.is_enemy(b))
                    .map(|b| (b, (b.centre - m.mv.origin).truncate().length()))
                    .filter(|(b, dist)| *dist <= arc.reach * 1.5 && sees(world, eye, b.centre))
                    .min_by(|a, b| a.1.total_cmp(&b.1));
                if let Some((b, _)) = near {
                    let want = yaw_toward(m.mv.origin, b.centre);
                    let turn = turn_between(m.yaw, want).clamp(-arc.assist_deg, arc.assist_deg);
                    let yaw = m.yaw + turn;
                    m.lock_yaw =
                        Some((yaw, now.wrapping_add(arc.timing.windup + arc.timing.active)));
                    m.yaw = yaw;
                }
            }
            actions.push(Action::Swing {
                ability,
                step,
                active_from: now.wrapping_add(arc.timing.windup),
                active_until: now.wrapping_add(arc.timing.windup + arc.timing.active),
            })
        }
        Verb::Projectile(_) => {
            let ab = &sheet.kit.abilities[ability as usize];
            let (turn, kick, cone_deg, headshot) = match &ab.firearm {
                Some(f) => {
                    let g = &m.guns[m.held.min(1) as usize];
                    let crouched = input.buttons & buttons::CROUCH != 0;
                    let scoped = input.buttons & buttons::SCOPE != 0;
                    let cone = cone_deg(
                        f,
                        sheet.derived.max_speed,
                        m,
                        g,
                        crouched,
                        scoped,
                        script.shot,
                        now,
                    );
                    (f.turn(script.shot), f.kick(script.shot), cone, f.headshot)
                }
                None => ((0.0, 0.0), (0.0, 0.0), 0.0, 1.0),
            };
            actions.push(Action::Fire {
                ability,
                step,
                turn,
                kick,
                cone_deg,
                headshot,
                target: script.target,
            })
        }
        Verb::AreaEffect(_) => actions.push(Action::Area {
            ability,
            step,
            target: script.target,
        }),
        Verb::ApplyStatus(s) => {
            // Self-targeted statuses are predicted; hit/area targets are the server's.
            if s.target == StatusTarget::Actor {
                m.statuses.apply(s, s.duration, now, 0);
            }
        }
        Verb::MoveSelf(ms) => {
            let (fwd, right) = yaw_vectors(input.yaw);
            let wish = fwd * input.forward + right * input.side;
            let dir = if wish.length_squared() > 1e-4 {
                wish.normalize()
            } else {
                yaw_vectors(m.yaw).0
            };
            let fwd = yaw_vectors(m.yaw).0;
            let duration = match ms.kind {
                MoveKind::Dash { speed, duration } => {
                    m.dash = Some(Dash {
                        dir,
                        speed,
                        until: now.wrapping_add(duration),
                        stop_on_hit: false,
                    });
                    duration
                }
                MoveKind::Charge {
                    speed,
                    duration,
                    stop_on_hit,
                } => {
                    m.dash = Some(Dash {
                        dir: fwd,
                        speed,
                        until: now.wrapping_add(duration),
                        stop_on_hit,
                    });
                    duration
                }
                MoveKind::Leap { forward, up } => {
                    m.mv.velocity += dir * forward + Vec3::new(0.0, 0.0, up);
                    m.mv.on_ground = false;
                    TickRate::COMBAT.ms_to_ticks(250)
                }
                MoveKind::Blink { distance } => {
                    // Hull-traced along the facing; never through geometry (VOCABULARY.md 5.5).
                    let start = m.mv.origin;
                    let end = start + fwd * distance;
                    let tr = world.trace(m.mv.hull, start, end);
                    if !tr.start_solid && !tr.all_solid {
                        m.mv.origin = crate::movement::nudge_position(world, m.mv.hull, tr.end);
                    }
                    m.mv.velocity = Vec3::ZERO;
                    0
                }
            };
            m.evade_until = now.wrapping_add(duration).wrapping_add(EVADING_GRACE_TICKS);
            if ms.iframes > 0 {
                m.iframes_until = now.wrapping_add(ms.iframes);
            }
        }
        // Guard verbs live in the guard slot, never in a script.
        Verb::Guard(_) => {}
    }
}

/// Wedge test of VOCABULARY.md 5.1 against one capsule. Returns the point to trace line of
/// sight to when the capsule is inside the arc.
pub fn melee_hit_point(eye: Vec3, yaw: f32, arc: &MeleeArc, target: &Capsule) -> Option<Vec3> {
    let center = target.center();
    let to = center - eye;
    let horiz = to.truncate();
    let dist = horiz.length();
    if dist - target.radius > arc.reach {
        return None;
    }
    let half_height = (target.b.z - target.a.z) * 0.5 + target.radius;
    if to.z.abs() > arc.half_height + half_height {
        return None;
    }
    if dist > 1e-3 {
        let (fwd, _) = yaw_vectors(yaw);
        let cos = fwd.truncate().dot(horiz) / dist;
        let angle = cos.clamp(-1.0, 1.0).acos().to_degrees();
        let allowance = (target.radius / dist.max(target.radius))
            .min(1.0)
            .asin()
            .to_degrees();
        if angle - allowance > arc.arc_deg * 0.5 {
            return None;
        }
    }
    Some(target.closest_axis_point(eye))
}
