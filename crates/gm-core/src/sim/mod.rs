//! The shared zone simulation: players, abilities, projectiles, areas, statuses, guards,
//! melee with lag compensation, deaths and respawns, in the tick order of VOCABULARY.md 7 and
//! the contract of PROTOCOL.md 7.
//!
//! [`step_mover`] is the part both sides run: the client predicts its own [`Mover`] with it and
//! the server runs it for everyone. [`Zone`] is the authoritative container the server drives;
//! it also runs in tests without any networking.

pub mod aim;
mod mover;
pub mod test_content;
mod zone;

#[cfg(test)]
mod tests;

pub use mover::{
    Action, CAST_ANIM_NONE, CREEP_SPEED, CROUCH_DROP, Company, Dash, GuardState, GunState, Input,
    ItemRefusal, Mover, Nearby, RUN_SPEED, SCOPED_CONE, Script, anim, bar_cell, buttons,
    capsule_at, command_exit_ticks, cone_deg, item_refusal, kit_use_ticks, melee_hit_point,
    move_share, sees, step_mover, view_dir, yaw_toward,
};
pub use zone::{
    Area, DOT_INTERVAL_TICKS, Driver, HEAD_BAND, History, HitKind, INSTANT_AREA_ECHO_MS,
    MAX_CLAIMED_VIEW_LAG, MAX_ENTITY_ID, Player, Projectile, Shot, Spawn, Swing, Zone, ZoneEvent,
    script_anim,
};

/// Diminishing returns on controls (MODES.md 4.5): the window within which the second of a
/// kind lasts half and the third does nothing.
pub const CONTROL_WINDOW_MS: u32 = 10_000;

use crate::tick::Tick;

/// Lag compensation bound (PROTOCOL.md 7.4): 13 ticks ≈ 200 ms at 64 Hz.
pub const MAX_REWIND_TICKS: Tick = 13;
/// Interpolation delay a legitimate client adds on top of its one-way latency (PROTOCOL.md 4).
pub const REWIND_ALLOWANCE_TICKS: Tick = 8;
/// Rewind (13) + the longest windup and active window; PROTOCOL.md 7.4.
pub const HISTORY_TICKS: usize = 32;
/// Frame ledger (PROTOCOL.md 4).
pub const MAX_FRAMES_PER_TICK: u32 = 3;
pub const CREDIT_BURST: f32 = 8.0;
pub const MAX_QUEUED_FRAMES: usize = 32;
/// Frames held back so one tick of arrival jitter never starves the simulation (one tick of
/// added latency).
pub const RESERVE_FRAMES: usize = 1;
/// Queue depth from which two frames run per tick to drain a burst.
pub const DRAIN_DEPTH: usize = 4;
/// A projectile ignores its owner's capsule this long after spawning (VOCABULARY.md 5.2).
pub const PROJECTILE_OWNER_GRACE: Tick = 2;
pub const RESPAWN_MS: u32 = 3000;
/// Seven slotted (MATRIX.md 9) and what comes with them: a chain's stages, a knife
/// (MODES.md 4.3, 3.7).
pub const MAX_ABILITIES: usize = 12;
/// Stamina regeneration pauses this long after a spend (MATRIX.md 6).
pub const REGEN_PAUSE_MS: u32 = 1000;
/// A kit's use, from the press to the heal (MODES.md 11.3).
pub const KIT_USE_MS: u32 = 1500;
/// The item bar's cells (LOOK.md 3.2): four, after the abilities.
pub const BAR_CELLS: usize = 4;
/// Standing up from the command stance takes this long (COMPANIONS.md 5.1).
pub const COMMAND_EXIT_MS: u32 = 400;
/// The team of creatures in a wild zone (COMPANIONS.md 3.1).
pub const TEAM_WILD: u8 = 3;

/// Wrapping tick difference `a - b`.
pub fn tick_delta(a: Tick, b: Tick) -> i32 {
    a.wrapping_sub(b) as i32
}
