use glam::{Vec2, Vec3};

use super::*;
use crate::build::{Build, ContentPack};
use crate::collide::{Aabb, BoxWorld};
use crate::geom::Capsule;
use crate::matrix::{ArmourClass, Aspects, Attributes, Element};
use crate::sim::test_content::{self, phase2_build};
use crate::tick::TickRate;
use crate::trace::Hull;
use crate::vocab::{ArchetypeFrame, DamageType, EntityId, Status};

const REST_Z: f32 = 24.0;
const RATE: TickRate = TickRate::COMBAT;

fn zone_with(spawns: Vec<(Vec3, f32)>) -> Zone {
    let spawns = spawns
        .into_iter()
        .map(|(origin, yaw)| Spawn {
            origin,
            yaw,
            team: 0,
        })
        .collect();
    Zone::new(RATE, 7, spawns, test_content::pack(RATE))
}

/// Phase 2 characters placed exactly at the listed `(origin, yaw)` pairs, in order.
fn arena(placements: &[(Vec3, f32)]) -> (BoxWorld, Zone, Vec<EntityId>) {
    let world = BoxWorld::floor();
    let mut zone = zone_with(placements.to_vec());
    let build = phase2_build(&zone.content);
    let ids = placements
        .iter()
        .map(|&(o, y)| zone.add_player_at(build.clone(), 0, o, y))
        .collect();
    (world, zone, ids)
}

/// Builds placed at the listed `(build, origin, yaw)` triples.
fn arena_with(placements: Vec<(Build, Vec3, f32)>) -> (BoxWorld, Zone, Vec<EntityId>) {
    let world = BoxWorld::floor();
    let mut zone = zone_with(placements.iter().map(|p| (p.1, p.2)).collect());
    let ids = placements
        .into_iter()
        .map(|(build, o, y)| zone.add_player_at(build, 0, o, y))
        .collect();
    (world, zone, ids)
}

/// The Phase 2 character with a bolt as its primary (the ranged primaries of 2026-10-06)
/// and a kick beside it.
fn bolt_build(pack: &ContentPack, key: &str) -> Build {
    Build {
        primary: pack.find(key).expect(key),
        secondary: pack.find("kick").expect("kick"),
        ..phase2_build(pack)
    }
}

/// Preset builds placed at the listed `(name, origin, yaw)` triples.
fn arena_builds(placements: &[(&str, Vec3, f32)]) -> (BoxWorld, Zone, Vec<EntityId>) {
    let world = BoxWorld::floor();
    let mut zone = zone_with(placements.iter().map(|p| (p.1, p.2)).collect());
    let ids = placements
        .iter()
        .map(|&(name, o, y)| {
            // The ironclad as it was until MATRIX.md 16, for the tests of the hammer's
            // blow and the fortify it ignores: the same body with the hammer and fortify.
            let build = if name == "ironclad_hammer" {
                let mut b = zone.content.build("ironclad").expect(name).clone();
                b.primary = zone.content.find("hammer").unwrap();
                b.actives[1] = zone.content.find("fortify").unwrap();
                b
            } else {
                zone.content.build(name).expect(name).clone()
            };
            zone.add_player_at(build, 0, o, y)
        })
        .collect();
    (world, zone, ids)
}

fn input(yaw: f32, forward: f32, buttons: u16) -> Input {
    Input {
        buttons,
        yaw,
        pitch: 0.0,
        forward,
        side: 0.0,
        ability: 0,
        held: 0,
        target: 0,
        use_slot: 0,
    }
}

fn active(yaw: f32, forward: f32, slot: u8) -> Input {
    Input {
        ability: slot,
        ..input(yaw, forward, 0)
    }
}

/// Feed every player one new frame per tick and step. The frame tick continues after the
/// frames still queued (the server keeps one in reserve, so execution runs a tick behind).
fn tick(zone: &mut Zone, world: &BoxWorld, inputs: &[(EntityId, Input)], view: Tick) {
    for (id, inp) in inputs {
        let t = zone
            .player(*id)
            .map_or(0, |p| p.last_input_tick + p.queued_frames() as u32)
            + 1;
        zone.queue_input(*id, t, *inp, view);
    }
    zone.step(world);
}

fn run(zone: &mut Zone, world: &BoxWorld, inputs: &[(EntityId, Input)], ticks: usize) {
    for _ in 0..ticks {
        tick(zone, world, inputs, 0);
    }
}

fn hits(zone: &Zone, kind: HitKind) -> usize {
    zone.events
        .iter()
        .filter(|e| matches!(e, ZoneEvent::Hit { kind: k, .. } if *k == kind))
        .count()
}

#[test]
fn phase2_kit_validates_and_has_durations() {
    let sheet = test_content::phase2_sheet(RATE);
    let kit = &sheet.kit;
    // The sword's two further stages come with it (MODES.md 4.3).
    assert_eq!(kit.abilities.len(), 5);
    let (sword, crossbow, dash) = (
        kit.primary.unwrap() as usize,
        kit.secondary.unwrap() as usize,
        kit.actives[0].unwrap() as usize,
    );
    assert_eq!(kit.durations[sword], 10 + 4 + 20);
    assert_eq!(kit.durations[dash], 10);
    assert_eq!(kit.durations[crossbow], 8);
    assert_eq!(sheet.derived.health, 900);
    assert_eq!(sheet.derived.stamina, 120.0);
}

#[test]
fn sword_hits_in_front_and_not_behind() {
    for (attacker_yaw, expect_hit) in [(0.0f32, true), (180.0f32, false)] {
        let (world, mut zone, ids) = arena(&[
            (Vec3::new(0.0, 0.0, REST_Z), 0.0),
            (Vec3::new(48.0, 0.0, REST_Z), 180.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        run(
            &mut zone,
            &world,
            &[
                (a, input(attacker_yaw, 0.0, buttons::PRIMARY)),
                (b, input(180.0, 0.0, 0)),
            ],
            13,
        );
        let hp = zone.player(b).unwrap().health;
        if expect_hit {
            // 60 slash x 1.0 (STR 10) x 1.25 (cloth) x 0.80 (armour) = 60.
            assert_eq!(hp, 900 - 60, "yaw {attacker_yaw}");
            assert!(zone.events.iter().any(|e| matches!(
                e,
                ZoneEvent::Hit { attacker, target, amount: 60, kind: HitKind::Melee, .. } if *attacker == a && *target == b
            )));
        } else {
            assert_eq!(hp, 900, "yaw {attacker_yaw}");
        }
        // Holding the button does not re-trigger; one swing per press.
        assert_eq!(hits(&zone, HitKind::Melee), expect_hit as usize);
    }
}

#[test]
fn crossbow_bolt_hits_the_first_body_in_line_even_an_ally() {
    let pack = test_content::pack(RATE);
    let (world, mut zone, ids) = arena_with(vec![
        (
            bolt_build(&pack, "crossbow"),
            Vec3::new(0.0, 0.0, REST_Z),
            0.0,
        ),
        (phase2_build(&pack), Vec3::new(120.0, 0.0, REST_Z), 0.0),
        (phase2_build(&pack), Vec3::new(240.0, 0.0, REST_Z), 0.0),
    ]);
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    let idle = input(0.0, 0.0, 0);
    tick(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle), (c, idle)],
        0,
    );
    run(&mut zone, &world, &[(a, idle), (b, idle), (c, idle)], 40);
    // 80 pierce x 1.0 x 1.0 (cloth) x 0.80 = 64.
    assert_eq!(
        zone.player(b).unwrap().health,
        900 - 64,
        "the body in between takes the bolt"
    );
    assert_eq!(zone.player(c).unwrap().health, 900);
    assert!(
        zone.events
            .iter()
            .any(|e| matches!(e, ZoneEvent::ProjectileSpawned { owner, .. } if *owner == a))
    );
    assert!(
        zone.events
            .iter()
            .any(|e| matches!(e, ZoneEvent::ProjectileRemoved(_)))
    );
    assert!(zone.projectiles().is_empty());
}

#[test]
fn dash_moves_fast_costs_stamina_and_pauses_regen() {
    let (world, mut zone, ids) = arena(&[(Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    tick(
        &mut zone,
        &world,
        &[(a, input(0.0, 1.0, buttons::ABILITY1))],
        0,
    );
    // The press runs one tick late (reserve); ten dash frames follow.
    run(&mut zone, &world, &[(a, input(0.0, 1.0, 0))], 10);
    let p = zone.player(a).unwrap();
    // 900 u/s for 10 ticks = 140.6 u, minus the epsilon pull-backs.
    assert!(
        p.mover.mv.origin.x > 130.0 && p.mover.mv.origin.x < 142.0,
        "{:?}",
        p.mover.mv.origin
    );
    assert_eq!(
        p.mover.stamina, 90.0,
        "regen pauses for a second after a spend"
    );
    assert_eq!(p.anim, anim::DASH);
    assert!(p.mover.evading(p.last_input_tick));
    run(&mut zone, &world, &[(a, input(0.0, 1.0, 0))], 2);
    assert!(zone.player(a).unwrap().mover.dash.is_none());
    run(&mut zone, &world, &[(a, input(0.0, 1.0, 0))], 64);
    let p = zone.player(a).unwrap();
    assert!(p.mover.stamina > 90.5, "regen resumed: {}", p.mover.stamina);
    assert!(!p.mover.evading(p.last_input_tick));
}

#[test]
fn lag_compensation_rewinds_the_target() {
    for (rewind, expect_hit) in [(true, true), (false, false)] {
        let (world, mut zone, ids) = arena(&[
            (Vec3::new(0.0, 0.0, REST_Z), 0.0),
            (Vec3::new(40.0, 0.0, REST_Z), 0.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        run(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, 0)), (b, input(0.0, 1.0, 0))],
            10,
        );
        let then = zone.tick;
        let old_x = zone.player(b).unwrap().mover.mv.origin.x;
        for _ in 0..MAX_REWIND_TICKS {
            tick(
                &mut zone,
                &world,
                &[(a, input(0.0, 0.0, 0)), (b, input(0.0, 1.0, 0))],
                0,
            );
        }
        let now_x = zone.player(b).unwrap().mover.mv.origin.x;
        assert!(old_x - 14.0 <= 72.0, "old position in reach: {old_x}");
        assert!(
            now_x - 14.0 > 72.0 + 20.0,
            "new position out of reach: {now_x}"
        );
        let view = if rewind { then } else { 0 };
        for _ in 0..12 {
            tick(
                &mut zone,
                &world,
                &[
                    (a, input(0.0, 0.0, buttons::PRIMARY)),
                    (b, input(0.0, 1.0, 0)),
                ],
                view,
            );
        }
        let hit = zone.player(b).unwrap().health < 900;
        assert_eq!(hit, expect_hit, "rewind {rewind}: old {old_x} now {now_x}");
    }
}

/// LOOK.md 13: while a body's stance is its script's, the zone says which ability it is
/// (so a client can draw the swing where it lands), and the stance is the one
/// `script_anim` gives for the ticks gone, which is how a client says its own.
#[test]
fn a_stance_names_the_ability_it_belongs_to_and_only_while_it_is_the_scripts() {
    let (world, mut zone, ids) = arena(&[(Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    assert_eq!(zone.player(a).unwrap().acting, 0);
    let mut stances = Vec::new();
    for t in 0..40 {
        let press = if t < 2 { buttons::PRIMARY } else { 0 };
        tick(&mut zone, &world, &[(a, input(0.0, 0.0, press))], 0);
        let p = zone.player(a).unwrap();
        match p.mover.script {
            Some(s) if anim::acts(p.anim) => {
                let ability = &p.sheet.kit.abilities[s.ability as usize];
                assert_eq!(p.acting, ability.id.0, "tick {t}");
                assert_ne!(p.acting, 0);
                let elapsed = crate::sim::tick_delta(p.last_input_tick, s.started).max(0) as u32;
                assert_eq!(
                    p.anim,
                    crate::sim::script_anim(ability, elapsed),
                    "tick {t}"
                );
            }
            _ => assert_eq!(p.acting, 0, "tick {t}: stance {}", p.anim),
        }
        if stances.last() != Some(&p.anim) {
            stances.push(p.anim);
        }
    }
    // (It lands on the floor in its first tick.)
    assert!(
        stances.ends_with(&[anim::WINDUP, anim::SWING, anim::RECOVER, anim::IDLE]),
        "a swing is wound up, lands, and is recovered from: {stances:?}"
    );
}

#[test]
fn frames_run_exactly_once_and_are_rate_limited() {
    let (world, mut zone, ids) = arena(&[(Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    for t in 1..=20 {
        zone.queue_input(a, t, input(0.0, 1.0, 0), 0);
    }
    zone.queue_input(a, 20, input(0.0, 1.0, 0), 0);
    zone.queue_input(a, 3, input(0.0, 1.0, 0), 0);
    assert_eq!(zone.player(a).unwrap().queued_frames(), 20);
    let mut per_tick = Vec::new();
    for _ in 0..8 {
        let before = zone.player(a).unwrap().executed_frames;
        zone.step(&world);
        per_tick.push(zone.player(a).unwrap().executed_frames - before);
    }
    assert_eq!(per_tick, [2, 2, 2, 2, 2, 2, 2, 1]);
    assert_eq!(zone.player(a).unwrap().last_input_tick, 15);
    for _ in 0..8 {
        zone.step(&world);
    }
    let p = zone.player(a).unwrap();
    assert_eq!(p.executed_frames, 19);
    assert_eq!(p.queued_frames(), 1);
    assert_eq!(p.starved_ticks, 4, "reserve held for the last ticks");
    zone.queue_input(a, 21, input(0.0, 1.0, 0), 0);
    zone.step(&world);
    let p = zone.player(a).unwrap();
    assert_eq!(p.executed_frames, 20);
    assert_eq!(p.last_input_tick, 20);
    zone.step(&world);
    assert_eq!(
        zone.player(a).unwrap().executed_frames,
        20,
        "nothing invented"
    );
}

#[test]
fn reordered_datagrams_keep_every_frame() {
    let (world, mut zone, ids) = arena(&[(Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    for t in 4..=7 {
        zone.queue_input(a, t, input(0.0, 1.0, 0), 0);
    }
    for t in 1..=4 {
        zone.queue_input(a, t, input(0.0, 1.0, 0), 0);
    }
    assert_eq!(zone.player(a).unwrap().queued_frames(), 7);
    for _ in 0..8 {
        zone.step(&world);
    }
    let p = zone.player(a).unwrap();
    assert_eq!(p.executed_frames, 6, "one in reserve");
    assert_eq!(p.last_input_tick, 6);
    zone.queue_input(a, 3, input(0.0, 1.0, 0), 0);
    assert_eq!(zone.player(a).unwrap().queued_frames(), 1);
}

#[test]
fn players_block_each_other() {
    let (world, mut zone, ids) = arena(&[
        (Vec3::new(-100.0, 0.0, REST_Z), 0.0),
        (Vec3::new(100.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 1.0, 0)), (b, input(180.0, 1.0, 0))],
        128,
    );
    let ax = zone.player(a).unwrap().mover.mv.origin.x;
    let bx = zone.player(b).unwrap().mover.mv.origin.x;
    assert!(ax < bx, "players passed through each other: {ax} {bx}");
    assert!(bx - ax >= 32.0 - 0.1, "hulls overlap: {ax} {bx}");
    assert!(bx - ax < 34.0, "players did not meet: {ax} {bx}");
}

#[test]
fn death_and_respawn() {
    let (world, mut zone, ids) = arena(&[
        (Vec3::new(0.0, 0.0, REST_Z), 0.0),
        (Vec3::new(48.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    let mut swings = 0;
    let mut killed_at = None;
    // 900 health at 60 a blow is fifteen blows; the sword's cooldown is 39 ticks.
    for t in 0..1000 {
        let press = t % 48 == 0;
        if press {
            swings += 1;
        }
        let btn = if press { buttons::PRIMARY } else { 0 };
        tick(
            &mut zone,
            &world,
            &[(a, input(0.0, 1.0, btn)), (b, input(180.0, 0.0, 0))],
            0,
        );
        if killed_at.is_none()
            && zone.events.iter().any(|e| matches!(e, ZoneEvent::Killed { victim, killer } if *victim == b && *killer == a))
        {
            killed_at = Some(zone.tick);
            assert!(!zone.player(b).unwrap().alive);
            assert_eq!(zone.player(b).unwrap().anim, anim::DEAD);
            assert_eq!(zone.player(a).unwrap().kills, 1);
            break;
        }
    }
    let killed_at = killed_at.expect("fifteen swings kill");
    assert!(swings >= 15);
    for _ in 0..zone.rate.ms_to_ticks(RESPAWN_MS) {
        tick(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
            0,
        );
    }
    let p = zone.player(b).unwrap();
    assert!(
        p.alive,
        "respawned {} ticks after {killed_at}",
        zone.tick - killed_at
    );
    assert_eq!(p.health, 900);
    assert!(
        zone.events
            .iter()
            .any(|e| matches!(e, ZoneEvent::Respawned(id) if *id == b))
    );
}

#[test]
fn history_rewinds_to_the_nearest_recorded_tick() {
    let mut h = History::default();
    for t in 1..=40u32 {
        h.record(t, vec![(1, Vec3::new(t as f32, 0.0, 0.0), t > 30)]);
    }
    assert_eq!(h.origin_at(10, 1), Some(Vec3::new(10.0, 0.0, 0.0)));
    assert_eq!(h.body_at(35, 1), Some((Vec3::new(35.0, 0.0, 0.0), true)));
    assert_eq!(h.origin_at(2, 1), Some(Vec3::new(9.0, 0.0, 0.0)));
    assert_eq!(h.origin_at(41, 1), None);
    assert_eq!(h.origin_at(10, 2), None);
}

#[test]
fn add_player_refuses_invalid_builds_and_spawns_by_team() {
    let world = BoxWorld::floor();
    let spawns = vec![
        Spawn {
            origin: Vec3::new(-500.0, 0.0, REST_Z),
            yaw: 0.0,
            team: 1,
        },
        Spawn {
            origin: Vec3::new(500.0, 0.0, REST_Z),
            yaw: 180.0,
            team: 2,
        },
    ];
    let mut zone = Zone::new(RATE, 1, spawns, test_content::pack(RATE));
    let blade = zone.content.build("blade").unwrap().clone();
    assert!(
        zone.add_player(&world, phase2_build(&zone.content), 1)
            .is_err()
    );
    let a = zone.add_player(&world, blade.clone(), 1).unwrap();
    let b = zone.add_player(&world, blade.clone(), 2).unwrap();
    let c = zone.add_player(&world, blade, 2).unwrap();
    assert!(zone.player(a).unwrap().mover.mv.origin.x < 0.0);
    assert!(zone.player(b).unwrap().mover.mv.origin.x > 0.0);
    assert!(zone.player(c).unwrap().mover.mv.origin.x > 0.0);
    assert_ne!(
        zone.player(b).unwrap().mover.mv.origin,
        zone.player(c).unwrap().mover.mv.origin,
        "second spawn is offset"
    );
    assert_eq!(zone.smallest_team(), 1);
    assert_eq!(zone.player(a).unwrap().team(), 1);
}

#[test]
fn shield_wall_blocks_from_the_front_costs_stamina_and_breaks() {
    // An ironclad facing a blade: sword from the front is blocked, from behind it is not.
    for (facing, expect_block) in [(180.0f32, true), (0.0f32, false)] {
        let (world, mut zone, ids) = arena_builds(&[
            ("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0),
            ("ironclad", Vec3::new(50.0, 0.0, REST_Z), facing),
        ]);
        let (a, b) = (ids[0], ids[1]);
        let hold = input(facing, 0.0, buttons::GUARD);
        run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, hold)], 3);
        assert_eq!(zone.player(b).unwrap().anim, anim::GUARD);
        let stamina_before = zone.player(b).unwrap().mover.stamina;
        run(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, hold)],
            13,
        );
        let p = zone.player(b).unwrap();
        let taken = p.max_health() - p.health;
        // Sword 60 x 1.28 (STR 17) x 0.5 (plate) x 0.6 (armour 0.40) = 23.04 -> 23 unblocked,
        // x 0.2 blocked = 4.608 -> 5.
        if expect_block {
            assert_eq!(taken, 5, "facing {facing}");
            assert_eq!(stamina_before - p.mover.stamina, 12.0);
        } else {
            assert_eq!(taken, 23, "facing {facing}");
        }
    }
    // Guard break: with less stamina than the block costs, the hit lands in full and staggers.
    let (world, mut zone, ids) = arena_builds(&[
        ("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("ironclad", Vec3::new(50.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    zone.player_mut(b).unwrap().mover.stamina = 5.0;
    let hold = input(180.0, 0.0, buttons::GUARD);
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, hold)], 3);
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, hold)],
        13,
    );
    let p = zone.player(b).unwrap();
    assert_eq!(p.max_health() - p.health, 23);
    assert_eq!(p.mover.stamina, 0.0);
    assert!(p.mover.statuses.has(Status::Stagger));
    assert!(
        zone.events
            .iter()
            .any(|e| matches!(e, ZoneEvent::GuardBroken(id) if *id == b))
    );
}

#[test]
fn parry_negates_staggers_and_ripostes() {
    // Two blades: b parries a's sword inside the window.
    let (world, mut zone, ids) = arena_builds(&[
        ("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("blade", Vec3::new(50.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    // a presses attack; b presses parry three ticks later so the 150 ms window covers the
    // 90 ms windup.
    tick(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, buttons::PRIMARY)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        2,
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, 0)),
            (b, input(180.0, 0.0, buttons::GUARD)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        12,
    );
    assert_eq!(
        zone.player(b).unwrap().health,
        zone.player(b).unwrap().max_health()
    );
    assert!(
        zone.events
            .iter()
            .any(|e| matches!(e, ZoneEvent::Parried { defender, attacker } if *defender == b && *attacker == a))
    );
    let pa = zone.player(a).unwrap();
    assert!(pa.health < pa.max_health(), "riposte landed");
    assert!(
        zone.events
            .iter()
            .any(|e| matches!(e, ZoneEvent::StatusApplied { target, status: Status::Stagger, .. } if *target == a))
    );
    // A parry pressed with nobody attacking whiffs into recovery.
    let (world, mut zone, ids) = arena_builds(&[("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    tick(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, buttons::GUARD))],
        0,
    );
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0))], 2);
    assert!(matches!(
        zone.player(a).unwrap().mover.guard,
        GuardState::Parry { .. }
    ));
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0))], 12);
    assert!(matches!(
        zone.player(a).unwrap().mover.guard,
        GuardState::Whiff { .. }
    ));
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0))], 30);
    assert_eq!(zone.player(a).unwrap().mover.guard, GuardState::None);
}

#[test]
fn stomp_pulses_once_damages_and_slows_everyone_in_range() {
    let (world, mut zone, ids) = arena_builds(&[
        ("ironclad", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("blade", Vec3::new(100.0, 0.0, REST_Z), 180.0),
        ("blade", Vec3::new(300.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    let idle = input(180.0, 0.0, 0);
    // Stomp is active slot 1 of the ironclad.
    tick(
        &mut zone,
        &world,
        &[(a, active(0.0, 0.0, 1)), (b, idle), (c, idle)],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, idle), (c, idle)],
        20,
    );
    let pb = zone.player(b).unwrap();
    // 35 ground x 0.8 (INT 5), Ground into Water is 1x, ward 0.02 * 5 = 0.10:
    // 35 * 0.8 * 1 * 0.9 = 25.2 -> 25.
    assert_eq!(pb.max_health() - pb.health, 25);
    assert!(pb.mover.statuses.has(Status::Slow));
    assert!(
        (pb.mover.statuses.speed_scale() - 0.7).abs() < 1e-6,
        "{}",
        pb.mover.statuses.speed_scale()
    );
    assert_eq!(
        zone.player(c).unwrap().health,
        zone.player(c).unwrap().max_health()
    );
    assert_eq!(hits(&zone, HitKind::Area), 1);
    // An instant pulse stays on the wire for its echo (INSTANT_AREA_ECHO_MS), spent: it
    // is seen, and it never pulses again.
    assert!(
        !zone.areas().is_empty(),
        "an instant pulse lingers for its echo after it struck"
    );
    let echo = zone.rate.ms_to_ticks(INSTANT_AREA_ECHO_MS) as usize + 1;
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, idle), (c, idle)],
        echo,
    );
    assert!(zone.areas().is_empty(), "the echo ended");
    assert_eq!(hits(&zone, HitKind::Area), 1, "spent: no second pulse");
    assert!(
        zone.events
            .iter()
            .any(|e| matches!(e, ZoneEvent::AreaRemoved(_)))
    );
}

#[test]
fn frost_nova_chills_twice_and_a_shard_freezes() {
    let (world, mut zone, ids) = arena_builds(&[
        ("frostweaver", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("ironclad", Vec3::new(100.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    let idle = input(180.0, 0.0, 0);
    tick(&mut zone, &world, &[(a, active(0.0, 0.0, 1)), (b, idle)], 0);
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 25);
    let pb = zone.player(b).unwrap();
    assert_eq!(pb.mover.statuses.stacks(Status::Chill), 2);
    assert!((pb.mover.statuses.speed_scale() - 0.7).abs() < 1e-6);
    // Water into Ground is 2x and ignores plate: 30 * 1.4 (INT 20) * 2 * (1 - 0.2 ward) = 67.2 -> 67.
    assert_eq!(pb.max_health() - pb.health, 67);
    // The ice shard (the frostweaver's primary) adds the third stack: frozen (rooted),
    // chill cleared, immune after.
    tick(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle)],
        0,
    );
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 20);
    let pb = zone.player(b).unwrap();
    assert!(pb.mover.statuses.has(Status::Root), "frozen");
    assert_eq!(pb.mover.statuses.stacks(Status::Chill), 0);
    assert_eq!(pb.mover.statuses.speed_scale(), 0.0);
    // Rooted: walking does nothing.
    let x = pb.mover.mv.origin.x;
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 1.0, 0))],
        10,
    );
    assert!((zone.player(b).unwrap().mover.mv.origin.x - x).abs() < 1.0);
}

#[test]
fn burn_ticks_four_times_a_second_and_kills_are_attributed() {
    // A firebolt (a primary) into a shade.
    let pack = test_content::pack(RATE);
    let shade = pack.build("shade").unwrap().clone();
    let (world, mut zone, ids) = arena_with(vec![
        (
            bolt_build(&pack, "firebolt"),
            Vec3::new(0.0, 0.0, REST_Z),
            0.0,
        ),
        (shade, Vec3::new(200.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    let idle = input(180.0, 0.0, 0);
    tick(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle)],
        0,
    );
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 30);
    let pb = zone.player(b).unwrap();
    assert!(pb.mover.statuses.has(Status::Burn));
    let after_hit = pb.health;
    assert_eq!(hits(&zone, HitKind::Projectile), 1);
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 64);
    let dots = hits(&zone, HitKind::Dot);
    assert!((4..=5).contains(&dots), "{dots} burn pulses in a second");
    assert!(zone.player(b).unwrap().health < after_hit);
    // Burn to death: attribution goes to the source.
    zone.player_mut(b).unwrap().health = 2;
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 20);
    assert!(zone.events.iter().any(
        |e| matches!(e, ZoneEvent::Killed { victim, killer } if *victim == b && *killer == a)
    ));
    assert_eq!(zone.player(a).unwrap().kills, 1);
}

#[test]
fn blink_teleports_along_the_facing_and_stops_at_walls() {
    let mut world = BoxWorld::floor();
    world.push(
        Vec3::new(150.0, -256.0, 0.0),
        Vec3::new(200.0, 256.0, 200.0),
    );
    let mut zone = zone_with(vec![(Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let shade = zone.content.build("shade").unwrap().clone();
    let a = zone.add_player_at(shade, 0, Vec3::new(0.0, 0.0, REST_Z), 0.0);
    // Blink is active slot 1 of the shade: 256 u, blocked by the wall at x = 150.
    tick(&mut zone, &world, &[(a, active(0.0, 0.0, 1))], 0);
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0))], 2);
    let x = zone.player(a).unwrap().mover.mv.origin.x;
    assert!(x > 130.0 && x <= 134.0 + 1e-3, "x = {x}");
    let focus = zone.player(a).unwrap().mover.focus;
    assert!(focus < zone.player(a).unwrap().sheet.derived.focus - 29.0);
    // Facing away from the wall, the full distance.
    let mut zone = zone_with(vec![(Vec3::new(0.0, 0.0, REST_Z), 180.0)]);
    let shade = zone.content.build("shade").unwrap().clone();
    let a = zone.add_player_at(shade, 0, Vec3::new(0.0, 0.0, REST_Z), 180.0);
    tick(&mut zone, &world, &[(a, active(180.0, 0.0, 1))], 0);
    run(&mut zone, &world, &[(a, input(180.0, 0.0, 0))], 2);
    let x = zone.player(a).unwrap().mover.mv.origin.x;
    assert!((x + 256.0).abs() < 1.0, "x = {x}");
}

#[test]
fn hammer_staggers_at_the_threshold_then_immunity_holds() {
    // Ironclad hammer (35 stagger, a 64-tick cooldown) into a blade (threshold
    // 40 + 3 * 15 = 85): a blow every 66 ticks, the meter losing 20.6 between blows
    // (STAGGER_DECAY_PER_S 20), is 35, 49.4, 63.8, 78.1, 92.5: five hits.
    let (world, mut zone, ids) = arena_builds(&[
        ("ironclad_hammer", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("blade", Vec3::new(55.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    let idle = input(180.0, 0.0, 0);
    let mut staggered_at = None;
    for t in 0..400 {
        let btn = if t % 66 == 0 { buttons::PRIMARY } else { 0 };
        // The attacker keeps walking into the target so knockback cannot carry it away.
        tick(
            &mut zone,
            &world,
            &[(a, input(0.0, 1.0, btn)), (b, idle)],
            0,
        );
        if staggered_at.is_none() && zone.player(b).unwrap().mover.statuses.staggered() {
            staggered_at = Some(t);
        }
    }
    let t = staggered_at.expect("staggered");
    assert!(t >= 4 * 66, "needs five hits, staggered at tick {t}");
    let staggers = zone
        .events
        .iter()
        .filter(|e| matches!(e, ZoneEvent::Staggered(id) if *id == b))
        .count();
    assert_eq!(staggers, 1, "immunity after the first stagger");
    assert!(
        hits(&zone, HitKind::Melee) >= 6,
        "{}",
        hits(&zone, HitKind::Melee)
    );
}

#[test]
fn fortify_cuts_frost_but_not_the_hammer() {
    let (world, mut zone, ids) = arena_builds(&[
        ("ironclad_hammer", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("ironclad_hammer", Vec3::new(55.0, 0.0, REST_Z), 180.0),
        ("frostweaver", Vec3::new(0.0, 200.0, REST_Z), 270.0),
    ]);
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    let idle = input(180.0, 0.0, 0);
    // b fortifies (active slot 2), then a hammers b and c throws an ice shard at b.
    tick(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, 0)),
            (b, active(180.0, 0.0, 2)),
            (c, idle),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, idle), (c, idle)],
        3,
    );
    assert!(zone.player(b).unwrap().mover.statuses.has(Status::Fortify));
    tick(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle), (c, idle)],
        0,
    );
    // (The hammer's 300 ms windup, then its knockback carrying b: long enough for b to
    // settle, fortify holding for 5 s.)
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, idle), (c, idle)],
        60,
    );
    let pb = zone.player(b).unwrap();
    // Hammer 90 x 1.2 (STR 15) x 1.25 (blunt vs plate) x 0.6 (armour 0.40) = 81, fortify ignored.
    assert_eq!(pb.max_health() - pb.health, 81);
    let before = pb.health;
    // c aims at b where the hammer's knockback left it.
    let to = pb.mover.mv.origin - zone.player(c).unwrap().mover.mv.origin;
    let yaw = to.y.atan2(to.x).to_degrees().rem_euclid(360.0);
    tick(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, 0)),
            (b, idle),
            (c, input(yaw, 0.0, buttons::PRIMARY)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, idle), (c, input(yaw, 0.0, 0))],
        40,
    );
    let pb = zone.player(b).unwrap();
    // Ice shard 50 x 1.4 (INT 20) x 2 (water into ground) x 0.8 (ward 0.20) x 0.6 (fortify)
    // = 67.2 -> 67.
    assert_eq!(
        before - pb.health,
        67,
        "{}",
        hits(&zone, HitKind::Projectile)
    );
}

#[test]
fn respec_applies_at_the_next_respawn() {
    let (world, mut zone, ids) = arena_builds(&[
        ("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("blade", Vec3::new(50.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    let frost = zone.content.build("frostweaver").unwrap().clone();
    // A hundred attribute points where thirty are free.
    let bad = Build {
        attributes: Attributes::flat(25),
        ..frost.clone()
    };
    assert!(zone.request_respec(b, bad).is_err());
    zone.request_respec(b, frost).unwrap();
    assert_eq!(
        zone.player(b).unwrap().sheet.build.frame,
        ArchetypeFrame::Striker
    );
    zone.player_mut(b).unwrap().health = 1;
    run(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, buttons::PRIMARY)),
            (b, input(180.0, 0.0, 0)),
        ],
        13,
    );
    assert!(!zone.player(b).unwrap().alive);
    for _ in 0..zone.rate.ms_to_ticks(RESPAWN_MS) {
        tick(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
            0,
        );
    }
    let pb = zone.player(b).unwrap();
    assert!(pb.alive);
    assert_eq!(pb.sheet.build.frame, ArchetypeFrame::Caster);
    assert_eq!(pb.sheet.build.armour, ArmourClass::Cloth);
    assert!(pb.sheet.build.aspects.contains(Element::Water));
    assert_eq!(pb.health, pb.sheet.derived.health);
    assert_eq!(pb.health, 500 + 40 * 5);
    assert_eq!(
        pb.sheet.build.aspects,
        Aspects::two(Element::Water, Element::Air)
    );
}

// ---------- Phase 7: minds, the command stance, aimed areas, zero packets, held respawns ----------

/// A mind-driven body with a preset build at an exact place.
fn add_mind(zone: &mut Zone, name: &str, origin: Vec3, yaw: f32) -> EntityId {
    let build = zone.content.build(name).expect(name).clone();
    let sheet = crate::build::Sheet::new(build, &zone.content, 0);
    zone.add_body(sheet, origin, yaw, Driver::Mind)
}

#[test]
fn a_mind_runs_exactly_one_frame_a_tick_and_never_starves() {
    let (world, mut zone, ids) = arena_builds(&[("blade", Vec3::new(48.0, 0.0, REST_Z), 180.0)]);
    let target = ids[0];
    let mind = add_mind(&mut zone, "blade", Vec3::new(0.0, 0.0, REST_Z), 0.0);
    assert_eq!(zone.player(mind).unwrap().party, mind, "its own party");
    // No frame handed over: the body is not simulated, and that is not starvation.
    zone.step(&world);
    let p = zone.player(mind).unwrap();
    assert_eq!((p.executed_frames, p.starved_ticks), (0, 0));
    // One frame per tick from the first tick, no reserve: the swing lands on time.
    for t in 0..20 {
        let buttons = if t == 0 { buttons::PRIMARY } else { 0 };
        zone.drive(mind, input(0.0, 0.0, buttons));
        tick(&mut zone, &world, &[(target, input(180.0, 0.0, 0))], 0);
    }
    let p = zone.player(mind).unwrap();
    assert_eq!(p.executed_frames, 20);
    assert_eq!(p.last_input_tick, 20);
    assert_eq!(p.starved_ticks, 0);
    assert_eq!(hits(&zone, HitKind::Melee), 1);
    // A client cannot drive a mind's body, and `drive` does nothing for a client's.
    zone.queue_input(mind, 500, input(0.0, 1.0, 0), 0);
    zone.drive(target, input(0.0, 1.0, 0));
    let x = zone.player(mind).unwrap().mover.mv.origin.x;
    zone.step(&world);
    assert_eq!(zone.player(mind).unwrap().mover.mv.origin.x, x);
}

#[test]
fn the_command_stance_roots_the_body_and_standing_up_takes_400_ms() {
    let (world, mut zone, ids) = arena_builds(&[("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    let exit = command_exit_ticks(RATE.dt());
    assert_eq!(exit, 26, "400 ms at 64 Hz");
    // Held with the stick forward and the sword pressed: nothing happens but the stance.
    let held = input(
        0.0,
        1.0,
        buttons::COMMAND | buttons::PRIMARY | buttons::GUARD,
    );
    run(&mut zone, &world, &[(a, held)], 30);
    let p = zone.player(a).unwrap();
    assert!(p.mover.commanding(p.last_input_tick));
    assert!(p.mover.mv.origin.x.abs() < 0.5, "{:?}", p.mover.mv.origin);
    assert!(p.mover.script.is_none());
    assert_eq!(p.mover.guard, GuardState::None);
    assert_eq!(p.anim, anim::COMMAND);
    assert_eq!(hits(&zone, HitKind::Melee), 0);
    // Released: still rooted while standing up...
    let walk = input(0.0, 1.0, 0);
    run(&mut zone, &world, &[(a, walk)], 20);
    let p = zone.player(a).unwrap();
    assert!(p.mover.mv.origin.x.abs() < 0.5, "{:?}", p.mover.mv.origin);
    assert_eq!(p.anim, anim::COMMAND);
    // ...and free a little later.
    run(&mut zone, &world, &[(a, walk)], 40);
    let p = zone.player(a).unwrap();
    assert!(!p.mover.commanding(p.last_input_tick));
    assert!(p.mover.mv.origin.x > 60.0, "{:?}", p.mover.mv.origin);
}

#[test]
fn the_stance_waits_for_a_running_script() {
    let (world, mut zone, ids) = arena_builds(&[("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    // Swing first (the sword's script is 19 ticks), then hold the button: the swing is not
    // cancelled and the body keeps moving until the script is over.
    tick(
        &mut zone,
        &world,
        &[(a, input(0.0, 1.0, buttons::PRIMARY))],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 1.0, buttons::COMMAND))],
        12,
    );
    let p = zone.player(a).unwrap();
    assert!(p.mover.script.is_some());
    assert!(!p.mover.commanding(p.last_input_tick));
    let moving = p.mover.mv.origin.x;
    assert!(moving > 5.0, "{moving}");
    // The stance begins when the script ends; friction then brings the body to rest.
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 1.0, buttons::COMMAND))],
        70,
    );
    let p = zone.player(a).unwrap();
    assert!(p.mover.commanding(p.last_input_tick));
    let stopped = p.mover.mv.origin.x;
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 1.0, buttons::COMMAND))],
        30,
    );
    assert!((zone.player(a).unwrap().mover.mv.origin.x - stopped).abs() < 0.5);
}

#[test]
fn a_mend_dart_heals_whoever_it_hits_and_is_not_an_attack() {
    let (world, mut zone, ids) = arena_builds(&[
        ("mender", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("ironclad", Vec3::new(200.0, 0.0, REST_Z), 180.0),
    ]);
    let (healer, tank) = (ids[0], ids[1]);
    zone.player_mut(tank).unwrap().health -= 60;
    let hurt = zone.player(tank).unwrap().health;
    let full_stamina = zone.player(tank).unwrap().mover.stamina;
    // The tank holds its shield wall towards the healer: a zero packet is not blocked.
    let guard = input(180.0, 0.0, buttons::GUARD);
    tick(
        &mut zone,
        &world,
        &[(healer, input(0.0, 0.0, buttons::SECONDARY)), (tank, guard)],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(healer, input(0.0, 0.0, 0)), (tank, guard)],
        40,
    );
    let p = zone.player(tank).unwrap();
    assert!(p.mover.statuses.has(Status::Regen), "the dart landed");
    assert_eq!(hits(&zone, HitKind::Projectile), 0, "no damage event");
    assert_eq!(p.mover.stamina, full_stamina, "nothing was blocked");
    assert_eq!(p.mover.guard, GuardState::Block, "and nothing interrupted");
    // 20/s in pulses of 5 for 3 s, neither shortened nor lengthened by the ironclad's SPR 10
    // (x 1.0): twelve or thirteen pulses depending on where the dart landed in the pulse
    // cycle, all credited to the healer.
    run(
        &mut zone,
        &world,
        &[(healer, input(0.0, 0.0, 0)), (tank, guard)],
        200,
    );
    let healed = zone.player(tank).unwrap().health - hurt;
    assert!(healed == 60 || healed == 65, "healed {healed}");
    let credited: i32 = zone
        .events
        .iter()
        .filter_map(|e| match e {
            ZoneEvent::Healed {
                target,
                source,
                amount,
            } if *target == tank && *source == healer => Some(*amount),
            _ => None,
        })
        .sum();
    assert_eq!(credited, healed);
}

#[test]
fn an_aimed_area_lands_under_the_first_body_or_surface_on_the_view_ray() {
    // At a body: its feet.
    let (world, mut zone, ids) = arena_builds(&[
        ("mender", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("blade", Vec3::new(300.0, 0.0, REST_Z), 180.0),
    ]);
    let (healer, ally) = (ids[0], ids[1]);
    zone.player_mut(ally).unwrap().health -= 250;
    let idle = input(180.0, 0.0, 0);
    tick(
        &mut zone,
        &world,
        &[(healer, active(0.0, 0.0, 1)), (ally, idle)],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(healer, input(0.0, 0.0, 0)), (ally, idle)],
        24,
    );
    assert_eq!(zone.areas().len(), 1);
    let o = zone.areas()[0].origin;
    assert!(
        (o - Vec3::new(300.0, 0.0, 0.0)).length() < 1.0,
        "under the ally: {o:?}"
    );
    // It heals who stands in it, 60/s while they stay.
    run(
        &mut zone,
        &world,
        &[(healer, input(0.0, 0.0, 0)), (ally, idle)],
        130,
    );
    let healed =
        zone.player(ally).unwrap().health - (zone.player(ally).unwrap().max_health() - 250);
    assert!(
        (100..=150).contains(&healed),
        "healed {healed} in two seconds"
    );

    // At the floor: where the ray meets it. Into the sky: the point at the range, dropped.
    for (pitch, expect_x) in [
        (30.0f32, 46.0 / 30.0f32.to_radians().tan()),
        (-45.0, 353.55),
    ] {
        let (world, mut zone, ids) = arena_builds(&[("mender", Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
        let a = ids[0];
        let aim = |ability: u8| Input {
            pitch,
            ability,
            ..input(0.0, 0.0, 0)
        };
        tick(&mut zone, &world, &[(a, aim(1))], 0);
        run(&mut zone, &world, &[(a, aim(0))], 24);
        assert_eq!(zone.areas().len(), 1, "pitch {pitch}");
        let o = zone.areas()[0].origin;
        assert!(
            (o.x - expect_x).abs() < 6.0 && o.y.abs() < 0.1 && o.z.abs() < 0.5,
            "pitch {pitch}: {o:?}, expected x {expect_x}"
        );
    }
}

#[test]
fn a_quake_is_a_telegraph_you_can_walk_out_of() {
    for stays in [true, false] {
        let (world, mut zone, ids) =
            arena_builds(&[("blade", Vec3::new(260.0, 0.0, REST_Z), 180.0)]);
        let victim = ids[0];
        let (_, def) = zone
            .content
            .creature("warden")
            .expect("the fixture's Warden");
        let sheet = crate::build::Sheet::creature(def, &zone.content, crate::sim::TEAM_WILD);
        assert_eq!(sheet.derived.health, 15000);
        let warden = zone.add_body(sheet, Vec3::new(0.0, 0.0, REST_Z), 0.0, Driver::Mind);
        zone.set_party(warden, 0);
        zone.set_hold(warden, true);
        // Quake is the Warden's first active: a 300 ms cast, then a circle under what it
        // looks at, breaking 1,300 ms later.
        let away = if stays { 0.0 } else { -1.0 };
        for t in 0..130 {
            zone.drive(
                warden,
                if t == 0 {
                    active(0.0, 0.0, 1)
                } else {
                    input(0.0, 0.0, 0)
                },
            );
            // The victim faces the Warden; walking backwards takes it out of the circle.
            tick(&mut zone, &world, &[(victim, input(180.0, away, 0))], 0);
            if t == 40 {
                assert_eq!(
                    zone.areas().len(),
                    1,
                    "the circle is an entity before it breaks"
                );
                // Under the victim as it stood when the cast finished (it has walked about
                // 85 u by then if it is leaving); the circle does not follow it afterwards.
                let o = zone.areas()[0].origin;
                let expect = if stays { 260.0 } else { 345.0 };
                assert!((o.x - expect).abs() < 12.0 && o.z.abs() < 0.5, "{o:?}");
                assert_eq!(hits(&zone, HitKind::Area), 0);
            }
        }
        let p = zone.player(victim).unwrap();
        if stays {
            // 70 ground x 1.16 (INT 14) x 2 (the Warden's might) x 1 (Ground into Water)
            // x 0.9 (ward 0.10) = 146.2.
            assert_eq!(p.max_health() - p.health, 146);
        } else {
            assert_eq!(p.health, p.max_health(), "at {:?}", p.mover.mv.origin);
        }
        assert!(zone.areas().is_empty());
        // The Warden's own quake never touches it.
        let w = zone.player(warden).unwrap();
        assert_eq!(w.health, w.max_health());
    }
}

#[test]
fn a_held_body_stays_down_until_it_is_revived_or_released() {
    let (world, mut zone, ids) = arena_builds(&[
        ("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("blade", Vec3::new(50.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    zone.set_hold(b, true);
    zone.player_mut(b).unwrap().health = 1;
    let idle = input(180.0, 0.0, 0);
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle)],
        13,
    );
    assert!(!zone.player(b).unwrap().alive);
    let wait = zone.rate.ms_to_ticks(RESPAWN_MS) as usize + 20;
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, idle)],
        wait,
    );
    assert!(!zone.player(b).unwrap().alive, "held: no timed respawn");
    // Revived where its holder says, with full pools, and the event says so.
    zone.events.clear();
    zone.revive(b, Vec3::new(400.0, 0.0, REST_Z), 90.0, false);
    let p = zone.player(b).unwrap();
    assert!(p.alive && p.health == p.max_health());
    assert_eq!(p.mover.mv.origin, Vec3::new(400.0, 0.0, REST_Z));
    assert!(zone.events.contains(&ZoneEvent::Respawned(b)));
    // Released while dead: the timed respawn runs from the release.
    zone.player_mut(b).unwrap().health = 0;
    zone.player_mut(b).unwrap().alive = false;
    zone.set_hold(b, false);
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 20);
    assert!(!zone.player(b).unwrap().alive);
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, idle)],
        wait,
    );
    assert!(zone.player(b).unwrap().alive);
}

#[test]
fn a_spot_near_somebody_is_free_and_on_the_ground() {
    let (mut world, zone, ids) = arena_builds(&[("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    let at = zone.player(a).unwrap().mover.mv.origin;
    let spot = zone.spot_near(&world, at, Hull::Player);
    let me = zone.player(a).unwrap().aabb();
    assert!(!Aabb::around(spot, Hull::Player).overlaps(&me), "{spot:?}");
    assert!((spot - at).truncate().length() <= 160.5);
    assert!((spot.z - REST_Z).abs() < 0.1, "on the floor: {spot:?}");
    // A wall right beside: the spot is never behind it.
    world.push(Vec3::new(30.0, -400.0, 0.0), Vec3::new(40.0, 400.0, 200.0));
    for _ in 0..4 {
        let spot = zone.spot_near(&world, at, Hull::Player);
        assert!(spot.x < 30.0 - 16.0 + 0.1, "{spot:?}");
    }
}

/// ITEMS.md 3: what a body wears moves the damage it deals and takes, type by type, from
/// the moment the zone is told; a bolt keeps the edge it was loosed with; the pulses of a
/// status take none; a body in nothing is hit as before.
#[test]
fn worn_gear_moves_damage_by_its_own_type_and_at_once() {
    use crate::matrix::Gear;
    let swing = |attacker: Gear, defender: Gear| {
        let (world, mut zone, ids) = arena(&[
            (Vec3::new(0.0, 0.0, REST_Z), 0.0),
            (Vec3::new(48.0, 0.0, REST_Z), 180.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        zone.set_gear(a, attacker);
        zone.set_gear(b, defender);
        assert!(!zone.player(a).unwrap().fought_within(zone.tick, 640));
        run(
            &mut zone,
            &world,
            &[
                (a, input(0.0, 0.0, buttons::PRIMARY)),
                (b, input(180.0, 0.0, 0)),
            ],
            13,
        );
        // Both are in a fight now, and not for ever.
        for id in [a, b] {
            let p = zone.player(id).unwrap();
            assert!(p.fought_within(zone.tick, 640));
            assert!(!p.fought_within(zone.tick.wrapping_add(641), 640));
        }
        900 - zone.player(b).unwrap().health
    };
    let slash = DamageType::Slash as usize;
    let mut sword = Gear::NONE;
    sword.dealt[slash] = 220;
    let mut cuirass = Gear::NONE;
    cuirass.taken[slash] = 220;
    // 60 slash x 1.25 (cloth) x 0.80 (armour) = 60 in nothing (the test above); a place
    // counts for half its item's edge, so 220 per mille is a factor of 1.11.
    assert_eq!(swing(Gear::NONE, Gear::NONE), 60);
    assert_eq!(swing(sword, Gear::NONE), 67, "60 x 1.11");
    assert_eq!(swing(Gear::NONE, cuirass), 54, "60 / 1.11");
    assert_eq!(swing(sword, cuirass), 60, "a wash");
    // An edge on another type is no edge on this one.
    let mut frost = Gear::NONE;
    frost.dealt[DamageType::Water as usize] = 250;
    frost.taken[DamageType::Water as usize] = 250;
    assert_eq!(swing(frost, frost), 60);
    // What a zone is told is kept within the cap.
    let mut wild = Gear::NONE;
    wild.dealt[slash] = 60_000;
    assert_eq!(swing(wild, Gear::NONE), 68, "60 x 1.125, not x 31");

    // A bolt in flight keeps the edge it was loosed with: the weapon is taken off while it
    // flies, and it lands as it left.
    let bolt = |edge: u16, take_off: bool| {
        let pack = test_content::pack(RATE);
        let (world, mut zone, ids) = arena_with(vec![
            (
                bolt_build(&pack, "crossbow"),
                Vec3::new(0.0, 0.0, REST_Z),
                0.0,
            ),
            (phase2_build(&pack), Vec3::new(600.0, 0.0, REST_Z), 180.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        let mut crossbow = Gear::NONE;
        crossbow.dealt[DamageType::Pierce as usize] = edge;
        zone.set_gear(a, crossbow);
        let idle = input(180.0, 0.0, 0);
        for _ in 0..40 {
            tick(
                &mut zone,
                &world,
                &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle)],
                0,
            );
            if !zone.projectiles().is_empty() {
                break;
            }
        }
        assert!(!zone.projectiles().is_empty(), "the bolt is in the air");
        if take_off {
            zone.set_gear(a, Gear::NONE);
        }
        run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 40);
        assert_eq!(hits(&zone, HitKind::Projectile), 1);
        900 - zone.player(b).unwrap().health
    };
    assert_eq!(bolt(250, true), bolt(250, false));
    assert!(bolt(250, false) > bolt(0, false));

    // The pulses of a status take no gear on either side: a pulse is a point or two, and
    // a factor on that would be a step of a third, not an edge of an eighth. The bolt of
    // flame that set the burn is moved; the burn is the same, with the best of everything
    // or with nothing. (At the content's burn a pulse is 3 points whatever is done to it;
    // the test below this one burns hard enough to tell.)
    let burn = |attacker: Gear, defender: Gear| {
        let pack = test_content::pack(RATE);
        let shade = pack.build("shade").unwrap().clone();
        let (world, mut zone, ids) = arena_with(vec![
            (
                bolt_build(&pack, "firebolt"),
                Vec3::new(0.0, 0.0, REST_Z),
                0.0,
            ),
            (shade, Vec3::new(200.0, 0.0, REST_Z), 180.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        zone.set_gear(a, attacker);
        zone.set_gear(b, defender);
        let idle = input(180.0, 0.0, 0);
        tick(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle)],
            0,
        );
        run(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, 0)), (b, idle)],
            160,
        );
        let sum = |kind: HitKind| -> i32 {
            zone.events
                .iter()
                .filter_map(|e| match e {
                    ZoneEvent::Hit {
                        kind: k, amount, ..
                    } if *k == kind => Some(*amount),
                    _ => None,
                })
                .sum()
        };
        (sum(HitKind::Projectile), sum(HitKind::Dot))
    };
    let mut flame = Gear::NONE;
    flame.dealt[DamageType::Fire as usize] = 250;
    let mut ward = Gear::NONE;
    ward.taken[DamageType::Fire as usize] = 250;
    let (bolt_plain, burn_plain) = burn(Gear::NONE, Gear::NONE);
    let (bolt_armed, burn_armed) = burn(flame, Gear::NONE);
    let (bolt_warded, burn_warded) = burn(Gear::NONE, ward);
    assert!(burn_plain > 0 && bolt_plain > 0);
    assert!(bolt_armed > bolt_plain && bolt_warded < bolt_plain);
    assert_eq!((burn_armed, burn_warded), (burn_plain, burn_plain));

    // What is worn stays through a death, a respawn and a change of build.
    let (world, mut zone, ids) = arena(&[
        (Vec3::new(0.0, 0.0, REST_Z), 0.0),
        (Vec3::new(48.0, 0.0, REST_Z), 180.0),
    ]);
    let (a, b) = (ids[0], ids[1]);
    zone.set_gear(b, cuirass);
    zone.request_respec(b, zone.content.build("blade").unwrap().clone())
        .unwrap();
    zone.player_mut(b).unwrap().health = 1;
    let wait = zone.rate.ms_to_ticks(RESPAWN_MS) as usize + 40;
    run(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, buttons::PRIMARY)),
            (b, input(180.0, 0.0, 0)),
        ],
        13,
    );
    assert!(!zone.player(b).unwrap().alive);
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        wait,
    );
    let p = zone.player(b).unwrap();
    assert!(p.alive && p.gear == cuirass, "{:?}", p.gear);
}

/// ITEMS.md 3.1 and 5, where a blow is made of several moments: the edge is taken when the
/// blow is made, by the body that makes it; a status burns the same whatever is worn; and
/// a fight is dealing or taking damage, nothing else.
#[test]
fn gear_is_taken_when_a_blow_is_made_and_a_fight_is_damage() {
    use crate::matrix::Gear;
    use crate::vocab::{ApplyStatus, StackRule, StatusTarget};
    let slash = DamageType::Slash as usize;
    let mut sword = Gear::NONE;
    sword.dealt[slash] = 220;

    // A swing: the sword that was on when the arm went back is the sword that lands,
    // whatever happens to it in the windup; and one put on in the windup lands as nothing.
    let swing = |before: Gear, during: Gear| {
        let (world, mut zone, ids) = arena(&[
            (Vec3::new(0.0, 0.0, REST_Z), 0.0),
            (Vec3::new(48.0, 0.0, REST_Z), 180.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        zone.set_gear(a, before);
        let idle = input(180.0, 0.0, 0);
        tick(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle)],
            0,
        );
        tick(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, idle)],
            0,
        );
        assert_eq!(hits(&zone, HitKind::Melee), 0, "still in the windup");
        zone.set_gear(a, during);
        run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 12);
        900 - zone.player(b).unwrap().health
    };
    assert_eq!(swing(sword, Gear::NONE), 67, "taken off in the windup");
    assert_eq!(swing(Gear::NONE, sword), 60, "put on in the windup");

    // An area: the caster's edge on the area's kind, the edge of whoever stands in it.
    let stomp = |caster: Gear, target: Gear| {
        let (world, mut zone, ids) = arena_builds(&[
            ("ironclad", Vec3::new(0.0, 0.0, REST_Z), 0.0),
            ("blade", Vec3::new(100.0, 0.0, REST_Z), 180.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        zone.set_gear(a, caster);
        zone.set_gear(b, target);
        let idle = input(180.0, 0.0, 0);
        tick(&mut zone, &world, &[(a, active(0.0, 0.0, 1)), (b, idle)], 0);
        run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 20);
        let pb = zone.player(b).unwrap();
        pb.max_health() - pb.health
    };
    let stone = DamageType::Ground as usize;
    let (mut hammer, mut robe) = (Gear::NONE, Gear::NONE);
    hammer.dealt[stone] = 250;
    robe.taken[stone] = 250;
    // 25.2 unrounded (the stomp test above).
    assert_eq!(stomp(Gear::NONE, Gear::NONE), 25);
    assert_eq!(stomp(hammer, Gear::NONE), 28, "25.2 x 1.125");
    assert_eq!(stomp(Gear::NONE, robe), 22, "25.2 / 1.125");
    assert_eq!(
        stomp(sword, Gear::NONE),
        25,
        "a sword's edge is not a stomp's"
    );

    // A riposte is the parrier's blow: the parrier's sword counts, the attacker's does not.
    let riposte = |attacker: Gear, parrier: Gear| {
        let (world, mut zone, ids) = arena_builds(&[
            ("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0),
            ("blade", Vec3::new(50.0, 0.0, REST_Z), 180.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        zone.set_gear(a, attacker);
        zone.set_gear(b, parrier);
        let (rest_a, rest_b) = (input(0.0, 0.0, 0), input(180.0, 0.0, 0));
        tick(
            &mut zone,
            &world,
            &[(a, input(0.0, 0.0, buttons::PRIMARY)), (b, rest_b)],
            0,
        );
        run(&mut zone, &world, &[(a, rest_a), (b, rest_b)], 2);
        tick(
            &mut zone,
            &world,
            &[(a, rest_a), (b, input(180.0, 0.0, buttons::GUARD))],
            0,
        );
        // The blow was parried: nobody was hurt, and nobody is in a fight for it.
        run(&mut zone, &world, &[(a, rest_a), (b, rest_b)], 3);
        if zone
            .events
            .iter()
            .any(|e| matches!(e, ZoneEvent::Parried { .. }))
            && hits(&zone, HitKind::Melee) == 0
        {
            for id in [a, b] {
                assert!(!zone.player(id).unwrap().fought_within(zone.tick, 640));
            }
        }
        run(&mut zone, &world, &[(a, rest_a), (b, rest_b)], 12);
        assert!(
            zone.events
                .iter()
                .any(|e| matches!(e, ZoneEvent::Parried { .. }))
        );
        let pa = zone.player(a).unwrap();
        pa.max_health() - pa.health
    };
    let plain = riposte(Gear::NONE, Gear::NONE);
    assert!(plain > 10, "{plain}");
    assert_eq!(
        riposte(sword, Gear::NONE),
        plain,
        "the attacker's own sword"
    );
    let armed = riposte(Gear::NONE, sword);
    assert!(
        armed > plain && armed as f32 <= plain as f32 * 1.11 + 1.0,
        "{plain} then {armed}"
    );

    // A status burns the same with the best of everything and with nothing: a burn hard
    // enough that an eighth more or less would show in its first pulse.
    let burn = |attacker: Gear, defender: Gear| {
        let (world, mut zone, ids) = arena_builds(&[
            ("blade", Vec3::new(0.0, 0.0, REST_Z), 0.0),
            ("shade", Vec3::new(200.0, 0.0, REST_Z), 180.0),
        ]);
        let (a, b) = (ids[0], ids[1]);
        zone.set_gear(a, attacker);
        zone.set_gear(b, defender);
        let hard = ApplyStatus {
            status: Status::Burn,
            duration: zone.rate.ms_to_ticks(3000),
            magnitude: 160.0,
            max_stacks: 1,
            stacking: StackRule::Refresh,
            target: StatusTarget::Hit,
            dispellable: false,
        };
        zone.apply_status(b, a, &hard);
        let idle = input(180.0, 0.0, 0);
        run(&mut zone, &world, &[(a, input(0.0, 0.0, 0)), (b, idle)], 40);
        // The burn's pulses are the fight: both are in it, though no blow was struck.
        for id in [a, b] {
            assert!(zone.player(id).unwrap().fought_within(zone.tick, 640));
        }
        zone.events
            .iter()
            .find_map(|e| match e {
                ZoneEvent::Hit {
                    kind: HitKind::Dot,
                    amount,
                    ..
                } => Some(*amount),
                _ => None,
            })
            .expect("a pulse")
    };
    let flame = DamageType::Fire as usize;
    let (mut staff, mut ward) = (Gear::NONE, Gear::NONE);
    staff.dealt[flame] = 250;
    ward.taken[flame] = 250;
    let pulse = burn(Gear::NONE, Gear::NONE);
    assert!(pulse >= 12, "a pulse an eighth would move: {pulse}");
    assert_eq!(burn(staff, Gear::NONE), pulse);
    assert_eq!(burn(Gear::NONE, ward), pulse);
    assert_eq!(burn(staff, ward), pulse);

    // The fight lock's edges: in a fight for exactly as many ticks as it is long, across
    // the counter's wrap, and never before the first blow.
    let (_, mut zone, ids) = arena(&[(Vec3::new(0.0, 0.0, REST_Z), 0.0)]);
    let a = ids[0];
    assert!(!zone.player(a).unwrap().fought_within(0, u32::MAX));
    zone.player_mut(a).unwrap().fought_at = Some(1000);
    let p = zone.player(a).unwrap();
    assert!(p.fought_within(1000, 640) && p.fought_within(1639, 640));
    assert!(!p.fought_within(1640, 640));
    zone.player_mut(a).unwrap().fought_at = Some(u32::MAX - 2);
    let p = zone.player(a).unwrap();
    assert!(p.fought_within(5, 640) && !p.fought_within(640, 640));

    // Healing is not a fight: a mend dart marks neither the mender nor the mended.
    let (world, mut zone, ids) = arena_builds(&[
        ("mender", Vec3::new(0.0, 0.0, REST_Z), 0.0),
        ("ironclad", Vec3::new(200.0, 0.0, REST_Z), 180.0),
    ]);
    let (healer, tank) = (ids[0], ids[1]);
    zone.player_mut(tank).unwrap().health -= 60;
    let idle = input(180.0, 0.0, 0);
    tick(
        &mut zone,
        &world,
        &[(healer, input(0.0, 0.0, buttons::SECONDARY)), (tank, idle)],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(healer, input(0.0, 0.0, 0)), (tank, idle)],
        80,
    );
    assert!(
        zone.player(tank).unwrap().health > 140 - 60 - 1,
        "it healed"
    );
    for id in [healer, tank] {
        assert!(!zone.player(id).unwrap().fought_within(zone.tick, 640));
    }
}

/// A status pulses four times a second at any tick rate: a burn of 20 a second does 20 in
/// a second at 64 Hz and at 20 Hz alike (it did 6 at 20 Hz while the interval was a count
/// of 64 Hz ticks).
#[test]
fn a_status_pulses_four_times_a_second_at_any_rate() {
    use crate::vocab::{ApplyStatus, StackRule, StatusTarget};
    for hz in [64u32, 20] {
        let rate = TickRate::new(hz);
        let world = BoxWorld::floor();
        let spawns = [(0.0f32, 0.0f32), (200.0, 180.0)]
            .iter()
            .map(|&(x, yaw)| Spawn {
                origin: Vec3::new(x, 0.0, REST_Z),
                yaw,
                team: 0,
            })
            .collect();
        let mut zone = Zone::new(rate, 7, spawns, test_content::pack(rate));
        let build = zone.content.build("blade").unwrap().clone();
        let a = zone.add_player_at(build.clone(), 0, Vec3::new(0.0, 0.0, REST_Z), 0.0);
        let b = zone.add_player_at(build, 0, Vec3::new(200.0, 0.0, REST_Z), 180.0);
        let burn = ApplyStatus {
            status: Status::Burn,
            duration: rate.ms_to_ticks(10_000),
            magnitude: 40.0,
            max_stacks: 1,
            stacking: StackRule::Refresh,
            target: StatusTarget::Hit,
            dispellable: false,
        };
        zone.apply_status(b, a, &burn);
        let before = zone.player(b).unwrap().health;
        let idle = [(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))];
        run(&mut zone, &world, &idle, 2 * hz as usize);
        let pulses = hits(&zone, HitKind::Dot);
        assert!(
            (7..=9).contains(&pulses),
            "{hz} Hz: {pulses} pulses in two seconds"
        );
        let lost = before - zone.player(b).unwrap().health;
        // The same burn does the same in two seconds at either rate, to a pulse.
        let per_pulse = lost as f32 / pulses as f32;
        assert!(
            (lost as f32 - 8.0 * per_pulse).abs() <= per_pulse + 0.5,
            "{hz} Hz: {lost} in {pulses} pulses"
        );
        assert!(lost >= 20, "{hz} Hz: {lost}");
    }
}

// ---------- MODES.md: the action mode (15a) ----------

/// Two bodies on different teams, `apart` units apart on the x axis, the first facing
/// `yaw`, the second facing it.
fn duel(yaw: f32, apart: f32) -> (BoxWorld, Zone, EntityId, EntityId) {
    let world = BoxWorld::floor();
    let mut zone = zone_with(vec![
        (Vec3::new(0.0, 0.0, REST_Z), yaw),
        (Vec3::new(apart, 0.0, REST_Z), 180.0),
    ]);
    let build = phase2_build(&zone.content);
    let a = zone.add_player_at(build.clone(), 1, Vec3::new(0.0, 0.0, REST_Z), yaw);
    let b = zone.add_player_at(build, 2, Vec3::new(apart, 0.0, REST_Z), 180.0);
    (world, zone, a, b)
}

fn kit_index(zone: &Zone, id: EntityId, key: &str) -> u8 {
    let p = zone.player(id).unwrap();
    let pack_index = zone.content.find(key).expect(key);
    let ability_id = zone.content.abilities[pack_index as usize].ability.id;
    p.sheet
        .kit
        .abilities
        .iter()
        .position(|a| a.id == ability_id)
        .map(|i| i as u8)
        .unwrap_or_else(|| panic!("{key} is not in the kit"))
}

fn running(zone: &Zone, id: EntityId) -> Option<u8> {
    zone.player(id).unwrap().mover.script.map(|s| s.ability)
}

#[test]
fn a_chain_plays_its_stages_in_the_window_and_the_third_knocks_down() {
    let (world, mut zone, a, b) = duel(0.0, 48.0);
    let (s1, s2, s3) = (
        kit_index(&zone, a, "sword"),
        kit_index(&zone, a, "sword_2"),
        kit_index(&zone, a, "sword_3"),
    );
    // The target presses toward the attacker, as a fighter does: the knockback of the
    // first blows would otherwise carry it out of the third's reach.
    let idle = |zone: &mut Zone, world: &BoxWorld, n: usize| {
        run(
            zone,
            world,
            &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 1.0, 0))],
            n,
        )
    };
    let press = |zone: &mut Zone, world: &BoxWorld| {
        tick(
            zone,
            world,
            &[
                (a, input(0.0, 0.0, buttons::PRIMARY)),
                (b, input(180.0, 1.0, 0)),
            ],
            0,
        )
    };
    press(&mut zone, &world);
    idle(&mut zone, &world, 1);
    assert_eq!(running(&zone, a), Some(s1));
    // Pressed again in the recovery (the sword commits at windup + active = 14 ticks):
    // the recovery is cut and the second stage plays.
    idle(&mut zone, &world, 16);
    assert_eq!(running(&zone, a), Some(s1), "still in its recovery");
    press(&mut zone, &world);
    idle(&mut zone, &world, 1);
    assert_eq!(running(&zone, a), Some(s2), "the second stage");
    // Let the second stage end; within its window the slot still plays the third.
    idle(&mut zone, &world, 40);
    assert_eq!(running(&zone, a), None);
    assert!(
        zone.player(a).unwrap().mover.chain.is_some(),
        "the window is open"
    );
    press(&mut zone, &world);
    idle(&mut zone, &world, 1);
    assert_eq!(running(&zone, a), Some(s3), "the third stage");
    // The third lands and puts the target on the ground.
    idle(&mut zone, &world, 30);
    let t = zone.player(b).unwrap();
    assert!(
        t.mover.statuses.has(Status::Knockdown),
        "{:?}",
        t.mover.statuses
    );
    assert_eq!(t.anim, anim::DOWN);
    assert_eq!(hits(&zone, HitKind::Melee), 3);
    // Down, the body goes nowhere of its own (the knockback's last push aside) and
    // swings nothing.
    idle(&mut zone, &world, 12);
    let before = zone.player(b).unwrap().mover.mv.origin;
    run(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, 0)),
            (b, input(180.0, 1.0, buttons::PRIMARY)),
        ],
        8,
    );
    let t = zone.player(b).unwrap();
    assert!(
        (t.mover.mv.origin - before).length() < 4.0,
        "{:?} from {before:?}",
        t.mover.mv.origin
    );
    assert_eq!(t.mover.script, None);
    // After the chain the window closes by itself: the slot is the first stage again.
    idle(&mut zone, &world, 80);
    assert!(zone.player(a).unwrap().mover.chain.is_none());
    press(&mut zone, &world);
    idle(&mut zone, &world, 1);
    assert_eq!(running(&zone, a), Some(s1));
}

#[test]
fn the_chains_window_closes_and_a_press_too_late_is_the_first_stage() {
    let (world, mut zone, a, b) = duel(0.0, 400.0);
    let s1 = kit_index(&zone, a, "sword");
    let press = |zone: &mut Zone, world: &BoxWorld| {
        tick(
            zone,
            world,
            &[
                (a, input(0.0, 0.0, buttons::PRIMARY)),
                (b, input(180.0, 0.0, 0)),
            ],
            0,
        )
    };
    press(&mut zone, &world);
    // The script (34 ticks) and the window (400 ms = 26 ticks) both over.
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0))], 70);
    assert!(zone.player(a).unwrap().mover.chain.is_none());
    press(&mut zone, &world);
    run(&mut zone, &world, &[(a, input(0.0, 0.0, 0))], 1);
    assert_eq!(running(&zone, a), Some(s1));
}

#[test]
fn a_dash_cuts_a_recovery_and_cannot_be_hit_in_its_first_150_ms() {
    let (world, mut zone, a, b) = duel(0.0, 48.0);
    let dash = kit_index(&zone, a, "dash");
    tick(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, buttons::PRIMARY)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        13,
    );
    // The other starts a swing (it lands ten ticks on)...
    tick(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, 0)),
            (b, input(180.0, 0.0, buttons::PRIMARY)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        3,
    );
    assert!(
        running(&zone, a).is_some_and(|s| s != dash),
        "the sword in its recovery"
    );
    // ...and Space (active 1) in the recovery: the dash starts at once, untouchable.
    tick(
        &mut zone,
        &world,
        &[(a, active(0.0, 0.0, 1)), (b, input(180.0, 0.0, 0))],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        1,
    );
    assert_eq!(running(&zone, a), Some(dash));
    let p = zone.player(a).unwrap();
    assert!(p.mover.invulnerable(p.last_input_tick));
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        12,
    );
    let on_a = zone
        .events
        .iter()
        .filter(|e| matches!(e, ZoneEvent::Hit { target, .. } if *target == a))
        .count();
    assert_eq!(on_a, 0, "the blow passed through the dodge");
    assert_eq!(zone.player(a).unwrap().health, 900);
    assert_eq!(
        hits(&zone, HitKind::Melee),
        1,
        "the one blow that landed is the first sword's, on the other"
    );
}

#[test]
fn the_magnet_turns_a_swing_toward_the_nearest_enemy_but_not_an_ally() {
    // Facing 70 degrees off a body 48 units away: a 90-degree arc misses it; the
    // magnet's 30 degrees bring it within the arc.
    for (team, expect) in [(2u8, true), (1u8, false)] {
        let world = BoxWorld::floor();
        let mut zone = zone_with(vec![
            (Vec3::new(0.0, 0.0, REST_Z), 70.0),
            (Vec3::new(48.0, 0.0, REST_Z), 180.0),
        ]);
        let build = phase2_build(&zone.content);
        let a = zone.add_player_at(build.clone(), 1, Vec3::new(0.0, 0.0, REST_Z), 70.0);
        let b = zone.add_player_at(build, team, Vec3::new(48.0, 0.0, REST_Z), 180.0);
        run(
            &mut zone,
            &world,
            &[
                (a, input(70.0, 0.0, buttons::PRIMARY)),
                (b, input(180.0, 0.0, 0)),
            ],
            13,
        );
        assert_eq!(hits(&zone, HitKind::Melee), expect as usize, "team {team}");
        let yaw = zone.player(a).unwrap().mover.yaw;
        if expect {
            assert!((yaw - 40.0).abs() < 1.0, "turned 30 toward it: {yaw}");
        } else {
            assert!((yaw - 70.0).abs() < 1e-3, "left alone: {yaw}");
        }
    }
}

#[test]
fn controls_diminish_the_second_lasts_half_the_third_does_nothing() {
    let (world, mut zone, a, b) = duel(0.0, 400.0);
    let down = crate::vocab::ApplyStatus {
        status: Status::Knockdown,
        duration: 64,
        magnitude: 1.0,
        max_stacks: 1,
        stacking: crate::vocab::StackRule::Refresh,
        target: crate::vocab::StatusTarget::Hit,
        dispellable: true,
    };
    let left = |zone: &Zone| {
        let p = zone.player(b).unwrap();
        p.mover
            .statuses
            .get(Status::Knockdown)
            .map(|s| tick_delta(s.until, p.last_input_tick))
    };
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        2,
    );
    zone.apply_status(b, a, &down);
    assert_eq!(left(&zone), Some(64));
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        70,
    );
    assert_eq!(left(&zone), None, "it ran out");
    zone.apply_status(b, a, &down);
    assert_eq!(left(&zone), Some(32), "the second lasts half");
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        40,
    );
    zone.apply_status(b, a, &down);
    assert_eq!(left(&zone), None, "the third does nothing");
    // Ten seconds later the count is forgotten.
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        64 * 11,
    );
    zone.apply_status(b, a, &down);
    assert_eq!(left(&zone), Some(64));
}

#[test]
fn launched_lifts_the_body_and_it_is_down_until_it_lands() {
    let (world, mut zone, a, b) = duel(0.0, 400.0);
    let up = crate::vocab::ApplyStatus {
        status: Status::Launched,
        duration: 40,
        magnitude: 300.0,
        max_stacks: 1,
        stacking: crate::vocab::StackRule::Refresh,
        target: crate::vocab::StatusTarget::Hit,
        dispellable: true,
    };
    run(
        &mut zone,
        &world,
        &[(a, input(0.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        2,
    );
    zone.apply_status(b, a, &up);
    let t = zone.player(b).unwrap();
    assert!(t.mover.mv.velocity.z >= 300.0 && !t.mover.mv.on_ground);
    run(
        &mut zone,
        &world,
        &[
            (a, input(0.0, 0.0, 0)),
            (b, input(180.0, 1.0, buttons::PRIMARY)),
        ],
        6,
    );
    let t = zone.player(b).unwrap();
    assert!(
        t.mover.mv.origin.z > REST_Z + 10.0,
        "in the air: {:?}",
        t.mover.mv.origin
    );
    assert_eq!(t.anim, anim::DOWN);
    assert_eq!(t.mover.script, None, "nothing is swung from the air");
}

// ---------- MODES.md: the gun mode (15b) ----------

/// The musketeer (a musket of one round, a pistol of eight, the knife) facing `yaw`,
/// and a blade `apart` units along x facing it, on different teams.
fn gun_duel(yaw: f32, apart: f32) -> (BoxWorld, Zone, EntityId, EntityId) {
    gun_duel_at(RATE, yaw, apart)
}

/// The same at a zone's `rate` (a town runs at 20 Hz).
fn gun_duel_at(rate: TickRate, yaw: f32, apart: f32) -> (BoxWorld, Zone, EntityId, EntityId) {
    let world = BoxWorld::floor();
    let spawns = [
        (Vec3::new(0.0, 0.0, REST_Z), yaw),
        (Vec3::new(apart, 0.0, REST_Z), 180.0),
    ]
    .into_iter()
    .map(|(origin, yaw)| Spawn {
        origin,
        yaw,
        team: 0,
    })
    .collect();
    let mut zone = Zone::new(rate, 7, spawns, test_content::pack(rate));
    let gunner = zone.content.build("musketeer").expect("musketeer").clone();
    let blade = zone.content.build("blade").expect("blade").clone();
    let a = zone.add_player_at(gunner, 1, Vec3::new(0.0, 0.0, REST_Z), yaw);
    let b = zone.add_player_at(blade, 2, Vec3::new(apart, 0.0, REST_Z), 180.0);
    // What the hub would read from its inventory (MODES.md 11): the stacks the two
    // firearms name, and two kits.
    zone.set_stacks(
        a,
        &[
            ("ball".to_string(), 24, None),
            ("pistol_round".to_string(), 32, None),
            ("kit".to_string(), 2, Some(50)),
        ],
        &[Some("kit".to_string()), None, None, None],
    );
    (world, zone, a, b)
}

fn held(yaw: f32, forward: f32, buttons: u16, held: u8) -> Input {
    Input {
        held,
        ..input(yaw, forward, buttons)
    }
}

fn gun(zone: &Zone, id: EntityId, i: usize) -> GunState {
    zone.player(id).unwrap().mover.guns[i]
}

fn bolts(zone: &Zone) -> usize {
    zone.events
        .iter()
        .filter(|e| matches!(e, ZoneEvent::ProjectileSpawned { .. }))
        .count()
}

#[test]
fn a_gun_build_carries_its_knife_and_loads_at_a_spawn() {
    let (_, zone, a, _) = gun_duel(0.0, 400.0);
    let p = zone.player(a).unwrap();
    let kit = &p.sheet.kit;
    assert_eq!(kit.mode, crate::vocab::Mode::Gun);
    assert!(kit.knife.is_some(), "the knife comes with the mode");
    assert!(kit.guard.is_none());
    assert_eq!(
        (gun(&zone, a, 0).magazine, gun(&zone, a, 0).reserve),
        (1, 24)
    );
    assert_eq!(
        (gun(&zone, a, 1).magazine, gun(&zone, a, 1).reserve),
        (8, 32)
    );
}

#[test]
fn the_musket_fires_one_round_then_reloads_by_itself_and_r_reloads_a_pistol() {
    let (world, mut zone, a, b) = gun_duel(0.0, 400.0);
    let quiet = |zone: &mut Zone, world: &BoxWorld, n: usize, h: u8| {
        run(
            zone,
            world,
            &[(a, held(0.0, 0.0, 0, h)), (b, input(180.0, 0.0, 0))],
            n,
        )
    };
    // One shot: the magazine is empty and the bolt flies.
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::PRIMARY, 0)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    quiet(&mut zone, &world, 2, 0);
    assert_eq!(bolts(&zone), 1);
    assert_eq!(gun(&zone, a, 0).magazine, 0);
    // The empty magazine reloads by itself (2.8 s), no trigger needed; a pull flies nothing.
    quiet(&mut zone, &world, 80, 0);
    let p = zone.player(a).unwrap();
    assert!(
        p.mover.reloading(p.last_input_tick),
        "reloads with no key pressed"
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::PRIMARY, 0)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    quiet(&mut zone, &world, 2, 0);
    assert_eq!(bolts(&zone), 1);
    let p = zone.player(a).unwrap();
    assert!(p.mover.reloading(p.last_input_tick));
    assert_eq!(p.anim, anim::RELOAD);
    // While it reloads the trigger does nothing; when it is done the round is in.
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::PRIMARY, 0)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    quiet(&mut zone, &world, 190, 0);
    assert_eq!(bolts(&zone), 1);
    assert_eq!(
        (gun(&zone, a, 0).magazine, gun(&zone, a, 0).reserve),
        (1, 23)
    );
    // The pistol in hand: three shots, then R puts the rounds back from the reserve.
    quiet(&mut zone, &world, 1, 1);
    for _ in 0..3 {
        tick(
            &mut zone,
            &world,
            &[
                (a, held(0.0, 0.0, buttons::PRIMARY, 1)),
                (b, input(180.0, 0.0, 0)),
            ],
            0,
        );
        quiet(&mut zone, &world, 14, 1);
    }
    assert_eq!(bolts(&zone), 4);
    assert_eq!(gun(&zone, a, 1).magazine, 5);
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::RELOAD, 1)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    quiet(&mut zone, &world, 110, 1);
    assert_eq!(
        (gun(&zone, a, 1).magazine, gun(&zone, a, 1).reserve),
        (8, 29)
    );
}

#[test]
fn the_cycle_bounds_the_rate_and_a_stagger_drops_the_reload_keeping_the_rounds() {
    let (world, mut zone, a, b) = gun_duel(0.0, 400.0);
    // The pistol's cycle is 180 ms (12 ticks): a click a tick fires every twelfth.
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        2,
    );
    for i in 0..24 {
        let b_ = if i % 2 == 0 { buttons::PRIMARY } else { 0 };
        tick(
            &mut zone,
            &world,
            &[(a, held(0.0, 0.0, b_, 1)), (b, input(180.0, 0.0, 0))],
            0,
        );
    }
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        2,
    );
    assert_eq!(bolts(&zone), 2, "two shots in 24 ticks at a 12-tick cycle");
    // A reload begun, then a stagger: the reload is dropped, the rounds are where they were.
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::RELOAD, 1)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        3,
    );
    let p = zone.player(a).unwrap();
    assert!(p.mover.reloading(p.last_input_tick));
    let stagger = crate::vocab::ApplyStatus {
        status: Status::Stagger,
        duration: 20,
        magnitude: 1.0,
        max_stacks: 1,
        stacking: crate::vocab::StackRule::Refresh,
        target: crate::vocab::StatusTarget::Hit,
        dispellable: true,
    };
    zone.apply_status(a, b, &stagger);
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        2,
    );
    let p = zone.player(a).unwrap();
    assert!(!p.mover.reloading(p.last_input_tick));
    assert_eq!(
        (gun(&zone, a, 1).magazine, gun(&zone, a, 1).reserve),
        (6, 32)
    );
}

#[test]
fn the_pattern_kicks_each_shot_of_a_spray_and_the_cone_opens_on_the_move() {
    // Standing still, the pistol's first shot flies within its cone (0.8 degrees) of the
    // aim, turned by nothing (MODES.md 3.3); the second, 12 ticks on, is turned by the
    // pattern's first pair (0.0, 0.8), the kick after the first shot.
    let (world, mut zone, a, b) = gun_duel(0.0, 2000.0);
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        2,
    );
    let dir_of = |zone: &Zone| -> Vec3 {
        let pr = zone.projectiles().last().expect("a bolt");
        pr.vel.normalize()
    };
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::PRIMARY, 1)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        1,
    );
    let first = dir_of(&zone);
    let off = first.dot(Vec3::X).clamp(-1.0, 1.0).acos().to_degrees();
    assert!(
        off <= 0.8 + 0.1,
        "the first shot: {off} degrees off the aim"
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        11,
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::PRIMARY, 1)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        1,
    );
    let second = dir_of(&zone);
    let lift2 = second.z.asin().to_degrees();
    // The second shot's cone: the stance's 0.8 and the spray's 0.8 × (1/4)².
    assert!(
        lift2 > 0.8 - 0.85 - 0.1 && lift2 < 0.8 + 0.85 + 0.1,
        "the second: {lift2}"
    );
    assert!(
        second.z > first.z - 0.5f32.to_radians(),
        "the spray climbs: {} then {}",
        first.z,
        second.z
    );
    // On the run the cone opens to the move's share: many shots scatter wider than standing.
    let mut spread_still = 0.0f32;
    let mut spread_moving = 0.0f32;
    for (moving, spread) in [(0.0, &mut spread_still), (1.0, &mut spread_moving)] {
        // (The other stands far off: a runner covers two thousand units in the time.)
        let (world, mut zone, a, b) = gun_duel(0.0, 6000.0);
        run(
            &mut zone,
            &world,
            &[(a, held(0.0, moving, 0, 1)), (b, input(180.0, 0.0, 0))],
            40,
        );
        for _ in 0..8 {
            tick(
                &mut zone,
                &world,
                &[
                    (a, held(0.0, moving, buttons::PRIMARY, 1)),
                    (b, input(180.0, 0.0, 0)),
                ],
                0,
            );
            run(
                &mut zone,
                &world,
                &[(a, held(0.0, moving, 0, 1)), (b, input(180.0, 0.0, 0))],
                1,
            );
            let d = dir_of(&zone);
            let yaw_off = d.y.atan2(d.x).to_degrees().abs();
            *spread = spread.max(yaw_off);
            // A long pause between shots: no spray, so only the stance and the movement.
            run(
                &mut zone,
                &world,
                &[(a, held(0.0, moving, 0, 1)), (b, input(180.0, 0.0, 0))],
                50,
            );
        }
    }
    assert!(spread_still <= 0.8 + 0.1, "standing: {spread_still}");
    assert!(
        spread_moving > spread_still,
        "moving {spread_moving} vs still {spread_still}"
    );
}

#[test]
fn the_bolt_leaves_the_muzzle_for_the_point_under_the_crosshair() {
    // The director (2026-10-07): under the scope the mark was always up and right of the
    // crosshair. The musket's muzzle is 20 ahead, 4 right and 2 below the eye; its first
    // shot, scoped and still (a cone of 0.05°), flies from there (right is -Y at yaw 0)
    // to the point the eye
    // ray meets, the other body's capsule 2,000 u ahead, not parallel to the look.
    let (world, mut zone, a, b) = gun_duel(0.0, 2000.0);
    let still = held(0.0, 0.0, buttons::SCOPE, 0);
    run(
        &mut zone,
        &world,
        &[(a, still), (b, input(180.0, 0.0, 0))],
        4,
    );
    let eye = zone.player(a).unwrap().mover.eye();
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::SCOPE | buttons::PRIMARY, 0)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, still), (b, input(180.0, 0.0, 0))],
        1,
    );
    let (origin, dir) = match zone.events.iter().rev().find_map(|e| match e {
        ZoneEvent::ProjectileSpawned { origin, .. } => Some(*origin),
        _ => None,
    }) {
        Some(o) => (
            o,
            zone.projectiles().last().expect("a bolt").vel.normalize(),
        ),
        None => panic!("no bolt"),
    };
    assert!(
        (origin - (eye + Vec3::new(20.0, -4.0, -2.0))).length() < 0.01,
        "the muzzle: {origin:?} for the eye {eye:?}"
    );
    // Where the bolt crosses the other body's plane: at the eye's height and on its line.
    let bx = zone.player(b).unwrap().mover.mv.origin.x - 16.0;
    let t = (bx - origin.x) / dir.x;
    let at = origin + dir * t;
    assert!(
        (at.y - eye.y).abs() < 2.5 && (at.z - eye.z).abs() < 2.5,
        "the bolt crosses the target at {at:?}, the eye ray at ({}, {})",
        eye.y,
        eye.z
    );
}

#[test]
fn a_crouch_lowers_the_eye_and_halves_the_pace() {
    let (world, mut zone, a, b) = gun_duel(0.0, 6000.0);
    let standing = zone.player(a).unwrap().mover.eye();
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 1.0, 0, 0)), (b, input(180.0, 0.0, 0))],
        40,
    );
    let run_speed = zone.player(a).unwrap().mover.mv.ground_speed();
    run(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 1.0, buttons::CROUCH, 0)),
            (b, input(180.0, 0.0, 0)),
        ],
        40,
    );
    let m = &zone.player(a).unwrap().mover;
    assert!(m.crouched);
    assert!(
        (m.mv.ground_speed() - run_speed * 0.5).abs() < run_speed * 0.1,
        "crouched {} of a run's {run_speed}",
        m.mv.ground_speed()
    );
    assert!(
        ((standing.z - m.eye().z) - CROUCH_DROP).abs() < 0.01,
        "the eye: {} standing, {} crouched",
        standing.z,
        m.eye().z
    );
    // Let go: the eye is back where it was.
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 0)), (b, input(180.0, 0.0, 0))],
        3,
    );
    let m = &zone.player(a).unwrap().mover;
    assert!(!m.crouched && (m.eye().z - standing.z).abs() < 0.01);
}

#[test]
fn a_crouched_body_is_shorter_and_a_shot_at_its_standing_head_passes_over() {
    // MODES.md 3.5: the hitbox loses CROUCH_DROP off its top while crouched, on the live
    // body and in the rewind, so the head band comes down with the body.
    let (world, mut zone, a, b) = gun_duel(0.0, 300.0);
    let standing = zone.player(b).unwrap().capsule();
    run(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, 0, 0)),
            (b, input(180.0, 0.0, buttons::CROUCH)),
        ],
        4,
    );
    let crouched = zone.player(b).unwrap().capsule();
    let top = |c: &Capsule| c.a.z.max(c.b.z) + c.radius;
    assert!(
        ((top(&standing) - top(&crouched)) - CROUCH_DROP).abs() < 0.01,
        "the top: {} standing, {} crouched",
        top(&standing),
        top(&crouched)
    );
    assert_eq!(
        standing.a.z.min(standing.b.z),
        crouched.a.z.min(crouched.b.z)
    );
    assert_eq!(
        zone.history().body_at(zone.tick, b).map(|(_, c)| c),
        Some(true),
        "the history remembers the posture"
    );
    // A's eye is 46 u over the feet, B's standing band from 44 up and B's crouched top at
    // 40: the level shot that was a headshot flies over the crouched body (the eye is
    // level, so the bolt is at 46 u at 300 u, over a top of 40).
    let aim = Input {
        pitch: 0.0,
        ..held(0.0, 0.0, buttons::PRIMARY | buttons::SCOPE, 0)
    };
    tick(
        &mut zone,
        &world,
        &[(a, aim), (b, input(180.0, 0.0, buttons::CROUCH))],
        0,
    );
    run(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::SCOPE, 0)),
            (b, input(180.0, 0.0, buttons::CROUCH)),
        ],
        6,
    );
    assert_eq!(bolts(&zone), 1, "the shot was fired");
    assert!(
        !zone
            .events
            .iter()
            .any(|e| matches!(e, ZoneEvent::Hit { target, .. } if *target == b)),
        "a level shot passes over a crouched body"
    );
    // Aimed a little down it enters the crouched hull near its top: the head band.
    let (world, mut zone, a, b) = gun_duel(0.0, 300.0);
    run(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, 0, 0)),
            (b, input(180.0, 0.0, buttons::CROUCH)),
        ],
        4,
    );
    // 46 - 300 tan(2.1°) = 35: inside the top 12 of a 40 u hull.
    let aim = Input {
        pitch: 2.1,
        ..held(0.0, 0.0, buttons::PRIMARY | buttons::SCOPE, 0)
    };
    tick(
        &mut zone,
        &world,
        &[(a, aim), (b, input(180.0, 0.0, buttons::CROUCH))],
        0,
    );
    run(
        &mut zone,
        &world,
        &[
            (
                a,
                Input {
                    pitch: 2.1,
                    ..held(0.0, 0.0, buttons::SCOPE, 0)
                },
            ),
            (b, input(180.0, 0.0, buttons::CROUCH)),
        ],
        6,
    );
    let hit = zone
        .events
        .iter()
        .find_map(|e| match e {
            ZoneEvent::Hit { target, amount, .. } if *target == b => Some(*amount),
            _ => None,
        })
        .unwrap_or(0);
    let body = {
        // The same shot into a standing body enters at 35 u: under its band of 44.
        let (world, mut zone, a, b) = gun_duel(0.0, 300.0);
        run(
            &mut zone,
            &world,
            &[(a, held(0.0, 0.0, 0, 0)), (b, input(180.0, 0.0, 0))],
            4,
        );
        tick(
            &mut zone,
            &world,
            &[
                (
                    a,
                    Input {
                        pitch: 2.1,
                        ..held(0.0, 0.0, buttons::PRIMARY | buttons::SCOPE, 0)
                    },
                ),
                (b, input(180.0, 0.0, 0)),
            ],
            0,
        );
        run(
            &mut zone,
            &world,
            &[
                (
                    a,
                    Input {
                        pitch: 2.1,
                        ..held(0.0, 0.0, buttons::SCOPE, 0)
                    },
                ),
                (b, input(180.0, 0.0, 0)),
            ],
            6,
        );
        zone.events
            .iter()
            .find_map(|e| match e {
                ZoneEvent::Hit { target, amount, .. } if *target == b => Some(*amount),
                _ => None,
            })
            .unwrap_or(0)
    };
    assert!(body > 0, "the shot lands on a standing body: {body}");
    assert!(
        hit >= body * 3,
        "the crouched band is where the crouched head is: {hit} against {body}"
    );
}

#[test]
fn the_spray_forgets_itself_after_recover() {
    // The pistol's recover is 350 ms: a second shot inside it is the spray's second, a
    // shot after a pause longer than that is a first again (click, pause, click).
    let (world, mut zone, a, b) = gun_duel(0.0, 2000.0);
    let shoot = |zone: &mut Zone| {
        tick(
            zone,
            &world,
            &[
                (a, held(0.0, 0.0, buttons::PRIMARY, 1)),
                (b, input(180.0, 0.0, 0)),
            ],
            0,
        );
    };
    let rest = |zone: &mut Zone, ticks: usize| {
        run(
            zone,
            &world,
            &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
            ticks,
        );
    };
    rest(&mut zone, 2);
    shoot(&mut zone);
    rest(&mut zone, 1);
    assert_eq!(gun(&zone, a, 1).spray, 1);
    rest(&mut zone, 11);
    shoot(&mut zone);
    rest(&mut zone, 1);
    assert_eq!(gun(&zone, a, 1).spray, 2, "the second within recover");
    rest(&mut zone, RATE.ms_to_ticks(350) as usize + 2);
    shoot(&mut zone);
    rest(&mut zone, 1);
    assert_eq!(gun(&zone, a, 1).spray, 1, "a first again after the pause");
}

#[test]
fn the_cone_is_the_roots_shape() {
    // Nothing from the move up to a creep of half speed, all of it from four fifths; the
    // spray's share grows with the square of the shots to four times `shot` by the eighth.
    assert_eq!(move_share(0.0), 0.0);
    assert_eq!(move_share(0.5), 0.0);
    assert!(move_share(0.65) > 0.4 && move_share(0.65) < 0.6);
    assert_eq!(move_share(0.8), 1.0);
    assert_eq!(move_share(1.2), 1.0);
}

#[test]
fn a_bolt_in_the_head_band_is_a_headshot_and_a_bolt_action_needs_a_stand() {
    // The blade at 300 u: the musket's bullet (110 pierce) into mail. Aimed at the body it
    // lands once; aimed at the top of the hull, four times over.
    let hit_for = |pitch_deg: f32| -> i32 {
        let (world, mut zone, a, b) = gun_duel(0.0, 300.0);
        run(
            &mut zone,
            &world,
            &[(a, held(0.0, 0.0, 0, 0)), (b, input(180.0, 0.0, 0))],
            2,
        );
        // Under the scope: the cone is 0.05 degrees, nothing of the shot is luck.
        let aim = Input {
            pitch: pitch_deg,
            ..held(0.0, 0.0, buttons::PRIMARY | buttons::SCOPE, 0)
        };
        tick(&mut zone, &world, &[(a, aim), (b, input(180.0, 0.0, 0))], 0);
        run(
            &mut zone,
            &world,
            &[
                (
                    a,
                    Input {
                        pitch: pitch_deg,
                        ..held(0.0, 0.0, buttons::SCOPE, 0)
                    },
                ),
                (b, input(180.0, 0.0, 0)),
            ],
            6,
        );
        zone.events
            .iter()
            .find_map(|e| match e {
                ZoneEvent::Hit { target, amount, .. } if *target == b => Some(*amount),
                _ => None,
            })
            .unwrap_or(0)
    };
    // The eye is 46 u over the feet, the hull's top at 56 (a striker) and the band from
    // 44 up; the first shot flies where the crosshair is (3.3). Aimed 6 degrees down the
    // bolt enters the body low; aimed level it enters at the eye's height, in the band.
    let body = hit_for(6.0);
    let head = hit_for(0.0);
    assert!(body > 0, "the level shot lands: {body}");
    assert!(head >= body * 3, "the head band: {head} against {body}");
    // Running, a bolt action does not fire; walking, it does.
    let (world, mut zone, a, b) = gun_duel(0.0, 2000.0);
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 1.0, 0, 0)), (b, input(180.0, 0.0, 0))],
        30,
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 1.0, buttons::PRIMARY, 0)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 1.0, 0, 0)), (b, input(180.0, 0.0, 0))],
        2,
    );
    assert_eq!(bolts(&zone), 0, "no shot at a run");
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 0)), (b, input(180.0, 0.0, 0))],
        20,
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::PRIMARY, 0)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 0)), (b, input(180.0, 0.0, 0))],
        2,
    );
    assert_eq!(bolts(&zone), 1, "a shot standing");
}

#[test]
fn the_knife_in_hand_swings_and_a_switch_drops_a_reload() {
    let (world, mut zone, a, b) = gun_duel(0.0, 48.0);
    let knife = zone.player(a).unwrap().sheet.kit.knife.unwrap();
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 2)), (b, input(180.0, 0.0, 0))],
        2,
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::PRIMARY, 2)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 2)), (b, input(180.0, 0.0, 0))],
        1,
    );
    assert_eq!(running(&zone, a), Some(knife));
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 2)), (b, input(180.0, 0.0, 0))],
        14,
    );
    assert_eq!(hits(&zone, HitKind::Melee), 1);
    assert_eq!(bolts(&zone), 0);
    // The pistol, a reload begun, then the knife: the reload is dropped.
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        30,
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::PRIMARY, 1)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        14,
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::RELOAD, 1)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 1)), (b, input(180.0, 0.0, 0))],
        2,
    );
    let p = zone.player(a).unwrap();
    assert!(p.mover.reloading(p.last_input_tick));
    run(
        &mut zone,
        &world,
        &[(a, held(0.0, 0.0, 0, 2)), (b, input(180.0, 0.0, 0))],
        2,
    );
    let p = zone.player(a).unwrap();
    assert!(!p.mover.guns[1].reload_until.is_some());
    assert_eq!(gun(&zone, a, 1).magazine, 7);
}

// ---------- MODES.md: the RPG mode (15c) ----------

fn at(target: u32, input: Input) -> Input {
    Input { target, ..input }
}

/// A crossbow build (range 600 by content) for the aimed-bolt tests: the frostweaver's
/// kit with the bow for the shard, since no preset ships a crossbow (MATRIX.md 15).
fn crossbow_build(pack: &ContentPack) -> Build {
    let mut b = pack.build("frostweaver").unwrap().clone();
    b.primary = pack.find("crossbow").unwrap();
    b.validate(pack).unwrap();
    b
}

#[test]
fn a_target_action_turns_the_body_and_leads_the_bolt_within_range_and_sight() {
    // The crossbow (range 600 by content) faces away from a blade 400 u off
    // that walks across its line; with the blade as the target the bolt is aimed by the
    // zone, led to where the blade will be, and lands.
    let pack = test_content::pack(RATE);
    let world = BoxWorld::floor();
    let mut zone = zone_with(vec![
        (Vec3::new(0.0, 0.0, REST_Z), 180.0),
        (Vec3::new(400.0, -60.0, REST_Z), 90.0),
    ]);
    let shooter = zone.add_player_at(crossbow_build(&pack), 1, Vec3::new(0.0, 0.0, REST_Z), 180.0);
    let runner = zone.add_player_at(
        pack.build("blade").unwrap().clone(),
        2,
        Vec3::new(400.0, -60.0, REST_Z),
        90.0,
    );
    // The runner gets going (90 degrees: along +y, across the line of fire).
    run(
        &mut zone,
        &world,
        &[
            (shooter, input(180.0, 0.0, 0)),
            (runner, input(90.0, 1.0, 0)),
        ],
        20,
    );
    tick(
        &mut zone,
        &world,
        &[
            (shooter, at(runner, input(180.0, 0.0, buttons::PRIMARY))),
            (runner, input(90.0, 1.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[
            (shooter, input(180.0, 0.0, 0)),
            (runner, input(90.0, 1.0, 0)),
        ],
        2,
    );
    let p = zone.player(shooter).unwrap();
    assert!(p.mover.lock_yaw.is_some(), "the body turned to its target");
    assert!(
        p.mover.yaw.abs() < 20.0,
        "faces the runner, not 180: {}",
        p.mover.yaw
    );
    // (The crossbow lets go 150 ms into its script.)
    run(
        &mut zone,
        &world,
        &[
            (shooter, input(180.0, 0.0, 0)),
            (runner, input(90.0, 1.0, 0)),
        ],
        10,
    );
    assert_eq!(bolts(&zone), 1);
    let pr = zone.projectiles().last().expect("the bolt");
    let dir = pr.vel.normalize();
    assert!(
        dir.x > 0.9,
        "flies toward the runner, not where the view looked: {dir:?}"
    );
    assert!(dir.y > 0.0, "led ahead of it: {dir:?}");
    run(
        &mut zone,
        &world,
        &[
            (shooter, input(180.0, 0.0, 0)),
            (runner, input(90.0, 1.0, 0)),
        ],
        40,
    );
    assert_eq!(hits(&zone, HitKind::Projectile), 1, "the led bolt lands");
}

#[test]
fn a_target_out_of_range_or_out_of_sight_is_not_aimed_at() {
    let pack = test_content::pack(RATE);
    // Out of range: 900 u for a crossbow of 600.
    let mut world = BoxWorld::floor();
    let mut zone = zone_with(vec![
        (Vec3::new(0.0, 0.0, REST_Z), 180.0),
        (Vec3::new(900.0, 0.0, REST_Z), 180.0),
    ]);
    let shooter = zone.add_player_at(crossbow_build(&pack), 1, Vec3::new(0.0, 0.0, REST_Z), 180.0);
    let far = zone.add_player_at(
        pack.build("blade").unwrap().clone(),
        2,
        Vec3::new(900.0, 0.0, REST_Z),
        180.0,
    );
    run(
        &mut zone,
        &world,
        &[(shooter, input(180.0, 0.0, 0)), (far, input(180.0, 0.0, 0))],
        2,
    );
    tick(
        &mut zone,
        &world,
        &[
            (shooter, at(far, input(180.0, 0.0, buttons::PRIMARY))),
            (far, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(shooter, input(180.0, 0.0, 0)), (far, input(180.0, 0.0, 0))],
        12,
    );
    let p = zone.player(shooter).unwrap();
    assert!(p.mover.lock_yaw.is_none());
    let dir = zone.projectiles().last().expect("a bolt").vel.normalize();
    assert!(dir.x < -0.9, "flies where the body looks: {dir:?}");
    // Out of sight: a wall between, at 300 u.
    world.push(
        Vec3::new(140.0, -256.0, 0.0),
        Vec3::new(160.0, 256.0, 200.0),
    );
    let mut zone = zone_with(vec![
        (Vec3::new(0.0, 0.0, REST_Z), 180.0),
        (Vec3::new(300.0, 0.0, REST_Z), 180.0),
    ]);
    let shooter = zone.add_player_at(crossbow_build(&pack), 1, Vec3::new(0.0, 0.0, REST_Z), 180.0);
    let hidden = zone.add_player_at(
        pack.build("blade").unwrap().clone(),
        2,
        Vec3::new(300.0, 0.0, REST_Z),
        180.0,
    );
    run(
        &mut zone,
        &world,
        &[
            (shooter, input(180.0, 0.0, 0)),
            (hidden, input(180.0, 0.0, 0)),
        ],
        2,
    );
    tick(
        &mut zone,
        &world,
        &[
            (shooter, at(hidden, input(180.0, 0.0, buttons::PRIMARY))),
            (hidden, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[
            (shooter, input(180.0, 0.0, 0)),
            (hidden, input(180.0, 0.0, 0)),
        ],
        2,
    );
    assert!(
        zone.player(shooter).unwrap().mover.lock_yaw.is_none(),
        "unseen: not turned to"
    );
}

#[test]
fn a_target_action_with_a_melee_arc_swings_at_a_body_behind() {
    let (world, mut zone, a, b) = duel(180.0, 48.0);
    // Facing away, the sword pressed with the other as the target: the body turns and
    // the blow lands.
    run(
        &mut zone,
        &world,
        &[(a, input(180.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        2,
    );
    tick(
        &mut zone,
        &world,
        &[
            (a, at(b, input(180.0, 0.0, buttons::PRIMARY))),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(a, input(180.0, 0.0, 0)), (b, input(180.0, 0.0, 0))],
        16,
    );
    assert_eq!(hits(&zone, HitKind::Melee), 1);
    let p = zone.player(a).unwrap();
    assert!(
        p.mover.yaw.abs() < 1.0 || p.mover.lock_yaw.is_none(),
        "{}",
        p.mover.yaw
    );
}

#[test]
fn an_aimed_area_goes_under_its_target() {
    // The frostweaver's nova is aimed (`Origin::Aim`); with the dummy as the target it is
    // put at the dummy's feet, 400 u off, though the caster looks the other way.
    let pack = test_content::pack(RATE);
    let world = BoxWorld::floor();
    let mut zone = zone_with(vec![
        (Vec3::new(0.0, 0.0, REST_Z), 180.0),
        (Vec3::new(400.0, 0.0, REST_Z), 180.0),
    ]);
    let caster = zone.add_player_at(
        pack.build("frostweaver").unwrap().clone(),
        1,
        Vec3::new(0.0, 0.0, REST_Z),
        180.0,
    );
    let mark = zone.add_player_at(
        pack.build("blade").unwrap().clone(),
        2,
        Vec3::new(400.0, 0.0, REST_Z),
        180.0,
    );
    let kit = &zone.player(caster).unwrap().sheet.kit;
    let nova_slot = kit
        .actives
        .iter()
        .flatten()
        .position(|&k| {
            kit.abilities[k as usize]
                .steps
                .iter()
                .any(|s| matches!(&s.verb, crate::vocab::Verb::AreaEffect(ae) if matches!(ae.origin, crate::vocab::Origin::Aim { .. })))
        })
        .map(|i| i as u8 + 1);
    let Some(slot) = nova_slot else {
        // The fixture's nova is not aimed: nothing to test here.
        return;
    };
    let range = kit.abilities[kit.actives[slot as usize - 1].unwrap() as usize].range;
    assert!(range >= 400.0, "the nova reaches: {range}");
    run(
        &mut zone,
        &world,
        &[(caster, input(180.0, 0.0, 0)), (mark, input(180.0, 0.0, 0))],
        2,
    );
    tick(
        &mut zone,
        &world,
        &[
            (caster, at(mark, active(180.0, 0.0, slot))),
            (mark, input(180.0, 0.0, 0)),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(caster, input(180.0, 0.0, 0)), (mark, input(180.0, 0.0, 0))],
        30,
    );
    let area = zone.areas().iter().chain(zone.areas().iter()).next();
    let placed = zone
        .events
        .iter()
        .any(|e| matches!(e, ZoneEvent::AreaSpawned { .. }));
    assert!(placed, "the nova was cast");
    if let Some(a) = area {
        assert!(
            (a.origin.truncate() - Vec2::new(400.0, 0.0)).length() < 40.0,
            "under the mark: {:?}",
            a.origin
        );
    }
    assert!(hits(&zone, HitKind::Area) >= 1, "it struck the mark");
}

#[test]
fn a_frostweavers_shard_with_a_standing_target_flies_at_it() {
    // The frostweaver faces away (180) from a blade standing 300 u along +x; the shard
    // pressed with the blade as the target leaves toward the blade. (The director saw
    // it fly the camera's way on 2026-10-08: that was the client's tracer, the zone's
    // bolt was led as this says.)
    let pack = test_content::pack(RATE);
    let world = BoxWorld::floor();
    let mut zone = zone_with(vec![
        (Vec3::new(0.0, 0.0, REST_Z), 180.0),
        (Vec3::new(300.0, 0.0, REST_Z), 180.0),
    ]);
    let caster = zone.add_player_at(
        pack.build("frostweaver").unwrap().clone(),
        1,
        Vec3::new(0.0, 0.0, REST_Z),
        180.0,
    );
    let dummy = zone.add_player_at(
        pack.build("blade").unwrap().clone(),
        2,
        Vec3::new(300.0, 0.0, REST_Z),
        180.0,
    );
    let idle = |_: u32| input(180.0, 0.0, 0);
    run(&mut zone, &world, &[(caster, idle(0)), (dummy, idle(0))], 2);
    tick(
        &mut zone,
        &world,
        &[
            (caster, at(dummy, input(180.0, 0.0, buttons::PRIMARY))),
            (dummy, idle(0)),
        ],
        0,
    );
    // The body turns to the dummy for the script; the shard leaves 150 ms into it.
    run(&mut zone, &world, &[(caster, idle(0)), (dummy, idle(0))], 2);
    let p = zone.player(caster).unwrap();
    assert!(p.mover.yaw.abs() < 5.0, "faces the dummy: {}", p.mover.yaw);
    run(
        &mut zone,
        &world,
        &[(caster, idle(0)), (dummy, idle(0))],
        12,
    );
    let dir = zone
        .projectiles()
        .last()
        .expect("the shard")
        .vel
        .normalize();
    assert!(dir.x > 0.95, "flies at the dummy: {dir:?}");
    run(
        &mut zone,
        &world,
        &[(caster, idle(0)), (dummy, idle(0))],
        20,
    );
    assert_eq!(hits(&zone, HitKind::Projectile), 1, "the shard lands");
}

#[test]
fn a_frostweavers_shard_lands_on_a_walking_target_seen_a_round_trip_ago() {
    // The blade walks across the frostweaver's front, 300 u out, at its full pace. The
    // caster's inputs claim a view 8 ticks old, as a client's do (a 6-tick interpolation
    // delay and the half round trip), so the zone spawns the shard at that tick and steps
    // it forward. Until 2026-10-08 the lead was taken from where the target stood *now*
    // while the bolt left from 8 ticks ago: it arrived 8 ticks before the target did, a
    // body's width short of a walker, and the director saw his shards miss anyone who
    // moved (while the client's tracer, led from the same old view, flew true).
    let pack = test_content::pack(RATE);
    let world = BoxWorld::floor();
    let mut zone = zone_with(vec![
        (Vec3::new(0.0, 0.0, REST_Z), 0.0),
        (Vec3::new(300.0, 0.0, REST_Z), 90.0),
    ]);
    let caster = zone.add_player_at(
        pack.build("frostweaver").unwrap().clone(),
        1,
        Vec3::new(0.0, 0.0, REST_Z),
        0.0,
    );
    let walker = zone.add_player_at(
        pack.build("blade").unwrap().clone(),
        2,
        Vec3::new(300.0, 0.0, REST_Z),
        90.0,
    );
    let walk = input(90.0, 1.0, 0);
    let idle = input(0.0, 0.0, 0);
    // The walker gets up to pace.
    run(&mut zone, &world, &[(caster, idle), (walker, walk)], 30);
    let pace = zone.player(walker).unwrap().mover.mv.velocity.length();
    assert!(pace > 200.0, "the blade walks at {pace}");
    let viewed = |zone: &Zone| zone.tick.wrapping_sub(8);
    let v = viewed(&zone);
    tick(
        &mut zone,
        &world,
        &[
            (caster, at(walker, input(0.0, 0.0, buttons::PRIMARY))),
            (walker, walk),
        ],
        v,
    );
    for _ in 0..60 {
        let v = viewed(&zone);
        tick(&mut zone, &world, &[(caster, idle), (walker, walk)], v);
    }
    assert_eq!(
        hits(&zone, HitKind::Projectile),
        1,
        "the shard lands on the walker"
    );
}

/// MATRIX.md 8, 16: a bellow taunts every enemy in earshot and no ally. A taunted body
/// is turned to the roarer and held there whatever its frames say, until the taunt is
/// out; the second within ten seconds lasts half (MODES.md 4.5).
#[test]
fn a_briar_hurts_everyone_in_it_and_feeds_only_its_own_side() {
    // MATRIX.md 17, VOCABULARY.md 5.4: the briar's packets are friendly fire like every
    // packet; its Regen is `target = "allies"`, the shaman's side alone, the shaman too.
    let world = BoxWorld::floor();
    let spawns = vec![
        Spawn {
            origin: Vec3::new(0.0, 0.0, REST_Z),
            yaw: 0.0,
            team: 1,
        },
        Spawn {
            origin: Vec3::new(0.0, 0.0, REST_Z),
            yaw: 0.0,
            team: 2,
        },
    ];
    let mut zone = Zone::new(RATE, 1, spawns, test_content::pack(RATE));
    let shaman = zone.content.build("shaman").unwrap().clone();
    let blade = zone.content.build("blade").unwrap().clone();
    let caster = zone.add_player_at(shaman, 1, Vec3::new(0.0, 0.0, REST_Z), 0.0);
    // The patch lands where the caster looks: pitch 30 down puts it 80 u ahead (the
    // sanctuary's test above); a circle of 140 u holds all three.
    let ally = zone.add_player_at(blade.clone(), 1, Vec3::new(80.0, 60.0, REST_Z), 0.0);
    let enemy = zone.add_player_at(blade, 2, Vec3::new(80.0, -60.0, REST_Z), 0.0);
    let still = input(0.0, 0.0, 0);
    // The patch is placed when the cast's step fires (300 ms in), where the caster looks
    // then: the caster keeps looking down.
    let down = Input {
        pitch: 30.0,
        ..still
    };
    let cast = Input { ability: 1, ..down };
    tick(
        &mut zone,
        &world,
        &[(caster, cast), (ally, still), (enemy, still)],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(caster, down), (ally, still), (enemy, still)],
        40,
    );
    assert_eq!(zone.areas().len(), 1, "the briar stands");
    assert!(
        hits(&zone, HitKind::Area) >= 3,
        "the thorns hurt all three: {} area hits, the patch at {:?}",
        hits(&zone, HitKind::Area),
        zone.areas()[0].origin
    );
    let has = |id: EntityId| zone.player(id).unwrap().mover.statuses.has(Status::Regen);
    assert!(has(ally), "the ally in the patch has Regen");
    assert!(has(caster), "the shaman in its own patch too");
    assert!(!has(enemy), "the enemy in it has none");
}

#[test]
fn a_bellow_turns_enemies_to_the_roarer_and_not_allies() {
    let world = BoxWorld::floor();
    let spawns = vec![
        Spawn {
            origin: Vec3::new(0.0, 0.0, REST_Z),
            yaw: 0.0,
            team: 1,
        },
        Spawn {
            origin: Vec3::new(0.0, 0.0, REST_Z),
            yaw: 0.0,
            team: 2,
        },
    ];
    let mut zone = Zone::new(RATE, 1, spawns, test_content::pack(RATE));
    let mut ironclad = zone.content.build("ironclad").unwrap().clone();
    ironclad.actives[1] = zone.content.find("bellow").unwrap();
    let blade = zone.content.build("blade").unwrap().clone();
    let roarer = zone.add_player_at(ironclad, 1, Vec3::new(0.0, 0.0, REST_Z), 0.0);
    let ally = zone.add_player_at(blade.clone(), 1, Vec3::new(0.0, 120.0, REST_Z), 0.0);
    let enemy = zone.add_player_at(blade.clone(), 2, Vec3::new(200.0, 0.0, REST_Z), 0.0);
    let far = zone.add_player_at(blade, 2, Vec3::new(400.0, 0.0, REST_Z), 0.0);
    // Everybody looks east (yaw 0); the roarer bellows (active slot 2).
    let east = input(0.0, 0.0, 0);
    tick(
        &mut zone,
        &world,
        &[
            (roarer, active(0.0, 0.0, 2)),
            (ally, east),
            (enemy, east),
            (far, east),
        ],
        0,
    );
    run(
        &mut zone,
        &world,
        &[(roarer, east), (ally, east), (enemy, east), (far, east)],
        30,
    );
    let e = zone.player(enemy).unwrap();
    assert!(
        e.mover.statuses.has(Status::Taunt),
        "the enemy in earshot is taunted: roarer {:?} {:?} areas {}",
        zone.player(roarer).unwrap().mover.script,
        zone.player(roarer)
            .unwrap()
            .mover
            .statuses
            .active()
            .collect::<Vec<_>>(),
        zone.areas().len()
    );
    assert_eq!(e.mover.statuses.taunted_by(), Some(roarer));
    // Its frames say east; its body faces west, to the roarer.
    assert!(
        (e.mover.yaw - 180.0).abs() < 1.0,
        "turned to the roarer: yaw {}",
        e.mover.yaw
    );
    assert!(e.mover.lock_yaw.is_some());
    assert!(
        !zone.player(ally).unwrap().mover.statuses.has(Status::Taunt),
        "an ally is not"
    );
    assert!(
        !zone.player(far).unwrap().mover.statuses.has(Status::Taunt),
        "nor a body out of earshot"
    );
    assert!(
        zone.player(roarer)
            .unwrap()
            .mover
            .statuses
            .has(Status::Fortify),
        "the roarer braces"
    );
    // When the taunt is out (2.5 s by the blade's duration factor), the frames' yaw
    // holds again.
    let left = tick_delta(
        e.mover.statuses.get(Status::Taunt).unwrap().until,
        e.last_input_tick,
    )
    .max(0) as usize;
    run(
        &mut zone,
        &world,
        &[(roarer, east), (ally, east), (enemy, east), (far, east)],
        left + 3,
    );
    let e = zone.player(enemy).unwrap();
    assert!(!e.mover.statuses.has(Status::Taunt));
    assert!(e.mover.yaw.abs() < 1.0, "free again: yaw {}", e.mover.yaw);
    // The second taunt within ten seconds lasts half (MODES.md 4.5).
    let s = zone.content.abilities[zone.content.find("bellow").unwrap() as usize]
        .ability
        .steps[0]
        .clone();
    let crate::vocab::Verb::AreaEffect(a) = &s.verb else {
        panic!("the roar is an area")
    };
    let full = a.effects[0].duration;
    zone.apply_status(enemy, roarer, &a.effects[0]);
    let e = zone.player(enemy).unwrap();
    let left = tick_delta(
        e.mover.statuses.get(Status::Taunt).unwrap().until,
        e.last_input_tick,
    );
    let scaled = (full as f32 * e.sheet.derived.status_duration).round() as i32;
    assert!(
        (left - scaled / 2).abs() <= 1,
        "half: {left} of {scaled} ({full} by the verb)"
    );
}

/// The kit on the item bar (MODES.md 11.3, LOOK.md 3.2): a press of `USE` naming its cell,
/// on the ground with a kit carried and the hands free, begins a use of 1,500 ms that ends
/// with one kit fewer, 50 healed and `ItemUsed` said; at full health the zone clears it the
/// same tick; in the air, with the hands busy or with an empty cell it begins nothing, and
/// `item_refusal` says which; a refused press is not kept; a cell is used by its own key.
#[test]
fn a_kit_heals_when_pressed_with_free_hands_and_a_refused_press_says_why() {
    let (world, mut zone, a, b) = gun_duel(0.0, 400.0);
    let quiet = [(a, held(0.0, 0.0, 0, 0)), (b, input(180.0, 0.0, 0))];
    // The kit sits on the first cell (`F`): a `USE` names it, or names no cell at all.
    let use_cell = |slot: u8, h: u8| Input {
        use_slot: slot,
        ..held(0.0, 0.0, buttons::USE, h)
    };
    let press = [(a, use_cell(1, 0)), (b, input(180.0, 0.0, 0))];
    let press_b = [(a, held(0.0, 0.0, 0, 0)), (b, use_cell(0, 0))];
    run(&mut zone, &world, &quiet, 5);
    let a_mover = |zone: &Zone| zone.player(a).unwrap().mover;
    let refusal = |zone: &Zone, id: EntityId, cell: usize| {
        let p = zone.player(id).unwrap();
        item_refusal(
            &p.mover,
            cell,
            p.last_input_tick,
            p.mover.statuses.staggered(),
        )
    };
    assert_eq!(a_mover(&zone).bar, [2, 0, 0, 0]);
    assert_eq!(refusal(&zone, a, 0), None, "two kits, on the ground, idle");
    assert_eq!(
        refusal(&zone, a, 1),
        Some(ItemRefusal::Empty),
        "nothing on the second cell"
    );
    assert_eq!(refusal(&zone, b, 0), Some(ItemRefusal::Empty));
    assert_eq!(ItemRefusal::Empty.word(), "nothing to use");
    assert_eq!(
        (bar_cell(0), bar_cell(1), bar_cell(4), bar_cell(5)),
        (Some(0), Some(0), Some(3), None)
    );

    // At full health: the mover begins it and the zone clears it the same tick.
    tick(&mut zone, &world, &press, 0);
    run(&mut zone, &world, &quiet, 1);
    let p = zone.player(a).unwrap();
    assert!(
        p.mover.use_until.is_none() && p.mover.bar[0] == 2,
        "at full health nothing is begun"
    );
    run(&mut zone, &world, &quiet, 2);
    // An empty cell: nothing, whatever the health.
    zone.player_mut(b).unwrap().health -= 300;
    tick(&mut zone, &world, &press_b, 0);
    run(&mut zone, &world, &quiet, 1);
    assert!(zone.player(b).unwrap().mover.use_until.is_none());
    run(&mut zone, &world, &quiet, 2);

    // Hurt: the press begins a use; 1,500 ms later one kit is gone, 50 is healed, and
    // the zone said so.
    zone.player_mut(a).unwrap().health -= 300;
    let before = zone.player(a).unwrap().health;
    tick(&mut zone, &world, &press, 0);
    run(&mut zone, &world, &quiet, 1);
    let p = zone.player(a).unwrap();
    assert_eq!(
        p.mover.using_item(p.last_input_tick),
        Some(0),
        "the use began"
    );
    assert_eq!(p.anim, anim::USE);
    let ticks = kit_use_ticks(RATE.dt()) as usize;
    assert_eq!(ticks, RATE.ms_to_ticks(KIT_USE_MS) as usize);
    run(&mut zone, &world, &quiet, ticks - 3);
    let p = zone.player(a).unwrap();
    assert!(
        p.mover.using_item(p.last_input_tick).is_some(),
        "still at it"
    );
    assert_eq!(
        refusal(&zone, a, 0),
        Some(ItemRefusal::HandsBusy),
        "a second press during the use"
    );
    assert_eq!(p.health, before, "the heal comes at the end");
    run(&mut zone, &world, &quiet, 4);
    let p = zone.player(a).unwrap();
    assert!(p.mover.using_item(p.last_input_tick).is_none());
    assert_eq!((p.mover.bar[0], p.health), (1, before + 50));
    assert_eq!(
        zone.events
            .iter()
            .filter(|e| matches!(e, ZoneEvent::ItemUsed { id, cell: 0 } if *id == a))
            .count(),
        1
    );
    assert!(zone.events.iter().any(|e| matches!(
        e,
        ZoneEvent::Healed { target, source, amount: 50 } if *target == a && *source == a
    )));

    // Hands busy: the pistol fires one and reloads on `R`; a press during the reload
    // begins nothing and is not kept for when the hands are free.
    let pistol = |b_: u16| [(a, held(0.0, 0.0, b_, 1)), (b, input(180.0, 0.0, 0))];
    tick(&mut zone, &world, &pistol(0), 0);
    tick(&mut zone, &world, &pistol(buttons::PRIMARY), 0);
    tick(&mut zone, &world, &pistol(0), 0);
    tick(&mut zone, &world, &pistol(buttons::RELOAD), 0);
    tick(
        &mut zone,
        &world,
        &[(a, use_cell(1, 1)), (b, input(180.0, 0.0, 0))],
        0,
    );
    run(&mut zone, &world, &pistol(0), 1);
    let p = zone.player(a).unwrap();
    assert!(p.mover.reloading(p.last_input_tick), "the pistol reloads");
    assert_eq!(refusal(&zone, a, 0), Some(ItemRefusal::HandsBusy));
    assert_eq!(ItemRefusal::HandsBusy.word(), "hands busy");
    assert!(
        p.mover.use_until.is_none(),
        "nothing begun with the hands busy"
    );
    run(&mut zone, &world, &pistol(0), 200);
    let p = zone.player(a).unwrap();
    assert!(
        !p.mover.reloading(p.last_input_tick) && p.mover.script.is_none(),
        "the hands are free again"
    );
    assert!(
        p.mover.use_until.is_none(),
        "the refused press was not kept"
    );
    assert_eq!(p.mover.bar[0], 1);
    run(&mut zone, &world, &quiet, 2);

    // In the air: a jump, then the press.
    tick(
        &mut zone,
        &world,
        &[
            (a, held(0.0, 0.0, buttons::JUMP, 0)),
            (b, input(180.0, 0.0, 0)),
        ],
        0,
    );
    tick(&mut zone, &world, &press, 0);
    run(&mut zone, &world, &quiet, 1);
    let p = zone.player(a).unwrap();
    assert!(!p.mover.mv.on_ground, "in the air");
    assert_eq!(refusal(&zone, a, 0), Some(ItemRefusal::InTheAir));
    assert_eq!(ItemRefusal::InTheAir.word(), "not in the air");
    assert!(p.mover.use_until.is_none());
    run(&mut zone, &world, &quiet, 40);
    assert!(zone.player(a).unwrap().mover.mv.on_ground);
    assert_eq!(a_mover(&zone).bar[0], 1, "nothing was used in the air");

    // In a town at 20 Hz the use is the same second and a half, not 96 of its ticks.
    let (world, mut zone, a, b) = gun_duel_at(TickRate::TOWN, 0.0, 400.0);
    assert_eq!(kit_use_ticks(TickRate::TOWN.dt()), 30);
    run(&mut zone, &world, &quiet, 5);
    zone.player_mut(a).unwrap().health -= 300;
    let before = zone.player(a).unwrap().health;
    tick(&mut zone, &world, &press, 0);
    run(&mut zone, &world, &quiet, 1);
    let p = zone.player(a).unwrap();
    assert!(
        p.mover.using_item(p.last_input_tick).is_some(),
        "the use began at 20 Hz"
    );
    run(&mut zone, &world, &quiet, 27);
    let p = zone.player(a).unwrap();
    assert!(
        p.mover.using_item(p.last_input_tick).is_some(),
        "still at it after 1.35 s"
    );
    run(&mut zone, &world, &quiet, 4);
    let p = zone.player(a).unwrap();
    assert!(
        p.mover.using_item(p.last_input_tick).is_none(),
        "done after 1.5 s"
    );
    assert_eq!((p.mover.bar[0], p.health), (1, before + 50));

    // The kit moved to the third cell (the bar is the player's, LOOK.md 3.2): `F` finds
    // nothing, the cell's key uses it; the use ends with `used` for the zone, which heals
    // by the cell.
    zone.set_stacks(
        a,
        &[("kit".to_string(), 1, Some(50))],
        &[None, None, Some("kit".to_string()), None],
    );
    assert_eq!(a_mover(&zone).bar, [0, 0, 1, 0]);
    zone.player_mut(a).unwrap().health -= 300;
    let before = zone.player(a).unwrap().health;
    tick(&mut zone, &world, &press, 0);
    run(&mut zone, &world, &quiet, 1);
    assert!(
        zone.player(a).unwrap().mover.use_until.is_none(),
        "F: nothing to use"
    );
    tick(
        &mut zone,
        &world,
        &[(a, use_cell(3, 0)), (b, input(180.0, 0.0, 0))],
        0,
    );
    run(&mut zone, &world, &quiet, 1);
    let p = zone.player(a).unwrap();
    assert_eq!(p.mover.using_item(p.last_input_tick), Some(2));
    assert!(
        p.mover
            .use_progress(p.last_input_tick, TickRate::TOWN.dt())
            .is_some_and(|f| f < 0.2)
    );
    run(&mut zone, &world, &quiet, 32);
    let p = zone.player(a).unwrap();
    assert_eq!((p.mover.bar, p.health), ([0, 0, 0, 0], before + 50));
    assert!(
        zone.events
            .iter()
            .any(|e| matches!(e, ZoneEvent::ItemUsed { id, cell: 2 } if *id == a))
    );
}
