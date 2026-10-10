//! Delta-compressed snapshots (PROTOCOL.md 5).

use crate::bits::{BitReader, BitWriter};
use crate::{Kind, NetError, read_header, write_header};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EntityKind {
    Player = 0,
    Projectile = 1,
    /// A pulsing area effect (cosmetic on the client).
    Area = 2,
}

/// First-sight information, by kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpawnInfo {
    Player {
        /// Archetype frame: 0 colossus, 1 striker, 2 caster, 3 infiltrator.
        frame: u8,
        /// 0 = none, 1 or 2.
        team: u8,
        /// Bitmask over `gm_core::matrix::Element` indices (5 bits).
        aspects: u8,
        /// `gm_core::matrix::ArmourClass` index (2 bits).
        armour: u8,
    },
    Projectile {
        owner: u32,
        /// Index into the owner's kit.
        def: u32,
        /// The owner's client tick that fired it.
        input_tick: u32,
    },
    Area {
        owner: u32,
        /// Index into the owner's kit (0 for areas triggered by projectiles or parries).
        def: u32,
        /// Largest extent in whole units.
        radius: u32,
        /// It hurts who stands in it (`AreaEffect::harmful`): what its look says.
        harmful: bool,
    },
}

impl SpawnInfo {
    pub fn kind(&self) -> EntityKind {
        match self {
            SpawnInfo::Player { .. } => EntityKind::Player,
            SpawnInfo::Projectile { .. } => EntityKind::Projectile,
            SpawnInfo::Area { .. } => EntityKind::Area,
        }
    }
}

/// Entity flag bits (PROTOCOL.md 5).
pub mod flags {
    pub const ALIVE: u16 = 1 << 0;
    pub const ON_GROUND: u16 = 1 << 1;
    pub const GUARDING: u16 = 1 << 2;
    pub const DASHING: u16 = 1 << 3;
    pub const JUMP_HELD: u16 = 1 << 4;
    /// An ability script is running (own entity: the client drops a predicted script the
    /// server has interrupted).
    pub const SCRIPT: u16 = 1 << 5;
    /// A parry window is open or its whiff recovery runs (own entity: a successful parry on
    /// the server ends the window at once, and the client must follow).
    pub const PARRY: u16 = 1 << 6;
    /// In the command stance (own entity: a client whose prediction disagrees adopts the
    /// server's state, COMPANIONS.md 5.1).
    pub const COMMANDING: u16 = 1 << 7;
    /// Crouched (MODES.md 3.5, v14): the body is drawn in its squat and its hitbox is
    /// `CROUCH_DROP` shorter; every body carries it.
    pub const CROUCHED: u16 = 1 << 8;
    /// In the RPG mode (MODES.md 5.1, v15): the body does not turn with its camera, so a
    /// client draws it standing the way it last went or was turned, not the frame's yaw.
    pub const RPG: u16 = 1 << 9;
    /// How many bits of flags the wire carries.
    pub const BITS: u32 = 10;
}

mod mask {
    pub const SPAWN: u16 = 1 << 0;
    pub const POS: u16 = 1 << 1;
    pub const YAW: u16 = 1 << 2;
    pub const PITCH: u16 = 1 << 3;
    pub const VEL: u16 = 1 << 4;
    pub const ANIM: u16 = 1 << 5;
    pub const HEALTH: u16 = 1 << 6;
    pub const FLAGS: u16 = 1 << 7;
    pub const STATUS: u16 = 1 << 8;
    pub const BITS: u32 = 9;
}

/// One entity as seen by one client at one tick, in wire units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntityState {
    pub id: u32,
    pub spawn: SpawnInfo,
    /// Quantized position (1/4 u).
    pub pos: [i32; 3],
    pub yaw: u16,
    pub pitch: u16,
    /// Quantized velocity (1/8 u/s); own entity only.
    pub vel: Option<[i32; 3]>,
    pub anim: u8,
    /// The ability the stance belongs to (`AbilityId`; 0: none), sent with a stance that
    /// is a script's (v9): what is swung or cast, so a client can draw where it lands.
    pub acting: u16,
    /// Own entity (Phase 5: party) only.
    pub health: Option<u16>,
    /// `flags`: `BITS` bits on the wire.
    pub flags: u16,
    /// Active statuses, a bit per `gm_core::vocab::Status` index (cosmetic for others);
    /// `STATUS_BITS` of them on the wire.
    pub status: u32,
}

/// The width of an entity's status mask on the wire (PROTOCOL.md 5): room for the
/// seventeen statuses of MATRIX.md 8 and a few more.
pub const STATUS_BITS: u32 = 24;

/// One of the own entity's statuses (PROTOCOL.md 5, own block).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StatusWire {
    /// `gm_core::vocab::Status` index.
    pub status: u8,
    /// Frame ticks left, relative to `last_input_tick`.
    pub remaining: u32,
    /// Magnitude at 1/16 resolution.
    pub magnitude: f32,
    pub stacks: u8,
    /// Who put it there (an entity id; 0 for nobody): a Taunt turns the body to its source
    /// on both sides of the wire (MATRIX.md 8).
    pub source: u32,
}

/// The own entity's resources and statuses, sent in full every snapshot (they are small and
/// the client must adopt them exactly).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OwnState {
    pub stamina: u16,
    pub focus: u16,
    pub statuses: Vec<StatusWire>,
    /// The firearms of a gun build (MODES.md 3.8): the primary's and the secondary's
    /// magazine and reserve, and whether the one in hand is being reloaded. `None` for a
    /// build without one: a bit.
    pub guns: Option<GunsWire>,
    /// The item bar (LOOK.md 3.2, MODES.md 11.3), in every mode: how many of each cell's
    /// stack the body carries, and the cell in use, 1-based, 0 for none. v18 (v12 carried
    /// the kits as one count and one bit).
    pub bar: [u16; gm_core::sim::BAR_CELLS],
    pub using: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GunsWire {
    pub magazine: [u8; 2],
    pub reserve: [u16; 2],
    pub reloading: bool,
}

pub const MAX_OWN_STATUSES: usize = 8;

/// The **reconstructed** entity table at one tick (PROTOCOL.md 5): what the server tracks per
/// client and what the client rebuilds. Encoding writes only the records that differ from the
/// baseline; decoding carries the rest forward.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub server_tick: u32,
    /// 0 = full snapshot.
    pub baseline_tick: u32,
    pub last_input_tick: u32,
    pub own: OwnState,
    /// Every entity the client knows after this snapshot, ascending by id.
    pub entities: Vec<EntityState>,
    /// Ids present in the baseline and gone now.
    pub removed: Vec<u32>,
}

impl Snapshot {
    pub fn new(server_tick: u32) -> Self {
        Snapshot {
            server_tick,
            ..Default::default()
        }
    }

    pub fn find(&self, id: u32) -> Option<&EntityState> {
        self.entities
            .binary_search_by_key(&id, |e| e.id)
            .ok()
            .map(|i| &self.entities[i])
    }

    /// Sort entities by id and fill `removed` from `baseline`; call after building the table.
    pub fn normalize(&mut self, baseline: Option<&Snapshot>) {
        self.entities.sort_unstable_by_key(|e| e.id);
        self.removed.clear();
        let Some(b) = baseline else {
            return;
        };
        // Both lists ascend: one merged walk finds the baseline ids missing now.
        let mut i = 0;
        for be in &b.entities {
            while i < self.entities.len() && self.entities[i].id < be.id {
                i += 1;
            }
            if i >= self.entities.len() || self.entities[i].id != be.id {
                self.removed.push(be.id);
            }
        }
    }

    /// Encode against `baseline`, which must be the snapshot at `self.baseline_tick` (or
    /// `None` when that is 0). Entities must be ascending by id and `removed` must list every
    /// baseline entity missing from `entities` (see `normalize`). Records identical to their
    /// baseline record are not written at all.
    pub fn encode(&self, baseline: Option<&Snapshot>) -> Vec<u8> {
        debug_assert_eq!(
            baseline.map_or(0, |b| b.server_tick),
            self.baseline_tick,
            "baseline mismatch"
        );
        debug_assert!(self.entities.windows(2).all(|w| w[0].id < w[1].id));
        let mut w = BitWriter::with_capacity(32 + self.entities.len() * 8);
        write_header(&mut w, Kind::Snapshot);
        w.write_bits(self.server_tick as u64, 32);
        w.write_bits(self.baseline_tick as u64, 32);
        w.write_bits(self.last_input_tick as u64, 32);
        write_own(&mut w, &self.own);
        // Both tables ascend by id: one merged walk pairs each record with its baseline.
        let base_entities: &[EntityState] = baseline.map_or(&[], |b| &b.entities);
        let mut changed: Vec<(&EntityState, Option<&EntityState>)> =
            Vec::with_capacity(self.entities.len());
        let mut j = 0;
        for e in &self.entities {
            while j < base_entities.len() && base_entities[j].id < e.id {
                j += 1;
            }
            let base =
                (j < base_entities.len() && base_entities[j].id == e.id).then(|| &base_entities[j]);
            if base.is_none_or(|b| record_differs(e, b)) {
                changed.push((e, base));
            }
        }
        w.write_uvar(changed.len() as u64);
        for (e, base) in changed {
            write_entity(&mut w, e, base);
        }
        w.write_uvar(self.removed.len() as u64);
        for &id in &self.removed {
            w.write_uvar(id as u64);
        }
        w.finish()
    }

    /// Bytes `encode` would produce, without allocating the payload twice.
    pub fn encoded_len(&self, baseline: Option<&Snapshot>) -> usize {
        self.encode(baseline).len()
    }

    /// Decode; `baseline` resolves a tick to the snapshot decoded earlier.
    pub fn decode<'a, F>(bytes: &[u8], baseline: F) -> Result<Snapshot, NetError>
    where
        F: Fn(u32) -> Option<&'a Snapshot>,
    {
        let mut r = BitReader::new(bytes);
        if read_header(&mut r)? != Kind::Snapshot {
            return Err(NetError::Malformed("not a snapshot datagram"));
        }
        let server_tick = r.read_bits(32)? as u32;
        let baseline_tick = r.read_bits(32)? as u32;
        let base = if baseline_tick == 0 {
            None
        } else {
            Some(baseline(baseline_tick).ok_or(NetError::UnknownBaseline(baseline_tick))?)
        };
        let last_input_tick = r.read_bits(32)? as u32;
        let own = read_own(&mut r)?;
        let count = r.read_uvar32()? as usize;
        if count > 4096 {
            return Err(NetError::Malformed("too many entities"));
        }
        let mut listed = Vec::with_capacity(count);
        let mut prev: Option<u32> = None;
        for _ in 0..count {
            let id = r.read_uvar32()?;
            if prev.is_some_and(|p| id <= p) {
                return Err(NetError::Malformed("entity ids not ascending"));
            }
            prev = Some(id);
            let base_e = base.and_then(|b| b.find(id));
            listed.push(read_entity(&mut r, id, base_e)?);
        }
        let removed_count = r.read_uvar32()? as usize;
        if removed_count > 4096 {
            return Err(NetError::Malformed("too many removals"));
        }
        let mut removed = Vec::with_capacity(removed_count);
        for _ in 0..removed_count {
            removed.push(r.read_uvar32()?);
        }
        // Reconstruction: baseline minus removed, listed records applied, the rest carried
        // forward unchanged.
        let mut entities: Vec<EntityState> = match base {
            Some(b) => b
                .entities
                .iter()
                .filter(|e| !removed.contains(&e.id))
                .copied()
                .collect(),
            None => Vec::new(),
        };
        for e in listed {
            match entities.binary_search_by_key(&e.id, |x| x.id) {
                Ok(i) => entities[i] = e,
                Err(i) => entities.insert(i, e),
            }
        }
        Ok(Snapshot {
            server_tick,
            baseline_tick,
            last_input_tick,
            own,
            entities,
            removed,
        })
    }
}

const MAGNITUDE_SCALE: f32 = 16.0;

fn write_own(w: &mut BitWriter, own: &OwnState) {
    w.write_uvar(own.stamina as u64);
    w.write_uvar(own.focus as u64);
    let n = own.statuses.len().min(MAX_OWN_STATUSES);
    w.write_bits(n as u64, 4);
    for s in &own.statuses[..n] {
        w.write_bits(s.status as u64 & 0x1f, 5);
        w.write_uvar(s.remaining as u64);
        w.write_svar((s.magnitude * MAGNITUDE_SCALE).round() as i64);
        w.write_bits(s.stacks.min(7) as u64, 3);
        w.write_uvar(s.source as u64);
    }
    match own.guns {
        None => w.write_bits(0, 1),
        Some(g) => {
            w.write_bits(1, 1);
            for i in 0..2 {
                w.write_uvar(g.magazine[i] as u64);
                w.write_uvar(g.reserve[i] as u64);
            }
            w.write_bits(g.reloading as u64, 1);
        }
    }
    for n in own.bar {
        w.write_uvar(n as u64);
    }
    w.write_bits(own.using as u64, 3);
}

fn read_own(r: &mut BitReader<'_>) -> Result<OwnState, NetError> {
    let stamina = u16::try_from(r.read_uvar()?).map_err(|_| NetError::Malformed("stamina"))?;
    let focus = u16::try_from(r.read_uvar()?).map_err(|_| NetError::Malformed("focus"))?;
    let n = r.read_bits(4)? as usize;
    if n > MAX_OWN_STATUSES {
        return Err(NetError::Malformed("too many own statuses"));
    }
    let mut statuses = Vec::with_capacity(n);
    for _ in 0..n {
        let status = r.read_bits(5)? as u8;
        let remaining = r.read_uvar32()?;
        let magnitude = r.read_svar32()? as f32 / MAGNITUDE_SCALE;
        let stacks = r.read_bits(3)? as u8;
        let source = r.read_uvar32()?;
        statuses.push(StatusWire {
            status,
            remaining,
            magnitude,
            stacks,
            source,
        });
    }
    let guns = if r.read_bits(1)? == 1 {
        let mut g = GunsWire::default();
        for i in 0..2 {
            g.magazine[i] =
                u8::try_from(r.read_uvar()?).map_err(|_| NetError::Malformed("magazine"))?;
            g.reserve[i] =
                u16::try_from(r.read_uvar()?).map_err(|_| NetError::Malformed("reserve"))?;
        }
        g.reloading = r.read_bits(1)? == 1;
        Some(g)
    } else {
        None
    };
    let mut bar = [0u16; gm_core::sim::BAR_CELLS];
    for n in &mut bar {
        *n = u16::try_from(r.read_uvar()?).map_err(|_| NetError::Malformed("bar"))?;
    }
    let using = r.read_bits(3)? as u8;
    if using as usize > gm_core::sim::BAR_CELLS {
        return Err(NetError::Malformed("using out of range"));
    }
    Ok(OwnState {
        bar,
        using,
        stamina,
        focus,
        statuses,
        guns,
    })
}

fn record_differs(e: &EntityState, b: &EntityState) -> bool {
    e.spawn != b.spawn
        || e.pos != b.pos
        || e.yaw != b.yaw
        || e.pitch != b.pitch
        || (e.vel.is_some() && e.vel != b.vel)
        || e.anim != b.anim
        || e.acting != b.acting
        || (e.health.is_some() && e.health != b.health)
        // No longer sent (the body left the viewer's party): see `write_entity`.
        || (e.health.is_none() && b.health.is_some())
        || e.flags != b.flags
        || e.status != b.status
}

fn write_i32x3(w: &mut BitWriter, v: [i32; 3], base: Option<[i32; 3]>) {
    for i in 0..3 {
        let d = base.map_or(v[i], |b| v[i].wrapping_sub(b[i]));
        w.write_svar(d as i64);
    }
}

fn read_i32x3(r: &mut BitReader<'_>, base: Option<[i32; 3]>) -> Result<[i32; 3], NetError> {
    let mut out = [0i32; 3];
    for (i, o) in out.iter_mut().enumerate() {
        let d = r.read_svar32()?;
        *o = base.map_or(d, |b| b[i].wrapping_add(d));
    }
    Ok(out)
}

fn write_entity(w: &mut BitWriter, e: &EntityState, base: Option<&EntityState>) {
    // A health that is no longer sent cannot be unsaid by a delta: a reader carries
    // forward what the baseline had. Such a record is written whole, as a new one is, and
    // a whole record without a health is read without one (PARTY.md 2: the body left the
    // viewer's party; before parties changed, nothing ever stopped sending a health).
    let base = base.filter(|b| e.health.is_some() || b.health.is_none());
    w.write_uvar(e.id as u64);
    let mut m = 0u16;
    match base {
        None => {
            m |= mask::SPAWN | mask::POS | mask::YAW | mask::PITCH | mask::ANIM | mask::FLAGS;
            if e.vel.is_some() {
                m |= mask::VEL;
            }
            if e.health.is_some() {
                m |= mask::HEALTH;
            }
            if e.status != 0 {
                m |= mask::STATUS;
            }
        }
        Some(b) => {
            if e.pos != b.pos {
                m |= mask::POS;
            }
            if e.yaw != b.yaw {
                m |= mask::YAW;
            }
            if e.pitch != b.pitch {
                m |= mask::PITCH;
            }
            if e.vel.is_some() && e.vel != b.vel {
                m |= mask::VEL;
            }
            if e.anim != b.anim || e.acting != b.acting {
                m |= mask::ANIM;
            }
            if e.health.is_some() && e.health != b.health {
                m |= mask::HEALTH;
            }
            if e.flags != b.flags {
                m |= mask::FLAGS;
            }
            if e.status != b.status {
                m |= mask::STATUS;
            }
        }
    }
    w.write_bits(m as u64, mask::BITS);
    if m & mask::SPAWN != 0 {
        w.write_bits(e.spawn.kind() as u64, 4);
        match e.spawn {
            SpawnInfo::Player {
                frame,
                team,
                aspects,
                armour,
            } => {
                w.write_bits(frame as u64, 2);
                w.write_bits(team as u64, 2);
                w.write_bits(aspects as u64 & 0x3f, 6);
                w.write_bits(armour as u64, 2);
            }
            SpawnInfo::Projectile {
                owner,
                def,
                input_tick,
            } => {
                w.write_uvar(owner as u64);
                w.write_uvar(def as u64);
                w.write_uvar(input_tick as u64);
            }
            SpawnInfo::Area {
                owner,
                def,
                radius,
                harmful,
            } => {
                w.write_uvar(owner as u64);
                w.write_uvar(def as u64);
                w.write_uvar(radius as u64);
                w.write_bits(harmful as u64, 1);
            }
        }
    }
    if m & mask::POS != 0 {
        write_i32x3(w, e.pos, base.map(|b| b.pos));
    }
    if m & mask::YAW != 0 {
        w.write_bits(e.yaw as u64, 12);
    }
    if m & mask::PITCH != 0 {
        w.write_bits(e.pitch as u64, 11);
    }
    if m & mask::VEL != 0 {
        write_i32x3(w, e.vel.unwrap_or([0; 3]), base.and_then(|b| b.vel));
    }
    if m & mask::ANIM != 0 {
        w.write_bits(e.anim as u64, 8);
        if gm_core::sim::anim::acts(e.anim) {
            w.write_uvar(e.acting as u64);
        }
    }
    if m & mask::HEALTH != 0 {
        w.write_uvar(e.health.unwrap_or(0) as u64);
    }
    if m & mask::FLAGS != 0 {
        w.write_bits(e.flags as u64, flags::BITS);
    }
    if m & mask::STATUS != 0 {
        w.write_bits(e.status as u64, STATUS_BITS);
    }
}

fn read_entity(
    r: &mut BitReader<'_>,
    id: u32,
    base: Option<&EntityState>,
) -> Result<EntityState, NetError> {
    let m = r.read_bits(mask::BITS)? as u16;
    let spawn = if m & mask::SPAWN != 0 {
        match r.read_bits(4)? {
            0 => SpawnInfo::Player {
                frame: r.read_bits(2)? as u8,
                team: r.read_bits(2)? as u8,
                aspects: r.read_bits(6)? as u8,
                armour: r.read_bits(2)? as u8,
            },
            1 => SpawnInfo::Projectile {
                owner: r.read_uvar32()?,
                def: r.read_uvar32()?,
                input_tick: r.read_uvar32()?,
            },
            2 => SpawnInfo::Area {
                owner: r.read_uvar32()?,
                def: r.read_uvar32()?,
                radius: r.read_uvar32()?,
                harmful: r.read_bits(1)? != 0,
            },
            _ => return Err(NetError::Malformed("unknown entity kind")),
        }
    } else {
        base.ok_or(NetError::Malformed(
            "delta record without a baseline record",
        ))?
        .spawn
    };
    // With SPAWN set, deltas are absolute: the baseline record is ignored for this entity.
    let base = if m & mask::SPAWN != 0 { None } else { base };
    let mut e = match base {
        Some(b) => EntityState { id, ..*b },
        None => EntityState {
            id,
            spawn,
            pos: [0; 3],
            yaw: 0,
            pitch: 0,
            vel: None,
            anim: 0,
            acting: 0,
            health: None,
            flags: 0,
            status: 0,
        },
    };
    e.spawn = spawn;
    if m & mask::POS != 0 {
        e.pos = read_i32x3(r, base.map(|b| b.pos))?;
    }
    if m & mask::YAW != 0 {
        e.yaw = r.read_bits(12)? as u16;
        if e.yaw >= 3600 {
            return Err(NetError::Malformed("yaw out of range"));
        }
    }
    if m & mask::PITCH != 0 {
        e.pitch = r.read_bits(11)? as u16;
        if e.pitch > 1800 {
            return Err(NetError::Malformed("pitch out of range"));
        }
    }
    if m & mask::VEL != 0 {
        e.vel = Some(read_i32x3(r, base.and_then(|b| b.vel))?);
    }
    if m & mask::ANIM != 0 {
        e.anim = r.read_bits(8)? as u8;
        e.acting = if gm_core::sim::anim::acts(e.anim) {
            r.read_uvar()?.min(u16::MAX as u64) as u16
        } else {
            0
        };
    }
    if m & mask::HEALTH != 0 {
        let h = r.read_uvar()?;
        e.health = Some(u16::try_from(h).map_err(|_| NetError::Malformed("health too large"))?);
    }
    if m & mask::FLAGS != 0 {
        e.flags = r.read_bits(flags::BITS)? as u16;
    }
    if m & mask::STATUS != 0 {
        e.status = r.read_bits(STATUS_BITS)? as u32;
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn player_spawn() -> SpawnInfo {
        SpawnInfo::Player {
            frame: 1,
            team: 1,
            aspects: 0b00001,
            armour: 2,
        }
    }

    fn player(id: u32, x: i32, own: bool) -> EntityState {
        EntityState {
            id,
            spawn: player_spawn(),
            pos: [x, -400, 96],
            yaw: 1234,
            pitch: 900,
            vel: own.then_some([1000, 0, -40]),
            anim: 2,
            acting: 0,
            health: own.then_some(100),
            flags: flags::ALIVE | flags::ON_GROUND,
            status: 0,
        }
    }

    fn scene(tick: u32, shift: i32) -> Snapshot {
        let mut s = Snapshot::new(tick);
        s.own = OwnState {
            stamina: 97,
            focus: 120,
            statuses: vec![StatusWire {
                status: 1,
                remaining: 300,
                magnitude: 0.25,
                stacks: 1,
                source: 7,
            }],
            guns: Some(GunsWire {
                magazine: [1, 8],
                reserve: [23, 32],
                reloading: true,
            }),
            bar: [3, 0, 1, 0],
            using: 1,
        };
        s.entities.push(player(1, shift, true));
        for i in 2..17 {
            s.entities.push(player(i, i as i32 * 100 + shift, false));
        }
        s
    }

    #[test]
    fn full_snapshot_round_trips() {
        let s = scene(10, 0);
        let bytes = s.encode(None);
        let back = Snapshot::decode(&bytes, |_| None).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn delta_snapshot_is_small_and_exact() {
        let base = scene(10, 0);
        let mut next = scene(11, 20); // everyone moved 5 u in x
        next.baseline_tick = 10;
        next.last_input_tick = 77;
        next.entities[3].yaw = 1300;
        next.entities[5].flags = flags::ALIVE;
        next.entities[6].status = 1 << 3;
        let full = {
            let mut f = next.clone();
            f.baseline_tick = 0;
            f.encode(None).len()
        };
        let bytes = next.encode(Some(&base));
        // 16 entities: id (1) + mask (2) + x delta (1) + two zero deltas (2) ≈ 6 bytes each,
        // plus the own entity's velocity and a couple of changed fields, plus 14 bytes of
        // header and the own block (~8 bytes with one status).
        assert!(bytes.len() < 130, "delta is {} bytes", bytes.len());
        assert!(bytes.len() < full, "delta {} >= full {}", bytes.len(), full);
        let back = Snapshot::decode(&bytes, |t| (t == 10).then_some(&base)).unwrap();
        assert_eq!(back, next);
    }

    /// v9: a stance that is a script's carries the ability it belongs to; any other
    /// stance carries none and costs nothing more.
    #[test]
    fn the_acting_ability_travels_with_a_scripts_stance_and_with_no_other() {
        use gm_core::sim::anim;
        let base = scene(10, 0);
        let mut next = scene(11, 0);
        next.baseline_tick = 10;
        let still = next.encode(Some(&base)).len();
        // A swing of ability 7 begins: the stance and the ability, two bytes and a mask.
        next.entities[3].anim = anim::WINDUP;
        next.entities[3].acting = 7;
        let bytes = next.encode(Some(&base));
        assert!(bytes.len() <= still + 5, "{} over {still}", bytes.len());
        let swung = Snapshot::decode(&bytes, |t| (t == 10).then_some(&base)).unwrap();
        assert_eq!(swung, next);
        assert_eq!(
            (swung.entities[3].anim, swung.entities[3].acting),
            (anim::WINDUP, 7)
        );
        // The same stance of another ability is a change too.
        let mut other = next.clone();
        other.server_tick = 12;
        other.baseline_tick = 11;
        other.entities[3].acting = 300;
        let back =
            Snapshot::decode(&other.encode(Some(&next)), |t| (t == 11).then_some(&next)).unwrap();
        assert_eq!(back.entities[3].acting, 300);
        // Back on its feet: no ability is written and none is read.
        let mut after = other.clone();
        after.server_tick = 13;
        after.baseline_tick = 12;
        after.entities[3].anim = anim::RUN;
        after.entities[3].acting = 0;
        let back =
            Snapshot::decode(&after.encode(Some(&other)), |t| (t == 12).then_some(&other)).unwrap();
        assert_eq!(
            (back.entities[3].anim, back.entities[3].acting),
            (anim::RUN, 0)
        );
        // In a full snapshot as well.
        let mut full = next.clone();
        full.baseline_tick = 0;
        assert_eq!(
            Snapshot::decode(&full.encode(None), |_| None).unwrap(),
            full
        );
    }

    #[test]
    fn unchanged_entities_cost_nothing_and_are_carried_forward() {
        let base = scene(10, 0);
        let mut next = scene(11, 0);
        next.baseline_tick = 10;
        next.last_input_tick = 5;
        next.own.statuses.clear();
        let bytes = next.encode(Some(&base));
        // header 2 + 3 x 4 + own (stamina 1, focus 2, count 4 bits, the guns' bit and
        // their 4 bytes, the bar's four counts and the cell in use's 3 bits since v18)
        // + count 1 + removed 1, bit-packed: 28 bytes.
        assert_eq!(bytes.len(), 28);
        let back = Snapshot::decode(&bytes, |t| (t == 10).then_some(&base)).unwrap();
        assert_eq!(back, next);
        // One entity moves: only it is on the wire, the rest still come back.
        let mut moved = next.clone();
        moved.server_tick = 12;
        moved.baseline_tick = 10;
        moved.entities[7].pos[0] += 4;
        let bytes = moved.encode(Some(&base));
        assert!(bytes.len() <= 27 + 7, "{}", bytes.len());
        let back = Snapshot::decode(&bytes, |t| (t == 10).then_some(&base)).unwrap();
        assert_eq!(back, moved);
    }

    /// A body's health stops being sent when it leaves the viewer's party: the reader
    /// must not go on showing the last one it was told.
    #[test]
    fn a_health_that_is_no_longer_sent_is_no_longer_held() {
        let mut base = scene(10, 0);
        base.entities[4].health = Some(77);
        let mut next = base.clone();
        next.server_tick = 11;
        next.baseline_tick = 10;
        next.entities[4].health = None;
        let bytes = next.encode(Some(&base));
        let back = Snapshot::decode(&bytes, |t| (t == 10).then_some(&base)).unwrap();
        assert_eq!(back.entities[4].health, None);
        assert_eq!(back, next);
        // It costs one whole record, once: the next snapshot against this one is as
        // small as any in which nothing changed.
        let mut after = next.clone();
        after.server_tick = 12;
        after.baseline_tick = 11;
        let quiet = after.encode(Some(&next)).len();
        assert!(
            bytes.len() > quiet && bytes.len() <= quiet + 20,
            "{} {quiet}",
            bytes.len()
        );
        // And a health that is sent again is a delta as before.
        after.entities[4].health = Some(60);
        let back = Snapshot::decode(&after.encode(Some(&next)), |t| (t == 11).then_some(&next));
        assert_eq!(back.unwrap().entities[4].health, Some(60));
    }

    #[test]
    fn spawns_and_removals() {
        let base = scene(10, 0);
        let mut next = Snapshot::new(11);
        next.baseline_tick = 10;
        next.entities.push(player(1, 0, true));
        next.entities.push(EntityState {
            id: 40,
            spawn: SpawnInfo::Projectile {
                owner: 1,
                def: 2,
                input_tick: 4321,
            },
            pos: [10, 20, 30],
            yaw: 0,
            pitch: 0,
            vel: None,
            anim: 0,
            acting: 0,
            health: None,
            flags: flags::ALIVE,
            status: 0,
        });
        next.entities.push(EntityState {
            id: 41,
            spawn: SpawnInfo::Area {
                owner: 1,
                def: 3,
                radius: 128,
                harmful: true,
            },
            pos: [10, 20, 30],
            yaw: 0,
            pitch: 0,
            vel: None,
            anim: 0,
            acting: 0,
            health: None,
            flags: flags::ALIVE,
            status: 0,
        });
        next.normalize(Some(&base));
        assert_eq!(next.removed, (2..17).collect::<Vec<u32>>());
        let bytes = next.encode(Some(&base));
        let back = Snapshot::decode(&bytes, |t| (t == 10).then_some(&base)).unwrap();
        assert_eq!(back, next);
        assert_eq!(back.find(40).unwrap().spawn.kind(), EntityKind::Projectile);
        assert_eq!(back.find(41).unwrap().spawn.kind(), EntityKind::Area);
    }

    #[test]
    fn unknown_baseline_and_bad_records_are_errors() {
        let base = scene(10, 0);
        let mut next = scene(11, 4);
        next.baseline_tick = 10;
        let bytes = next.encode(Some(&base));
        assert_eq!(
            Snapshot::decode(&bytes, |_| None),
            Err(NetError::UnknownBaseline(10))
        );
        // A delta record for an entity the baseline never had.
        let mut smaller = base.clone();
        smaller.entities.retain(|e| e.id != 5);
        assert!(matches!(
            Snapshot::decode(&bytes, |_| Some(&smaller)),
            Err(NetError::Malformed(_))
        ));
        next.baseline_tick = 0;
        let mut cut = next.encode(None);
        cut.truncate(20);
        assert_eq!(Snapshot::decode(&cut, |_| None), Err(NetError::Overrun));
    }
}
