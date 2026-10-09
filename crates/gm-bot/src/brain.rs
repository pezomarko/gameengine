//! The scripted player. Pure: it reads a [`View`] of the world and returns one tick of input.

use glam::Vec3;
use gm_core::build::{ContentPack, Kit};
use gm_core::matrix::Aspects;
use gm_core::movement::yaw_vectors;
use gm_core::rng::Rng;
use gm_core::sim::{Input, Mover, anim, buttons, tick_delta};
use gm_core::vocab::{
    ArchetypeFrame, Guard, MeleeArc, MoveKind, Origin, Shape, StatusTarget, Verb,
};
use gm_net::client::RenderEntity;
use gm_net::snapshot::EntityKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Behaviour {
    /// Stand still, face the nearest enemy, swing when it is in reach.
    Hold,
    /// Run around, jump now and then, shoot and swing at whoever is nearest.
    Wander,
    /// Chase the nearest enemy: primary in reach, secondary otherwise.
    Hunter,
    /// Use the whole kit: melee kits close in and guard, ranged kits kite, actives fire when
    /// their shape says so. The arena acceptance test runs this on both sides.
    Duelist,
    /// A town: walk a while, stand a while, stay near where one arrived, never fight.
    Stroll,
    /// A dungeon: lead a squad through the creature posts of the map (`raid::Raid` thinks
    /// instead of this brain).
    Raid,
    /// A sparring partner for a town: the duelist's whole kit, but it stands where it arrived
    /// and fights only when struck at (a blow taken, or an enemy winding up within
    /// `SPAR_SIGHT`), for `SPAR_SECS` after the last; it never follows further than
    /// `SPAR_LEASH` from home (back it walks, then it stands again). Strollers, who never
    /// swing, are left alone whatever their team.
    Spar,
}

/// How near an enemy must be for its windup to wake a sparring partner.
const SPAR_SIGHT: f32 = 420.0;
/// How far a sparring partner follows a fight from home.
const SPAR_LEASH: f32 = 260.0;
/// How long a sparring partner keeps fighting after the last blow or windup aimed at it.
const SPAR_SECS: u32 = 8;

/// How far a strolling bot goes from where it arrived before it turns back.
const STROLL_LEASH: f32 = 420.0;

/// What the brain sees this tick.
pub struct View<'a> {
    pub me: &'a Mover,
    pub kit: &'a Kit,
    pub frame: ArchetypeFrame,
    pub team: u8,
    pub alive: bool,
    /// Own health as last read from the zone (a sparring partner wakes when it drops).
    pub health: i32,
    /// Other entities at the render time (players, projectiles, areas).
    pub others: &'a [RenderEntity],
    /// The brain's tick counter (the mover's frame clock).
    pub tick: u32,
}

/// Rough classification of an active for the duelist.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ActiveUse {
    Buff,
    /// Shockwave at the feet with this radius.
    Burst(f32),
    /// Area placed ahead at this distance.
    Placed(f32),
    Shot,
    Swing(f32),
    Closer,
    /// A charge at this speed (u/s) for this many ticks.
    Charge(f32, u32),
    Blink,
}

fn classify(kit: &Kit, slot: u8) -> Option<ActiveUse> {
    let ab = &kit.abilities[slot as usize];
    let step = ab.steps.first()?;
    Some(match &step.verb {
        Verb::ApplyStatus(s) if s.target == StatusTarget::Actor => ActiveUse::Buff,
        Verb::ApplyStatus(_) => return None,
        Verb::AreaEffect(a) => {
            let radius = match a.shape {
                Shape::Sphere { radius } | Shape::Cylinder { radius, .. } => radius,
                Shape::Cone { length, .. } => length,
                Shape::Box { half_extents } => half_extents[0],
            };
            match a.origin {
                Origin::Weapon { offset } => ActiveUse::Placed(offset[0] + radius * 0.5),
                _ => ActiveUse::Burst(radius),
            }
        }
        Verb::Projectile(_) => ActiveUse::Shot,
        Verb::MeleeArc(m) => ActiveUse::Swing(m.reach),
        Verb::MoveSelf(m) => match m.kind {
            MoveKind::Blink { .. } => ActiveUse::Blink,
            // A charge covers speed x duration and stops on the hit: used from where it
            // lands on the target, not from the dash's window.
            MoveKind::Charge {
                speed, duration, ..
            } => ActiveUse::Charge(speed, duration),
            _ => ActiveUse::Closer,
        },
        Verb::Guard(_) => return None,
    })
}

/// The swing among the two weapon slots, with its button (the range is the weapon's,
/// MATRIX.md 10: a primary is a blade or a bow).
fn swing_of(kit: &Kit) -> Option<(&MeleeArc, u16)> {
    [
        (kit.primary, buttons::PRIMARY),
        (kit.secondary, buttons::SECONDARY),
    ]
    .into_iter()
    .find_map(
        |(slot, button)| match &kit.abilities[slot? as usize].steps.first()?.verb {
            Verb::MeleeArc(m) => Some((m, button)),
            _ => None,
        },
    )
}

/// The harmful shot among the two weapon slots: its kit slot and button.
pub fn shot_of(kit: &Kit) -> Option<(u8, u16)> {
    [
        (kit.primary, buttons::PRIMARY),
        (kit.secondary, buttons::SECONDARY),
    ]
    .into_iter()
    .find_map(|(slot, button)| {
        let i = slot?;
        match &kit.abilities[i as usize].steps.first()?.verb {
            Verb::Projectile(p) if p.damage.amount > 0 => Some((i, button)),
            _ => None,
        }
    })
}

pub struct Brain {
    rng: Rng,
    pub behaviour: Behaviour,
    yaw: f32,
    pitch: f32,
    next_turn: u32,
    next_shot: u32,
    jump_until: u32,
    strafe: f32,
    next_strafe: u32,
    /// Ticks per second of the zone (stroll legs are timed in seconds).
    pub hz: u32,
    /// Where a strolling bot first stood.
    home: Option<Vec3>,
    /// A place to walk to and stay at, whatever the behaviour (a vendor's tile).
    pub goal: Option<Vec3>,
    /// Progress towards the goal: the distance a second ago, and a sidestep while stuck.
    goal_check: (u32, f32),
    sidestep_until: u32,
    /// A sparring partner fights until this tick, and the health it last saw of itself.
    spar_until: u32,
    last_health: i32,
}

impl Brain {
    pub fn new(seed: u64, behaviour: Behaviour) -> Brain {
        let mut rng = Rng::new(seed);
        let yaw = rng.range_f32(0.0, 360.0);
        Brain {
            rng,
            behaviour,
            yaw,
            pitch: 0.0,
            next_turn: 64,
            next_shot: 90,
            jump_until: 0,
            strafe: 1.0,
            next_strafe: 0,
            hz: 64,
            home: None,
            goal: None,
            goal_check: (0, f32::MAX),
            sidestep_until: 0,
            spar_until: 0,
            last_health: i32::MAX,
        }
    }

    fn nearest<'a>(
        &self,
        me: Vec3,
        team: u8,
        others: &'a [RenderEntity],
    ) -> Option<(&'a RenderEntity, f32)> {
        others
            .iter()
            .filter(|e| e.kind == EntityKind::Player && e.alive())
            .filter(|e| team == 0 || e.team() == 0 || e.team() != team)
            .map(|e| (e, (e.pos - me).length()))
            .min_by(|a, b| a.1.total_cmp(&b.1))
    }

    fn face(&mut self, me_eye: Vec3, target: Vec3) {
        let to = target + Vec3::new(0.0, 0.0, 28.0) - me_eye;
        let horiz = to.truncate().length();
        self.yaw = to.y.atan2(to.x).to_degrees().rem_euclid(360.0);
        self.pitch = (-to.z).atan2(horiz).to_degrees().clamp(-89.0, 89.0);
    }

    pub fn think(&mut self, v: &View<'_>) -> Input {
        let me = v.me.mv.origin;
        let eye = v.me.eye();
        let tick = v.tick;
        let mut buttons = 0u16;
        let mut forward = 0.0f32;
        let mut side = 0.0f32;
        let mut ability = 0u8;
        if let Some(goal) = self.goal {
            if !v.alive {
                return self.input(buttons, forward, side, ability);
            }
            let to = goal - me;
            let d = to.truncate().length();
            if d > 12.0 {
                self.yaw = to.y.atan2(to.x).to_degrees().rem_euclid(360.0);
                self.pitch = 0.0;
                forward = if d > 60.0 { 1.0 } else { 0.5 };
                // No closer than a second ago: something is in the way; step around it.
                if tick >= self.goal_check.0 {
                    if self.goal_check.1 - d < 8.0 {
                        self.sidestep_until = tick + self.hz / 2;
                        self.strafe = if self.rng.below(2) == 0 { 1.0 } else { -1.0 };
                    }
                    self.goal_check = (tick + self.hz, d);
                }
                if tick < self.sidestep_until {
                    side = self.strafe;
                    forward = 0.2;
                }
            }
            return self.input(buttons, forward, side, ability);
        }
        // Taunted (MATRIX.md 8): the taunter is the one to fight while it lasts.
        let nearest = match v.me.statuses.taunted_by() {
            Some(by) => v
                .others
                .iter()
                .find(|e| e.id == by && e.kind == EntityKind::Player && e.alive())
                .map(|e| (e, (e.pos - me).length()))
                .or_else(|| self.nearest(me, v.team, v.others)),
            None => self.nearest(me, v.team, v.others),
        };
        let (reach, swing) =
            swing_of(v.kit).map_or((70.0, buttons::PRIMARY), |(m, b)| (m.reach, b));
        let shot = shot_of(v.kit);
        match self.behaviour {
            // A raid leader that lost its `raid::Raid` (it never does) stands still.
            Behaviour::Raid => {}
            Behaviour::Hold => {
                if let Some((e, d)) = nearest {
                    self.face(eye, e.pos);
                    if d < reach && tick.is_multiple_of(24) {
                        buttons |= swing;
                    }
                }
            }
            Behaviour::Stroll => {
                // Home is where the bot first stood alive (before the first snapshot the
                // mover is still at the origin).
                if !v.alive {
                    return self.input(buttons, forward, side, ability);
                }
                let home = *self.home.get_or_insert(me);
                if tick >= self.next_turn {
                    self.pitch = 0.0;
                    let away = me - home;
                    self.yaw = if away.truncate().length() > STROLL_LEASH {
                        // Too far: head back, roughly.
                        (-away.y).atan2(-away.x).to_degrees() + self.rng.range_f32(-30.0, 30.0)
                    } else {
                        self.rng.range_f32(0.0, 360.0)
                    }
                    .rem_euclid(360.0);
                    // One leg in three is a pause.
                    self.strafe = if self.rng.below(3) == 0 { 0.0 } else { 1.0 };
                    self.next_turn = tick + self.hz * 2 + self.rng.below(self.hz * 5);
                }
                forward = 0.6 * self.strafe;
            }
            Behaviour::Wander => {
                if tick >= self.next_turn {
                    self.yaw = self.rng.range_f32(0.0, 360.0);
                    self.pitch = 0.0;
                    self.next_turn = tick + 64 + self.rng.below(128);
                }
                forward = 1.0;
                if self.rng.next_f32() < 0.02 {
                    self.jump_until = tick + 2;
                }
                if tick < self.jump_until {
                    buttons |= buttons::JUMP;
                }
                if let Some((e, d)) = nearest {
                    if d < reach && tick.is_multiple_of(24) {
                        self.face(eye, e.pos);
                        buttons |= swing;
                    } else if tick >= self.next_shot
                        && let Some((_, button)) = shot
                    {
                        self.face(eye, e.pos);
                        buttons |= button;
                        self.next_shot = tick + 96 + self.rng.below(64);
                    }
                }
            }
            Behaviour::Hunter => {
                if let Some((e, d)) = nearest {
                    self.face(eye, e.pos);
                    if d > 50.0 {
                        forward = 1.0;
                    }
                    if d < reach {
                        if tick.is_multiple_of(20) {
                            buttons |= swing;
                        }
                    } else if d > 150.0
                        && tick >= self.next_shot
                        && let Some((_, button)) = shot
                    {
                        buttons |= button;
                        self.next_shot = tick + 96;
                    }
                } else {
                    forward = 1.0;
                    if tick >= self.next_turn {
                        self.yaw = self.rng.range_f32(0.0, 360.0);
                        self.next_turn = tick + 96;
                    }
                }
            }
            Behaviour::Duelist | Behaviour::Spar => {
                let spar = self.behaviour == Behaviour::Spar;
                // Ranged is the weapon's, not the frame's (MATRIX.md 10): a caster with a
                // staff closes in, a striker with a crossbow or a musket keeps away.
                let ranged = v.kit.primary.is_some_and(|i| {
                    matches!(
                        v.kit.abilities[i as usize].steps.first().map(|s| &s.verb),
                        Some(Verb::Projectile(_))
                    )
                });
                let home = if spar && v.alive {
                    Some(*self.home.get_or_insert(me))
                } else {
                    None
                };
                let nearest = if spar {
                    // Struck at? A blow taken, or a windup aimed from near by.
                    let struck = v.health < self.last_health
                        || nearest.is_some_and(|(e, d)| {
                            d < SPAR_SIGHT && matches!(e.anim, anim::WINDUP | anim::CAST)
                        });
                    self.last_health = v.health;
                    if struck && v.alive {
                        self.spar_until = tick + SPAR_SECS * self.hz;
                    }
                    nearest.filter(|_| tick < self.spar_until)
                } else {
                    nearest
                };
                let Some((e, d)) = nearest else {
                    if let Some(home) = home {
                        // Nobody near: stand at home, walk back when a fight carried us off.
                        let away = me - home;
                        if away.truncate().length() > 40.0 {
                            self.yaw = (-away.y).atan2(-away.x).to_degrees().rem_euclid(360.0);
                            self.pitch = 0.0;
                            forward = 0.6;
                        }
                        return self.input(buttons, forward, side, ability);
                    }
                    // Nobody in sight: patrol.
                    forward = 1.0;
                    if tick >= self.next_turn {
                        self.yaw = self.rng.range_f32(0.0, 360.0);
                        self.pitch = 0.0;
                        self.next_turn = tick + 96;
                    }
                    return self.input(buttons, forward, side, ability);
                };
                self.face(eye, e.pos);
                let ready = |slot: u8| tick_delta(tick, v.me.cooldowns[slot as usize]) >= 0;
                let busy = v.me.script.is_some();
                let enemy_winding = matches!(e.anim, anim::WINDUP | anim::CAST);
                // Movement: melee closes, ranged kites; everyone strafes a little.
                if tick >= self.next_strafe {
                    self.strafe = if self.rng.next_f32() < 0.5 { 1.0 } else { -1.0 };
                    self.next_strafe = tick + 40 + self.rng.below(60);
                }
                let want = if ranged { 320.0 } else { reach * 0.7 };
                if d > want + 20.0 {
                    forward = 1.0;
                } else if ranged && d < want - 80.0 {
                    forward = -1.0;
                }
                if d < 500.0 {
                    side = self.strafe * 0.6;
                }
                // At the leash a sparring partner stops advancing (it still turns and swings).
                if let Some(home) = home
                    && forward > 0.0
                    && (me - home).truncate().length() > SPAR_LEASH
                {
                    forward = 0.0;
                }
                // Guard: block or parry a wind-up in reach.
                if !busy
                    && d < reach + 40.0
                    && enemy_winding
                    && let Some(g) = v.kit.guard_verb()
                {
                    match g {
                        Guard::Block(_) => buttons |= buttons::GUARD,
                        Guard::Parry(_) => {
                            if let Some(gi) = v.kit.guard
                                && ready(gi)
                                && tick.is_multiple_of(2)
                            {
                                buttons |= buttons::GUARD;
                            }
                        }
                    }
                }
                // Actives, highest slot first so the expensive ones get used.
                if !busy && buttons & buttons::GUARD == 0 {
                    for (i, slot) in v.kit.actives.iter().enumerate().rev() {
                        let Some(slot) = *slot else { continue };
                        if !ready(slot) {
                            continue;
                        }
                        let ab = &v.kit.abilities[slot as usize];
                        if v.me.stamina < ab.cost.stamina as f32
                            || v.me.focus < ab.cost.focus as f32
                        {
                            continue;
                        }
                        let use_it = match classify(v.kit, slot) {
                            Some(ActiveUse::Buff) => d < 450.0,
                            Some(ActiveUse::Burst(r)) => d < r * 0.8,
                            Some(ActiveUse::Placed(at)) => d < at + 60.0 && d > 40.0,
                            Some(ActiveUse::Shot) => (120.0..700.0).contains(&d),
                            Some(ActiveUse::Swing(r)) => d < r,
                            Some(ActiveUse::Closer) => {
                                if ranged {
                                    d < 140.0 && {
                                        forward = -1.0;
                                        true
                                    }
                                } else {
                                    (200.0..600.0).contains(&d)
                                }
                            }
                            Some(ActiveUse::Charge(speed, ticks)) => {
                                let cover = speed * ticks as f32 / self.hz.max(1) as f32;
                                !ranged && d > reach && d < cover + reach * 0.5
                            }
                            Some(ActiveUse::Blink) => {
                                if ranged {
                                    d < 120.0 && {
                                        // Blink away: turn around for the blink tick.
                                        self.yaw = (self.yaw + 180.0).rem_euclid(360.0);
                                        true
                                    }
                                } else {
                                    (260.0..600.0).contains(&d)
                                }
                            }
                            None => false,
                        };
                        if use_it {
                            ability = i as u8 + 1;
                            break;
                        }
                    }
                }
                if ability == 0 && !busy && buttons & buttons::GUARD == 0 {
                    if d < reach && tick.is_multiple_of(4) {
                        buttons |= swing;
                    } else if d > reach
                        && d < 900.0
                        && tick >= self.next_shot
                        && let Some((si, button)) = shot
                        && ready(si)
                    {
                        buttons |= button;
                        self.next_shot = tick + 8;
                    }
                }
            }
        }
        self.input(buttons, forward, side, ability)
    }

    fn input(&self, buttons: u16, forward: f32, side: f32, ability: u8) -> Input {
        Input {
            buttons,
            yaw: self.yaw,
            pitch: self.pitch,
            forward,
            side,
            ability,
            held: 0,
            target: 0,
            use_slot: 0,
        }
    }
}

/// Facing vectors for tests and the arena driver.
pub fn facing(yaw: f32) -> (Vec3, Vec3) {
    yaw_vectors(yaw)
}

/// The preset that best counters what the enemy is fielding, from the matrix alone: for each
/// preset, the product of its elements' multipliers into the enemy aspects over the enemy
/// elements' multipliers into the preset's aspects. `enemy` is the aspect mask seen most often
/// among enemies. Returns `None` when nothing beats the current build by a margin.
pub fn counter_pick(pack: &ContentPack, current: &str, enemy: Aspects) -> Option<String> {
    if enemy.count() == 0 {
        return None;
    }
    let score = |aspects: Aspects| -> f32 {
        // Best element we can bring against them, over the worst they can bring against us.
        let offence: f32 = aspects
            .iter()
            .map(|e| enemy.mult_against(e))
            .fold(0.0, f32::max);
        let defence: f32 = enemy
            .iter()
            .map(|e| aspects.mult_against(e))
            .fold(0.0, f32::max);
        offence / defence.max(1e-3)
    };
    let current_score = pack.build(current).map_or(0.0, |b| score(b.aspects));
    // Among presets that score the same (they bring the same aspects), the first listed.
    let best = pack
        .builds
        .iter()
        .map(|nb| (nb, score(nb.build.aspects)))
        .reduce(|best, next| if next.1 > best.1 { next } else { best })?;
    (best.0.name != current && best.1 > current_score * 1.5).then(|| best.0.name.clone())
}

/// The aspect mask seen most often among living enemies.
pub fn dominant_enemy_aspects(team: u8, others: &[RenderEntity]) -> Aspects {
    let mut counts: Vec<(u8, u32)> = Vec::new();
    for e in others {
        if e.kind != EntityKind::Player || !e.alive() {
            continue;
        }
        let gm_net::snapshot::SpawnInfo::Player {
            team: t, aspects, ..
        } = e.spawn
        else {
            continue;
        };
        if team != 0 && t == team {
            continue;
        }
        match counts.iter_mut().find(|(a, _)| *a == aspects) {
            Some((_, n)) => *n += 1,
            None => counts.push((aspects, 1)),
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map_or(Aspects::NONE, |(a, _)| Aspects(a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::matrix::Element;
    use gm_core::sim::test_content;
    use gm_core::tick::TickRate;

    #[test]
    fn counter_pick_follows_the_matrix() {
        let pack = test_content::pack(TickRate::COMBAT);
        // Against ironclads (Ground): frostweaver (Water beats Ground; Ground into
        // Water + Air is 0.5x), from a blade (Water alone: 2x in, 1x taken) as well.
        assert_eq!(
            counter_pick(&pack, "blade", Aspects::one(Element::Ground)).as_deref(),
            Some("frostweaver")
        );
        // Against a Water + Air team: mender (Electric is 4x into both).
        assert_eq!(
            counter_pick(
                &pack,
                "ironclad",
                Aspects::two(Element::Water, Element::Air)
            )
            .as_deref(),
            Some("mender")
        );
        // Against blades (Water): the shaman (Grass 2x in, Water 0.5x back; MATRIX.md 17),
        // over the frostweaver (its Air is 1x in, Water is 0.5x back) and the mender
        // (Electric 2x in, 1x back), and a frostweaver re-specs to it.
        assert_eq!(
            counter_pick(&pack, "ironclad", Aspects::one(Element::Water)).as_deref(),
            Some("shaman")
        );
        assert_eq!(
            counter_pick(&pack, "frostweaver", Aspects::one(Element::Water)).as_deref(),
            Some("shaman")
        );
        // Against a Grass team: nothing in the roster is its predator but the shade's Air
        // (Fire has no build); the shade scores 2x in, 0.5x back, and a frostweaver (its
        // Water 0.5x in, Air 1x back) goes to it.
        assert_eq!(
            counter_pick(&pack, "frostweaver", Aspects::one(Element::Grass)).as_deref(),
            Some("shade")
        );
        // Already the counter: nothing to change.
        assert_eq!(
            counter_pick(&pack, "frostweaver", Aspects::one(Element::Ground)),
            None
        );
        assert_eq!(counter_pick(&pack, "blade", Aspects::NONE), None);
    }
}
