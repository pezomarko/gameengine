//! The part every mind shares (COMPANIONS.md 2.3, 4): turning and aiming within the limits,
//! walking a route, stepping out of telegraphs, guarding what it sees coming, and using a kit
//! by the shape of its abilities without hitting its friends.

use glam::Vec3;
use gm_core::geom::closest_point_on_segment;
use gm_core::movement::yaw_vectors;
use gm_core::rng::Rng;
use gm_core::sim::{Input, anim, buttons, tick_delta};
use gm_core::tick::Tick;
use gm_core::vocab::{EntityId, Status};

use crate::kit::{GuardPlan, KitPlan, Use};
use crate::nav::Navigator;
use crate::sense::{Body, Senses};

/// COMPANIONS.md 2.3.
pub const REACTION_MS: u32 = 200;
pub const TURN_RATE_DEG_S: f32 = 720.0;
pub const AIM_ERROR_DEG: f32 = 1.5;
/// Line of sight to the current target is looked at this often, in ticks.
const LOS_EVERY: Tick = 4;
/// A hostile this close in a windup is a threat to guard against.
const THREAT_REACH: f32 = 130.0;

/// How to fight one target.
pub struct Engage<'a> {
    pub target: &'a Body,
    /// Fight only within this radius of the point (a `Hold` order, a creature's leash).
    pub tether: Option<(Vec3, f32)>,
    /// Work round to the target's back instead of standing in its face.
    pub flank: bool,
    /// The distance to keep when the kit shoots; 0 fights in reach.
    pub range: f32,
    /// Whom to put far-reaching areas on instead of the target (a creature's telegraphs go
    /// to the back line as well as to whoever is in its face).
    pub artillery: Option<&'a Body>,
    /// Who must not be hit, and who may hit us.
    pub ally: &'a dyn Fn(&Body) -> bool,
    pub hostile: &'a dyn Fn(&Body) -> bool,
}

/// One frame being put together.
#[derive(Clone, Copy, Debug, Default)]
struct Frame {
    buttons: u16,
    ability: u8,
    /// Where to walk, in the world, length 0..=1.
    wish: Vec3,
    /// Where to look.
    aim: Option<Vec3>,
}

/// A body this near on the way is walked round.
const ROUND_RADIUS: f32 = 76.0;

pub struct Fighter {
    pub rng: Rng,
    pub plan: KitPlan,
    pub yaw: f32,
    pub pitch: f32,
    pub nav: Navigator,
    strafe: f32,
    next_strafe: Tick,
    /// Windups seen: who, and since which tick (the reaction time runs from there).
    windups: Vec<(EntityId, Tick)>,
    /// Harmful areas seen: which, and since when.
    areas: Vec<(EntityId, Tick)>,
    /// This shot's aim error (yaw, pitch), drawn when the trigger is pulled.
    error: (f32, f32),
    /// Line of sight to a body, as last looked at.
    los: Option<(EntityId, Tick, bool)>,
    guard_until: Tick,
    /// The body being walked round, on which side (1 = to its right), and until when that
    /// choice stands.
    round: Option<(EntityId, f32, Tick)>,
}

/// Yaw and pitch that look from `eye` at `point` (positive pitch looks down).
pub fn angles_to(eye: Vec3, point: Vec3) -> (f32, f32) {
    let to = point - eye;
    let yaw = to.y.atan2(to.x).to_degrees().rem_euclid(360.0);
    let pitch = (-to.z)
        .atan2(to.truncate().length())
        .to_degrees()
        .clamp(-89.0, 89.0);
    (yaw, pitch)
}

fn angle_diff(a: f32, b: f32) -> f32 {
    let mut d = (a - b).rem_euclid(360.0);
    if d > 180.0 {
        d -= 360.0;
    }
    d
}

/// Where to aim a projectile of `speed` and `gravity` scale to meet a moving body.
pub fn lead(eye: Vec3, target: &Body, speed: f32, gravity: f32) -> Vec3 {
    gm_core::sim::aim::lead(eye, target.centre(), target.vel, speed, gravity)
}

impl Fighter {
    pub fn new(seed: u64, plan: KitPlan, yaw: f32) -> Fighter {
        Fighter {
            rng: Rng::new(seed),
            plan,
            yaw,
            pitch: 0.0,
            nav: Navigator::default(),
            strafe: 1.0,
            next_strafe: 0,
            windups: Vec::new(),
            areas: Vec::new(),
            error: (0.0, 0.0),
            los: None,
            guard_until: 0,
            round: None,
        }
    }

    /// Line of sight to `b`, looked at every few ticks.
    pub fn sees(&mut self, s: &Senses<'_>, b: &Body) -> bool {
        if let Some((id, at, seen)) = self.los
            && id == b.id
            && tick_delta(s.tick, at) < LOS_EVERY as i32
        {
            return seen;
        }
        let seen = s.sees(b);
        self.los = Some((b.id, s.tick, seen));
        seen
    }

    fn aimed(&self, eye: Vec3, point: Vec3, tolerance: f32) -> bool {
        let (yaw, pitch) = angles_to(eye, point);
        angle_diff(yaw, self.yaw).abs() <= tolerance
            && (pitch - self.pitch).abs() <= tolerance * 2.0
    }

    /// A unit wish towards `goal` along the nav grid; zero once within `arrive`.
    fn walk(&mut self, s: &Senses<'_>, goal: Vec3, arrive: f32) -> Vec3 {
        let steer = self
            .nav
            .steer(s.nav, s.world, s.pos(), goal, arrive, s.tick, s.hz);
        if steer.arrived {
            return Vec3::ZERO;
        }
        let me = s.pos();
        let to = (steer.toward - me).truncate().extend(0.0);
        let dir = to.normalize_or_zero();
        let right = Vec3::new(dir.y, -dir.x, 0.0);
        let side = self.nav.sidestep(s.tick);
        if side != 0.0 {
            return (dir * 0.3 + right * side).normalize_or_zero();
        }
        // A body in the way is walked round: the grid knows walls, not who stands in a
        // doorway. (A body at the goal is the goal, not an obstacle.)
        let blocker = s
            .bodies
            .iter()
            .filter(|b| b.alive && b.id != s.id && (b.pos.z - me.z).abs() < 64.0)
            .filter(|b| (b.pos - goal).truncate().length() > arrive.max(48.0))
            .map(|b| (b, (b.pos - me).truncate()))
            .filter(|(_, to_b)| {
                let d = to_b.length();
                d > 1.0 && d < ROUND_RADIUS && to_b.dot(dir.truncate()) / d > 0.35
            })
            .min_by(|a, b| a.1.length().total_cmp(&b.1.length()));
        let Some((b, to_b)) = blocker else {
            self.round = None;
            return dir;
        };
        let side = match self.round {
            Some((id, side, until)) if id == b.id && tick_delta(s.tick, until) < 0 => side,
            _ => {
                // Past it on the side it is not on; where a wall is in the way, the other.
                let off = if to_b.dot(right.truncate()) > 0.0 {
                    -1.0
                } else {
                    1.0
                };
                let side = [off, -off]
                    .into_iter()
                    .find(|side| {
                        let probe = me + right * *side * 44.0 + dir * 28.0;
                        s.nav.walkable_line(s.world, me, probe)
                    })
                    .unwrap_or(off);
                self.round = Some((b.id, side, s.tick.wrapping_add(s.ticks(600))));
                side
            }
        };
        (dir * 0.5 + right * side).normalize_or_zero()
    }

    /// The way out of a harmful area the mind has had time to notice, if it stands in one.
    fn flee_areas(&mut self, s: &Senses<'_>) -> Option<Vec3> {
        let react = s.ticks(REACTION_MS) as i32;
        self.areas
            .retain(|(id, _)| s.areas.iter().any(|a| a.id == *id));
        let me = s.pos();
        let mut out: Option<Vec3> = None;
        for a in s.areas {
            if !a.harmful || (a.spares_owner && a.owner == s.id) {
                continue;
            }
            let since = match self.areas.iter().find(|(id, _)| *id == a.id) {
                Some((_, t)) => *t,
                None => {
                    self.areas.push((a.id, s.tick));
                    s.tick
                }
            };
            let away = (me - a.pos).truncate();
            if away.length() > a.radius + 40.0 || tick_delta(s.tick, since) < react {
                continue;
            }
            let dir = if away.length() > 1.0 {
                away.normalize()
            } else {
                // Dead centre: any way is the short way.
                let (f, _) = yaw_vectors(self.yaw + 180.0);
                f.truncate()
            };
            out = Some(out.unwrap_or(Vec3::ZERO) + dir.extend(0.0));
        }
        out.map(|v| v.normalize_or_zero())
    }

    /// Whether an attack the mind has seen coming for the reaction time will reach it.
    fn threatened(&mut self, s: &Senses<'_>, hostile: &dyn Fn(&Body) -> bool) -> Option<Body> {
        let react = s.ticks(REACTION_MS) as i32;
        let me = s.pos();
        self.windups.retain(|(id, _)| {
            s.body(*id)
                .is_some_and(|b| b.alive && matches!(b.anim, anim::WINDUP | anim::SWING))
        });
        let mut out = None;
        for b in s.bodies {
            if !b.alive || !matches!(b.anim, anim::WINDUP | anim::SWING) || !hostile(b) {
                continue;
            }
            let to_me = (me - b.pos).truncate();
            if to_me.length() > THREAT_REACH
                || b.facing().truncate().dot(to_me.normalize_or_zero()) < 0.3
            {
                continue;
            }
            let since = match self.windups.iter().find(|(id, _)| *id == b.id) {
                Some((_, t)) => *t,
                None => {
                    self.windups.push((b.id, s.tick));
                    s.tick
                }
            };
            if tick_delta(s.tick, since) >= react {
                out = Some(*b);
            }
        }
        out
    }

    /// Whether a hostile in reach is winding up or swinging at the mind, seen or not yet
    /// reacted to: not starting something needs no reaction time.
    fn blow_coming(&self, s: &Senses<'_>, hostile: &dyn Fn(&Body) -> bool) -> bool {
        let me = s.pos();
        s.bodies.iter().any(|b| {
            if !b.alive || !matches!(b.anim, anim::WINDUP | anim::SWING) || !hostile(b) {
                return false;
            }
            let to_me = (me - b.pos).truncate();
            to_me.length() <= THREAT_REACH
                && b.facing().truncate().dot(to_me.normalize_or_zero()) >= 0.3
        })
    }

    /// Keep out of trouble without fighting (a healer with nobody to mend, a leader who has
    /// given its orders): away from the nearest hostile within `keep`, otherwise at `place`.
    pub fn stay_back(
        &mut self,
        s: &Senses<'_>,
        hostile: &dyn Fn(&Body) -> bool,
        place: Vec3,
        keep: f32,
    ) -> Input {
        let me = s.pos();
        let danger = s
            .bodies
            .iter()
            .filter(|b| b.alive && hostile(b))
            .map(|b| (s.dist(b), b))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let mut f = Frame::default();
        match danger {
            Some((d, h)) if d < keep => {
                // Away from it, bent towards the place to be so a wall is not the end of it.
                let away = (me - h.pos).truncate().extend(0.0).normalize_or_zero();
                let home = (place - me).truncate().extend(0.0).normalize_or_zero();
                f.wish = (away + home * 0.6).normalize_or_zero();
                f.aim = Some(h.centre());
            }
            other => {
                if (place - me).truncate().length() > 70.0 {
                    f.wish = self.walk(s, place, 36.0);
                }
                f.aim = other.map(|(_, h)| h.centre());
            }
        }
        if let Some(out) = self.flee_areas(s) {
            f.wish = out;
        }
        self.finish(s, f)
    }

    /// An ally whose body is in the way of a shot from `from` to `to`, with room for the
    /// aim error and for the ally's next step: the corridor widens with the distance.
    fn line_blocked(
        s: &Senses<'_>,
        from: Vec3,
        to: Vec3,
        skip: EntityId,
        ally: &dyn Fn(&Body) -> bool,
    ) -> bool {
        s.bodies.iter().any(|b| {
            if !b.alive || b.id == skip || b.id == s.id || !ally(b) {
                return false;
            }
            // The shot may miss: what stands behind the target counts as in the line too.
            let beyond = to + (to - from).normalize_or_zero() * 180.0;
            let near = closest_point_on_segment(from, beyond, b.centre());
            let along = (near - from).length();
            (near - b.centre()).length() < b.radius() + 20.0 + along * 0.07
        })
    }

    /// An ally inside the wedge a swing of `reach` and `arc` would sweep, with a margin for
    /// where it will be when the swing lands.
    fn wedge_blocked(
        &self,
        s: &Senses<'_>,
        reach: f32,
        arc: f32,
        skip: EntityId,
        ally: &dyn Fn(&Body) -> bool,
    ) -> bool {
        let (fwd, _) = yaw_vectors(self.yaw);
        s.bodies.iter().any(|b| {
            if !b.alive || b.id == skip || b.id == s.id || !ally(b) {
                return false;
            }
            let to = (b.pos - s.pos()).truncate();
            let d = to.length();
            d - b.radius() <= reach + 30.0
                && d > 1.0
                && fwd
                    .truncate()
                    .dot(to / d)
                    .clamp(-1.0, 1.0)
                    .acos()
                    .to_degrees()
                    <= arc * 0.5 + 25.0
        })
    }

    /// Turn within the limit and express the wish in the body's own axes.
    fn finish(&mut self, s: &Senses<'_>, f: Frame) -> Input {
        if let Some(point) = f.aim {
            let (yaw, pitch) = angles_to(s.eye(), point);
            let max = TURN_RATE_DEG_S * s.dt();
            self.yaw = (self.yaw + angle_diff(yaw, self.yaw).clamp(-max, max)).rem_euclid(360.0);
            self.pitch = (self.pitch + (pitch - self.pitch).clamp(-max, max)).clamp(-89.0, 89.0);
        }
        let (fwd, right) = yaw_vectors(self.yaw);
        Input {
            buttons: f.buttons,
            yaw: self.yaw,
            pitch: self.pitch,
            forward: f.wish.dot(fwd).clamp(-1.0, 1.0),
            side: f.wish.dot(right).clamp(-1.0, 1.0),
            ability: f.ability,
            held: 0,
            target: 0,
            use_slot: 0,
        }
    }

    /// Stand, looking at `face` if given.
    pub fn idle(&mut self, s: &Senses<'_>, face: Option<Vec3>) -> Input {
        let mut f = Frame {
            aim: face,
            ..Frame::default()
        };
        if let Some(out) = self.flee_areas(s) {
            f.wish = out;
        }
        self.finish(s, f)
    }

    /// Walk to `goal` (within `arrive`), looking where it goes. Returns the frame and
    /// whether it has arrived.
    pub fn go(&mut self, s: &Senses<'_>, goal: Vec3, arrive: f32) -> (Input, bool) {
        let wish = match self.flee_areas(s) {
            Some(out) => out,
            None => self.walk(s, goal, arrive),
        };
        let arrived = wish == Vec3::ZERO;
        let aim = (!arrived).then(|| s.eye() + wish * 64.0);
        let f = Frame {
            wish,
            aim,
            ..Frame::default()
        };
        (self.finish(s, f), arrived)
    }

    /// Whether the kit has a ready way to fight at range.
    fn ranged_slot(&self) -> Option<(u8, f32, f32, Tick)> {
        match self.plan.shot() {
            Some((
                slot,
                Use::Shot {
                    speed,
                    gravity,
                    mends: false,
                    release,
                },
            )) => Some((slot, speed, gravity, release)),
            _ => None,
        }
    }

    /// What the running script is, by use.
    fn running(&self, s: &Senses<'_>) -> Option<Use> {
        let slot = s.me.script?.ability;
        [self.plan.primary, self.plan.secondary]
            .into_iter()
            .chain(self.plan.actives)
            .flatten()
            .find(|(i, _)| *i == slot)
            .map(|(_, u)| u)
    }

    /// Press a button on a frame where it was not held (activations are edges).
    fn pulse(s: &Senses<'_>, button: u16) -> u16 {
        if s.me.buttons_prev & button == 0 {
            button
        } else {
            0
        }
    }

    /// One frame of fighting `e.target` with the whole kit.
    pub fn fight(&mut self, s: &Senses<'_>, e: &Engage<'_>) -> Input {
        let t = e.target;
        let me = s.pos();
        let eye = s.eye();
        let to = (t.pos - me).truncate();
        let d = to.length();
        let dir = if d > 1.0 {
            (to / d).extend(0.0)
        } else {
            Vec3::X
        };
        let side = Vec3::new(dir.y, -dir.x, 0.0);
        let seen = self.sees(s, t);
        let reach = self.plan.reach();
        let ranged = self.ranged_slot();
        let keep_range = e.range > 0.0 && ranged.is_some();
        let busy = s.me.script.is_some();
        let mut f = Frame {
            aim: Some(t.centre()),
            ..Frame::default()
        };

        if tick_delta(s.tick, self.next_strafe) >= 0 {
            self.strafe = if self.rng.next_f32() < 0.5 { 1.0 } else { -1.0 };
            self.next_strafe = s.tick.wrapping_add(s.hz / 2 + self.rng.below(s.hz));
        }

        // ---- where to stand ----
        let in_reach = d - t.radius() <= reach * 0.85;
        if !seen {
            f.wish = self.walk(s, t.pos, (reach * 0.7).max(40.0));
        } else if keep_range {
            if d > e.range + 60.0 {
                f.wish = self.walk(s, t.pos, e.range);
            } else if d < e.range - 110.0 {
                f.wish = -dir + side * (self.strafe * 0.5);
            } else {
                f.wish = side * (self.strafe * 0.5);
            }
        } else if !in_reach {
            f.wish = self.walk(s, t.pos, (reach * 0.7 + t.radius()).max(40.0));
        } else if e.flank {
            // In its face, or inside the sweep of its swing: work round to its back.
            let facing_me = t.facing().truncate().dot(-to.normalize_or_zero());
            if facing_me > -0.3 {
                // Round the target by the shorter way to its back: `side` is the
                // counter-clockwise tangent, taken when the back lies that way.
                let way = if to.perp_dot(t.facing().truncate()) >= 0.0 {
                    1.0
                } else {
                    -1.0
                };
                f.wish = side * way + dir * 0.15;
            }
        } else {
            f.wish = side * (self.strafe * 0.25);
        }

        // ---- the tether: a held post or a leash ----
        if let Some((post, radius)) = e.tether {
            let out = (me - post).truncate().length();
            if out > radius {
                f.wish = self.walk(s, post, radius * 0.5);
            } else if out > radius - 24.0 && f.wish.dot((me - post).normalize_or_zero()) > 0.0 {
                f.wish = Vec3::ZERO;
            }
        }

        // ---- what is coming ----
        if let Some(out) = self.flee_areas(s) {
            f.wish = out;
        }
        let threat = self.threatened(s, e.hostile);
        let mut guarding = false;
        match self.plan.guard {
            GuardPlan::Block { cost, .. } => {
                if let Some(th) = threat {
                    if s.me.stamina >= cost {
                        self.guard_until = s.tick.wrapping_add(s.ticks(120));
                        f.aim = Some(th.centre());
                    } else {
                        // A shield arm that cannot pay for the next blow steps out of its
                        // reach instead of having the guard broken.
                        f.wish = (s.pos() - th.pos)
                            .truncate()
                            .extend(0.0)
                            .normalize_or_zero();
                        f.aim = Some(th.centre());
                    }
                }
                if tick_delta(s.tick, self.guard_until) < 0 && !busy {
                    f.buttons |= buttons::GUARD;
                    guarding = true;
                }
            }
            GuardPlan::Parry { slot, .. } => {
                if let Some(th) = threat
                    && !busy
                    && s.ready(slot)
                {
                    // A creature's swing is public knowledge: wait for its end. A stranger's
                    // is answered on reaction.
                    let windup = th.creature.and_then(|c| {
                        let def = s.pack.creatures.get(c as usize)?;
                        KitPlan::swing_windup(s.pack, def.build.primary)
                    });
                    let since = self
                        .windups
                        .iter()
                        .find(|(id, _)| *id == th.id)
                        .map_or(0, |(_, at)| tick_delta(s.tick, *at));
                    let due = windup.is_none_or(|w| since + s.ticks(110) as i32 >= w as i32);
                    if due {
                        f.aim = Some(th.centre());
                        f.buttons |= Self::pulse(s, buttons::GUARD);
                        guarding = true;
                    }
                }
            }
            GuardPlan::None => {}
        }

        // A body with a shield does not open itself while a blow it could block is on its
        // way: it starts nothing until the swing has landed (and answers in the recovery).
        // The same body swings only into an opening: while the target faces it within
        // reach and is not recovering, casting or staggered, it waits behind its shield.
        let shield = matches!(self.plan.guard, GuardPlan::Block { .. });
        let facing_me = t.facing().truncate().dot(-to.normalize_or_zero()) >= 0.3;
        let opening = !facing_me
            || d > THREAT_REACH
            || matches!(
                t.anim,
                anim::RECOVER | anim::CAST | anim::STAGGER | anim::DEAD
            );
        let hold_back = shield && (self.blow_coming(s, e.hostile) || !opening);

        // ---- a script already running: keep it on target ----
        if busy {
            match self.running(s) {
                Some(Use::Shot {
                    speed,
                    gravity,
                    mends: false,
                    ..
                }) => {
                    let (ey, ep) = self.error;
                    let p = lead(eye, t, speed, gravity);
                    if Self::line_blocked(s, eye, p, t.id, e.ally) {
                        // A friend stepped into the line after the trigger was pulled: the
                        // shot goes over everybody's head instead.
                        f.aim = Some(eye + dir * 64.0 + Vec3::Z * 64.0);
                        return self.finish(s, f);
                    }
                    let dist = (p - eye).length();
                    f.aim = Some(
                        p + side * (ey.to_radians().tan() * dist)
                            + Vec3::Z * (ep.to_radians().tan() * dist),
                    );
                }
                Some(Use::Aimed { .. }) => {
                    f.aim = Some(e.artillery.unwrap_or(t).feet() + Vec3::Z * 8.0)
                }
                _ => {}
            }
            return self.finish(s, f);
        }
        if guarding || hold_back {
            return self.finish(s, f);
        }

        // ---- the kit: actives from the highest slot, then the primary, then the secondary ----
        let near_hostiles = |radius: f32| {
            s.bodies
                .iter()
                .filter(|b| b.alive && (e.hostile)(b) && (b.pos - me).truncate().length() <= radius)
                .count()
        };
        let my_id = s.id;
        let near_allies = |centre: Vec3, radius: f32| {
            s.bodies
                .iter()
                .filter(|b| {
                    b.alive
                        && b.id != my_id
                        && (e.ally)(b)
                        && (b.pos - centre).truncate().length() <= radius + b.radius() + 24.0
                })
                .count()
        };
        for (i, slot) in self.plan.actives.into_iter().enumerate().rev() {
            let Some((slot, use_)) = slot else { continue };
            if !s.ready(slot) {
                continue;
            }
            let go = match use_ {
                Use::Buff => seen && d < 600.0 && !self.buffed(s, slot),
                Use::Burst {
                    radius,
                    harmful: true,
                } => near_hostiles(radius * 0.8) >= 1 && near_allies(me, radius) == 0,
                Use::Burst {
                    radius,
                    harmful: false,
                } => seen && d < 500.0 && near_allies(me, radius) >= 1 && !self.buffed(s, slot),
                Use::Placed {
                    ahead,
                    radius,
                    harmful: true,
                } => {
                    let at = me + dir * ahead;
                    seen && (d - ahead).abs() < radius * 0.6
                        && self.aimed(eye, t.centre(), 12.0)
                        && near_allies(at, radius) == 0
                }
                Use::Aimed {
                    range,
                    radius,
                    harmful: true,
                } => {
                    let at = e.artillery.unwrap_or(t);
                    let point = at.feet() + Vec3::Z * 8.0;
                    let ok = (at.id != t.id || seen)
                        && s.dist(at) <= range * 0.95
                        && near_allies(at.pos, radius) == 0;
                    if ok {
                        f.aim = Some(point);
                    }
                    ok && self.aimed(eye, point, 6.0)
                }
                Use::Shot {
                    speed,
                    gravity,
                    mends: false,
                    ..
                } => {
                    let p = lead(eye, t, speed, gravity);
                    seen && (140.0..800.0).contains(&d)
                        && self.aimed(eye, p, 5.0)
                        && !Self::line_blocked(s, eye, p, t.id, e.ally)
                }
                Use::Swing { reach, arc, .. } => {
                    d - t.radius() <= reach * 0.9
                        && self.aimed(eye, t.centre(), arc * 0.3)
                        && !self.wedge_blocked(s, reach, arc, t.id, e.ally)
                }
                Use::Closer => {
                    if keep_range {
                        // A way out: dash along the wish, which already points away.
                        d < 150.0 && seen
                    } else {
                        seen && (220.0..520.0).contains(&d)
                            && self.aimed(eye, t.centre(), 15.0)
                            && {
                                f.wish = dir;
                                true
                            }
                    }
                }
                Use::Blink { distance } => {
                    !keep_range
                        && seen
                        && (distance * 0.9..distance * 1.8).contains(&d)
                        && self.aimed(eye, t.centre(), 10.0)
                }
                // Helpful areas and mending shots are the healer's business (`mend`).
                Use::Placed { harmful: false, .. }
                | Use::Aimed { harmful: false, .. }
                | Use::Shot { mends: true, .. } => false,
            };
            if go {
                f.ability = i as u8 + 1;
                return self.finish(s, f);
            }
        }
        if let Some((slot, Use::Swing { reach, arc, .. })) = self.plan.swing()
            && s.ready(slot)
            && d - t.radius() <= reach
            && self.aimed(eye, t.centre(), arc * 0.35)
            && !self.wedge_blocked(s, reach, arc, t.id, e.ally)
        {
            f.buttons |= Self::pulse(s, KitPlan::button_for(slot));
            return self.finish(s, f);
        }
        if let Some((slot, speed, gravity, _)) = ranged
            && seen
            && s.ready(slot)
            && d - t.radius() > reach + 20.0
            && d < 950.0
        {
            let p = lead(eye, t, speed, gravity);
            f.aim = Some(p);
            if self.aimed(eye, p, 3.0) {
                if Self::line_blocked(s, eye, p, t.id, e.ally) {
                    // Step aside for a clear line rather than shoot a friend in the back.
                    f.wish = side * self.strafe;
                } else {
                    self.error = (
                        self.rng.range_f32(-1.0, 1.0) * AIM_ERROR_DEG * 1.7,
                        self.rng.range_f32(-1.0, 1.0) * AIM_ERROR_DEG * 1.7,
                    );
                    f.buttons |= Self::pulse(s, KitPlan::button_for(slot));
                }
            }
        }
        self.finish(s, f)
    }

    /// Whether the status a self-buff in `slot` gives is already on the mind.
    fn buffed(&self, s: &Senses<'_>, slot: u8) -> bool {
        use gm_core::vocab::Verb;
        s.sheet.kit.abilities[slot as usize]
            .steps
            .iter()
            .any(|st| match &st.verb {
                Verb::ApplyStatus(a) => s.me.statuses.has(a.status),
                Verb::AreaEffect(a) => a.effects.iter().any(|e| s.me.statuses.has(e.status)),
                _ => false,
            })
    }

    /// A healing circle at the mind's own feet, when the kit has one ready: look down, cast.
    /// `None` when there is nothing to cast.
    pub fn circle_self(&mut self, s: &Senses<'_>) -> Option<Input> {
        let busy = s.me.script.is_some();
        let running = matches!(self.running(s), Some(Use::Aimed { harmful: false, .. }));
        let slot = self.plan.actives.into_iter().position(
            |a| matches!(a, Some((slot, Use::Aimed { harmful: false, .. })) if s.ready(slot)),
        );
        if !(running || (!busy && slot.is_some())) {
            return None;
        }
        let (fwd, _) = yaw_vectors(self.yaw);
        let point = s.pos() + fwd * 24.0 + Vec3::Z * gm_core::trace::Hull::Player.mins().z;
        let mut f = Frame {
            aim: Some(point),
            ..Frame::default()
        };
        if let Some(out) = self.flee_areas(s) {
            f.wish = out;
        }
        if !busy
            && self.aimed(s.eye(), point, 8.0)
            && let Some(i) = slot
        {
            f.ability = i as u8 + 1;
        }
        Some(self.finish(s, f))
    }

    /// One frame of healing `ally` (COMPANIONS.md 4): keep it in sight and in range, stay out
    /// of the nearest hostile's reach, mend it; a circle when it is badly hurt.
    pub fn mend(&mut self, s: &Senses<'_>, ally: &Body, hostile: &dyn Fn(&Body) -> bool) -> Input {
        let me = s.pos();
        let eye = s.eye();
        let d = s.dist(ally);
        let seen = self.sees(s, ally);
        let busy = s.me.script.is_some();
        let mut f = Frame {
            aim: Some(ally.centre()),
            ..Frame::default()
        };
        // Position: in sight, within 420 u, and not within 220 u of anything hostile.
        let danger = s
            .bodies
            .iter()
            .filter(|b| b.alive && hostile(b))
            .map(|b| (s.dist(b), b))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some((hd, h)) = danger
            && hd < 220.0
        {
            f.wish = (me - h.pos).truncate().extend(0.0).normalize_or_zero();
        } else if !seen || d > 420.0 {
            f.wish = self.walk(s, ally.pos, 200.0);
        }
        if let Some(out) = self.flee_areas(s) {
            f.wish = out;
        }
        if busy {
            match self.running(s) {
                Some(Use::Shot {
                    speed,
                    gravity,
                    mends: true,
                    ..
                }) => {
                    f.aim = Some(lead(eye, ally, speed, gravity));
                }
                Some(Use::Aimed { harmful: false, .. }) => {
                    f.aim = Some(ally.feet() + Vec3::Z * 8.0)
                }
                _ => {}
            }
            return self.finish(s, f);
        }
        let hurt = ally.health_for(s.party).unwrap_or(1000);
        // The circle for a body in real trouble, the dart otherwise; never on a regenerating
        // body that is nearly whole (the credit is only for damage mended).
        for (i, slot) in self.plan.actives.into_iter().enumerate() {
            if let Some((
                slot,
                Use::Aimed {
                    range,
                    harmful: false,
                    ..
                },
            )) = slot
                && s.ready(slot)
                && seen
                && hurt < 550
                && d <= range * 0.95
            {
                let p = ally.feet() + Vec3::Z * 8.0;
                f.aim = Some(p);
                if self.aimed(eye, p, 6.0) {
                    f.ability = i as u8 + 1;
                }
                return self.finish(s, f);
            }
        }
        if let Some((
            slot,
            Use::Shot {
                speed,
                gravity,
                mends: true,
                ..
            },
        )) = self.plan.secondary
            && s.ready(slot)
            && seen
            && hurt < 800
            && !ally.has(Status::Regen)
            && d < 700.0
        {
            let p = lead(eye, ally, speed, gravity);
            f.aim = Some(p);
            // Anyone in the way gets the dart instead: friend or foe, it would be wasted.
            let blocked = s.bodies.iter().any(|b| {
                b.alive
                    && b.id != ally.id
                    && b.id != s.id
                    && (closest_point_on_segment(eye, p, b.centre()) - b.centre()).length()
                        < b.radius() + 10.0
            });
            if blocked {
                let dir = (ally.pos - me).truncate().extend(0.0).normalize_or_zero();
                f.wish = Vec3::new(dir.y, -dir.x, 0.0) * self.strafe;
            } else if self.aimed(eye, p, 2.5) {
                f.buttons |= Self::pulse(s, buttons::SECONDARY);
            }
        }
        self.finish(s, f)
    }
}

impl KitPlan {
    /// The windup of the swing in pack ability `index`, when it is one: what a mind may know
    /// about a creature's primary from the public content.
    pub fn swing_windup(pack: &gm_core::build::ContentPack, index: u16) -> Option<Tick> {
        use gm_core::vocab::Verb;
        let ab = &pack.abilities.get(index as usize)?.ability;
        let step = ab.steps.first()?;
        match &step.verb {
            Verb::MeleeArc(m) => Some(step.at + m.timing.windup),
            _ => None,
        }
    }
}
