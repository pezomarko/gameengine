//! The RPG mode on the client (MODES.md 5): a target, a click on the ground that becomes
//! a walk on the nav grid, and target-actions that wait for the body to be in range.
//! Everything here makes ordinary frames: the zone sees inputs, the prediction and the
//! ledger are untouched; only the aim at the target is the zone's.

use glam::{Mat4, Vec3};
use gm_ai::nav::{NavGrid, Navigator};
use gm_core::build::Kit;
use gm_core::geom::ray_capsule;
use gm_core::movement::yaw_vectors;
use gm_core::sim::{Mover, buttons, capsule_at, sees};
use gm_core::trace::{CollisionWorld, Hull};
use gm_core::vocab::ArchetypeFrame;

/// What the pointer is over in the RPG mode (MODES.md 5.5), which picks the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hover {
    /// The ground, or nothing: a click walks there.
    Ground,
    /// A body that is not the target: a click targets it.
    Body,
    /// The target: a click is the primary at it, a right tap the secondary.
    Attack,
}

/// What a key asks for with a target (MODES.md 5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Primary,
    Secondary,
    /// An active slot, 1-based.
    Active(u8),
}

/// A body the RPG client may target or walk around: as the frame draws it.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub id: u32,
    /// The hull origin.
    pub origin: Vec3,
    pub frame: ArchetypeFrame,
    /// Crouched (MODES.md 3.5): its capsule is `CROUCH_DROP` shorter.
    pub crouched: bool,
    pub enemy: bool,
}

impl Body {
    pub fn capsule(&self) -> gm_core::geom::Capsule {
        capsule_at(self.origin, Hull::Player, self.frame, self.crouched)
    }

    pub fn centre(&self) -> Vec3 {
        self.capsule().center()
    }
}

/// The frame a wire index names (`gm_model::rig::frame_index` the other way).
pub fn frame_of(index: u8) -> ArchetypeFrame {
    match index {
        0 => ArchetypeFrame::Colossus,
        2 => ArchetypeFrame::Caster,
        3 => ArchetypeFrame::Infiltrator,
        _ => ArchetypeFrame::Striker,
    }
}

/// The camera's distance behind the body (MODES.md 5.5), and its bounds.
pub const DIST_DEFAULT: f32 = 240.0;
pub const DIST_MIN: f32 = 120.0;
pub const DIST_MAX: f32 = 400.0;
pub const CAMERA_UP: f32 = 90.0;
/// How far a click reaches into the world.
const PICK_REACH: f32 = 4096.0;
/// Within this of the goal a walk is over.
const ARRIVE: f32 = 24.0;

#[derive(Default)]
pub struct Rpg {
    pub target: Option<u32>,
    /// Where a click on the ground sends the body.
    pub walk: Option<Vec3>,
    /// The action waiting for the body to be in range of its target.
    pub act: Option<(Act, u32)>,
    /// The bodies Tab has been through since it last came round.
    cycled: Vec<u32>,
    nav: Option<NavGrid>,
    navigator: Navigator,
    pub dist: f32,
}

/// What a frame's input is made of in the RPG mode.
#[derive(Clone, Copy, Debug, Default)]
pub struct RpgFrame {
    pub forward: f32,
    pub side: f32,
    pub buttons: u16,
    pub ability: u8,
    pub target: u32,
}

impl Rpg {
    pub fn new() -> Rpg {
        Rpg {
            dist: DIST_DEFAULT,
            ..Rpg::default()
        }
    }

    /// Another map: its grid is built again at the next click.
    pub fn forget_map(&mut self) {
        self.nav = None;
        self.navigator.clear();
        self.walk = None;
    }

    pub fn clear_target(&mut self) {
        self.target = None;
        self.act = None;
    }

    /// Movement keys: the walk and the waiting action end, the target stays.
    pub fn moved_by_hand(&mut self) {
        self.walk = None;
        self.act = None;
        self.navigator.clear();
    }

    /// The ray from the camera through a pixel, from the last frame's view-projection.
    pub fn ray(vp: Mat4, size: (f32, f32), cursor: (f32, f32)) -> Option<(Vec3, Vec3)> {
        let inv = vp.inverse();
        let x = cursor.0 / size.0.max(1.0) * 2.0 - 1.0;
        let y = 1.0 - cursor.1 / size.1.max(1.0) * 2.0;
        let near = inv * glam::Vec4::new(x, y, 0.0, 1.0);
        let far = inv * glam::Vec4::new(x, y, 1.0, 1.0);
        if near.w.abs() < 1e-6 || far.w.abs() < 1e-6 {
            return None;
        }
        let near = near.truncate() / near.w;
        let far = far.truncate() / far.w;
        Some((near, (far - near).normalize_or_zero()))
    }

    /// A left click (MODES.md 5.2, 5.5): the body under the pointer becomes the target;
    /// else the ground there becomes where the body walks.
    /// The body under the pointer: the nearest capsule the ray meets before a wall.
    pub fn pick(world: &dyn CollisionWorld, from: Vec3, dir: Vec3, bodies: &[Body]) -> Option<u32> {
        let wall = world.trace(Hull::Point, from, from + dir * PICK_REACH);
        let reach = PICK_REACH * wall.fraction;
        let mut best: Option<(f32, u32)> = None;
        for b in bodies {
            let cap = b.capsule();
            if let Some(t) = ray_capsule(from, dir, reach, &cap)
                && best.is_none_or(|(bt, _)| t < bt)
            {
                best = Some((t, b.id));
            }
        }
        best.map(|(_, id)| id)
    }

    /// What the pointer would do where it is (MODES.md 5.5): the cursor says so.
    pub fn hover(&self, under: Option<u32>) -> Hover {
        match under {
            Some(id) if self.target == Some(id) => Hover::Attack,
            Some(_) => Hover::Body,
            None => Hover::Ground,
        }
    }

    /// A left click (MODES.md 5.2, 5.5): on a body not yet the target, it is targeted;
    /// on the target, the primary at it (Tales of Pirates, Ether Saga: the first click
    /// picks, the next attacks; the director, 2026-10-08); on the ground, a walk there.
    pub fn click(&mut self, world: &dyn CollisionWorld, from: Vec3, dir: Vec3, bodies: &[Body]) {
        if let Some(id) = Self::pick(world, from, dir, bodies) {
            if self.target == Some(id) {
                self.ask(Act::Primary);
            } else {
                self.clear_target();
                self.target = Some(id);
            }
            return;
        }
        let wall = world.trace(Hull::Point, from, from + dir * PICK_REACH);
        let reach = PICK_REACH * wall.fraction;
        if wall.fraction < 1.0 {
            // Just short of the surface, then down to the floor under it.
            let at = from + dir * (reach - 4.0).max(0.0);
            let down = world.trace(Hull::Point, at, at - Vec3::Z * 1024.0);
            let goal = if down.start_solid { at } else { down.end };
            self.walk = Some(goal + Vec3::Z * (-Hull::Player.mins().z));
            self.act = None;
            self.navigator.clear();
        }
    }

    /// Tab (MODES.md 5.2): the nearest enemy in sight not yet cycled; when every one has
    /// been, the round starts again.
    pub fn cycle(&mut self, candidates: &[(u32, f32)]) {
        let mut sorted: Vec<(u32, f32)> = candidates.to_vec();
        sorted.sort_by(|a, b| a.1.total_cmp(&b.1));
        let next = sorted
            .iter()
            .find(|(id, _)| !self.cycled.contains(id) && Some(*id) != self.target)
            .or_else(|| {
                self.cycled.clear();
                sorted.iter().find(|(id, _)| Some(*id) != self.target)
            })
            .map(|(id, _)| *id);
        if let Some(id) = next {
            self.cycled.push(id);
            self.clear_target();
            self.target = Some(id);
        }
    }

    /// A key with a target (MODES.md 5.3): the action waits for the range and is pressed
    /// once; another press is another action. Nothing repeats by itself: the pace of a
    /// fight is the player's (the director, 2026-10-08).
    pub fn ask(&mut self, act: Act) {
        let Some(t) = self.target else { return };
        self.act = Some((act, t));
        self.walk = None;
    }

    /// The target as the frame last saw it is gone (dead, left, out of sight too long).
    pub fn lost(&mut self, present: impl Fn(u32) -> bool) {
        if let Some(t) = self.target
            && !present(t)
        {
            self.clear_target();
        }
        if let Some((_, t)) = self.act
            && !present(t)
        {
            self.act = None;
        }
    }

    /// The input of one tick (MODES.md 5.3): WASD as pressed, else the walk to the goal
    /// or toward the target the waiting action needs; the action's press when the target
    /// is within its range and in sight. `yaw` is the camera's, what the axes are about.
    #[allow(clippy::too_many_arguments)]
    pub fn frame(
        &mut self,
        world: &dyn CollisionWorld,
        kit: &Kit,
        mover: &Mover,
        bodies: &[Body],
        axes: (f32, f32),
        yaw: f32,
        tick: u32,
        hz: u32,
    ) -> RpgFrame {
        let mut out = RpgFrame {
            target: self.target.unwrap_or(0),
            ..RpgFrame::default()
        };
        if axes.0 != 0.0 || axes.1 != 0.0 {
            self.moved_by_hand();
            out.forward = axes.0;
            out.side = axes.1;
            return out;
        }
        let pos = mover.mv.origin;
        // The waiting action: in range, it is pressed; else the body walks to its target.
        let mut goal: Option<Vec3> = self.walk;
        if let Some((act, target)) = self.act {
            let slot = match act {
                Act::Primary => kit.primary,
                Act::Secondary => kit.secondary,
                Act::Active(n) => kit.actives.get(n as usize - 1).copied().flatten(),
            };
            let body = bodies.iter().find(|b| b.id == target);
            match (slot, body) {
                (Some(slot), Some(b)) => {
                    let ab = &kit.abilities[slot as usize];
                    let range = if ab.range > 0.0 { ab.range } else { 64.0 };
                    let centre = b.centre();
                    let near = (centre - pos).truncate().length() <= range * 0.95;
                    let seen = sees(world, mover.eye(), centre);
                    let ready = gm_core::sim::tick_delta(tick, mover.cooldowns[slot as usize]) >= 0;
                    if near && seen {
                        if ready && mover.script.is_none() {
                            match act {
                                Act::Primary => out.buttons |= buttons::PRIMARY,
                                Act::Secondary => out.buttons |= buttons::SECONDARY,
                                Act::Active(n) => out.ability = n,
                            }
                            self.act = None;
                        }
                        goal = None;
                    } else {
                        goal = Some(b.origin);
                    }
                }
                _ => self.act = None,
            }
        }
        if let Some(g) = goal {
            let nav = self
                .nav
                .get_or_insert_with(|| NavGrid::build(world, &[pos]));
            let steer = self.navigator.steer(nav, world, pos, g, ARRIVE, tick, hz);
            if steer.arrived || steer.blocked {
                if self.walk == Some(g) {
                    self.walk = None;
                }
                if steer.blocked {
                    self.act = None;
                }
            } else {
                let wish = (steer.toward - pos).truncate().normalize_or_zero();
                let (fwd, right) = yaw_vectors(yaw);
                let side_step = self.navigator.sidestep(tick);
                out.forward = wish.dot(fwd.truncate()).clamp(-1.0, 1.0);
                out.side = (wish.dot(right.truncate()) + side_step).clamp(-1.0, 1.0);
            }
        }
        out
    }
}

/// The orbit camera (MODES.md 5.5): `dist` behind the body's centre and `CAMERA_UP` over
/// it, along the camera's own yaw and pitch, pulled in front of any wall.
pub fn orbit_camera(
    world: &dyn CollisionWorld,
    centre: Vec3,
    yaw: f32,
    pitch: f32,
    dist: f32,
) -> Vec3 {
    let fwd = gm_core::sim::view_dir(yaw, pitch);
    let desired = centre - fwd * dist + Vec3::Z * CAMERA_UP * (dist / DIST_DEFAULT);
    let tr = world.trace(Hull::Point, centre, desired);
    if tr.fraction < 1.0 {
        let back = (centre - tr.end).normalize_or_zero();
        tr.end + back * 6.0
    } else {
        desired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gm_core::collide::BoxWorld;

    fn dummy(id: u32, x: f32, y: f32) -> Body {
        Body {
            id,
            origin: Vec3::new(x, y, 0.0),
            frame: ArchetypeFrame::Striker,
            crouched: false,
            enemy: true,
        }
    }

    #[test]
    fn the_first_click_targets_and_the_next_attacks() {
        let world = BoxWorld::floor();
        let bodies = [dummy(7, 200.0, 0.0), dummy(8, 300.0, 200.0)];
        let from = Vec3::new(0.0, 0.0, 40.0);
        let at = |b: &Body| (b.centre() - from).normalize();
        let mut rpg = Rpg::new();
        assert_eq!(Rpg::pick(&world, from, at(&bodies[0]), &bodies), Some(7));
        assert_eq!(rpg.hover(Some(7)), Hover::Body);
        rpg.click(&world, from, at(&bodies[0]), &bodies);
        assert_eq!(rpg.target, Some(7));
        assert_eq!(rpg.act, None, "the first click only picks");
        assert_eq!(rpg.hover(Some(7)), Hover::Attack);
        assert_eq!(rpg.hover(Some(8)), Hover::Body);
        assert_eq!(rpg.hover(None), Hover::Ground);
        rpg.click(&world, from, at(&bodies[0]), &bodies);
        assert_eq!(
            rpg.act,
            Some((Act::Primary, 7)),
            "the next is the primary at it"
        );
        // Another body: a new target, the waiting action let go.
        rpg.click(&world, from, at(&bodies[1]), &bodies);
        assert_eq!(rpg.target, Some(8));
        assert_eq!(rpg.act, None);
        // The ground: a walk, the target kept.
        rpg.click(&world, from, Vec3::new(0.6, 0.0, -0.8).normalize(), &bodies);
        assert!(rpg.walk.is_some());
        assert_eq!(rpg.target, Some(8));
    }
}
