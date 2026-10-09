//! Phase 3 acceptance (PLAN.md 11.8, MATRIX.md 11): a build that dominates can be countered
//! by re-speccing. Eight against eight in the arena map, identical duelist brains on both
//! sides, driven straight through `gm_core::sim::Zone` (no network, no interpolation delay,
//! so neither side has a latency edge). Kills per team decide.

use std::path::Path;
use std::sync::Arc;

use glam::Vec3;
use gm_bot::{Behaviour, Brain, View};
use gm_bsp::Bsp;
use gm_core::sim::{Input, Spawn, Zone, ZoneEvent};
use gm_core::tick::TickRate;
use gm_core::vocab::EntityId;
use gm_net::client::RenderEntity;
use gm_net::snapshot::{SpawnInfo, flags};

const MAP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/maps/built/arena.bsp"
);
const CONTENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/content");
const RATE: TickRate = TickRate::COMBAT;

struct Outcome {
    kills: [u32; 3],
    deaths: [u32; 3],
}

fn spawns(bsp: &Bsp) -> Vec<Spawn> {
    bsp.entities
        .iter()
        .filter(|e| e.classname() == "gm_spawn")
        .filter_map(|e| {
            Some(Spawn {
                origin: e.origin()?,
                yaw: e.f32("angle").unwrap_or(0.0),
                team: e.f32("team").unwrap_or(0.0) as u8,
            })
        })
        .collect()
}

/// Play `secs` seconds of team 1 (`build_a`) against team 2 (`build_b`), 8 v 8.
fn play(bsp: &Arc<Bsp>, build_a: &str, build_b: &str, seed: u64, secs: u32) -> Outcome {
    let content = std::env::var("ARENA_CONTENT").unwrap_or_else(|_| CONTENT.to_string());
    let pack = gm_content::load_dir(Path::new(&content), RATE).expect("content");
    let mut zone = Zone::new(RATE, seed, spawns(bsp), pack.clone());
    let ammo: Vec<(String, u32, Option<i32>)> = pack
        .abilities
        .iter()
        .filter_map(|a| a.ability.firearm.as_ref())
        .filter(|f| !f.ammo.is_empty())
        .map(|f| (f.ammo.clone(), 500, None))
        .collect();
    let mut bots: Vec<(EntityId, Brain, u8)> = Vec::new();
    for i in 0..16 {
        let (team, name) = if i % 2 == 0 {
            (1u8, build_a)
        } else {
            (2u8, build_b)
        };
        let build = pack.build(name).expect(name).clone();
        let id = zone
            .add_player(bsp.as_ref(), build, team)
            .expect("preset validates");
        // A gun is issued with its magazine only; the rounds a body carries are stacks
        // the hub tells the zone of (MODES.md 11). Here every bot carries plenty of each.
        zone.set_stacks(id, &ammo, &[]);
        bots.push((
            id,
            Brain::new(seed * 100 + i as u64, Behaviour::Duelist),
            team,
        ));
    }
    let ticks = secs * RATE.hz();
    let mut kills = [0u32; 3];
    let mut deaths = [0u32; 3];
    for t in 1..=ticks {
        // Everyone sees everyone at this tick (the arena has no PVS edge for either side).
        let others: Vec<RenderEntity> = zone
            .players()
            .map(|p| RenderEntity {
                id: p.id,
                kind: gm_net::snapshot::EntityKind::Player,
                spawn: SpawnInfo::Player {
                    frame: 0,
                    team: p.team(),
                    aspects: p.sheet.build.aspects.0,
                    armour: p.sheet.build.armour as u8,
                },
                pos: p.mover.mv.origin,
                yaw: p.mover.yaw,
                pitch: p.mover.pitch,
                anim: p.anim,
                acting: 0,
                flags: if p.alive { flags::ALIVE } else { 0 },
                status: p.mover.statuses.mask(),
                health: None,
                vel: p.mover.mv.velocity,
            })
            .collect();
        let inputs: Vec<(EntityId, Input)> = bots
            .iter_mut()
            .map(|(id, brain, team)| {
                let p = zone.player(*id).expect("player");
                let visible: Vec<RenderEntity> =
                    others.iter().filter(|e| e.id != *id).copied().collect();
                let input = brain.think(&View {
                    me: &p.mover,
                    kit: &p.sheet.kit,
                    frame: p.sheet.build.frame,
                    team: *team,
                    alive: p.alive,
                    health: p.health,
                    others: &visible,
                    tick: t,
                });
                (*id, input)
            })
            .collect();
        for (id, input) in inputs {
            zone.queue_input(id, t, input, 0);
        }
        zone.step(bsp.as_ref());
        let events: Vec<ZoneEvent> = zone.events.drain(..).collect();
        for ev in events {
            if let ZoneEvent::Killed { victim, killer } = ev {
                let kt = zone.player(killer).map_or(0, |p| p.team()) as usize;
                let vt = zone.player(victim).map_or(0, |p| p.team()) as usize;
                if kt != vt {
                    kills[kt] += 1;
                }
                deaths[vt] += 1;
            }
        }
    }
    Outcome { kills, deaths }
}

fn ratio(o: &Outcome) -> f64 {
    o.kills[1] as f64 / o.kills[2].max(1) as f64
}

#[test]
fn ironclad_beats_blade_and_frostweaver_beats_ironclad() {
    let bsp = Arc::new(Bsp::load(Path::new(MAP)).expect("arena built"));
    let secs = 60;
    let seeds = [1u64, 2, 3];
    let mut a_total = [0u32; 3];
    let mut b_total = [0u32; 3];
    let mut c_total = [0u32; 3];
    for &seed in &seeds {
        // The cycle closes: the frostweaver's Water is halved by the blade's Water (MATRIX.md 5).
        let c = play(&bsp, "blade", "frostweaver", seed, secs);
        println!(
            "seed {seed}: blade {} : {} frostweaver (deaths {:?}) ratio {:.2}",
            c.kills[1],
            c.kills[2],
            &c.deaths[1..],
            ratio(&c)
        );
        for (t, k) in c_total.iter_mut().zip(c.kills) {
            *t += k;
        }
        let a = play(&bsp, "ironclad", "blade", seed, secs);
        println!(
            "seed {seed}: ironclad {} : {} blade (deaths {:?}) ratio {:.2}",
            a.kills[1],
            a.kills[2],
            &a.deaths[1..],
            ratio(&a)
        );
        let b = play(&bsp, "ironclad", "frostweaver", seed, secs);
        println!(
            "seed {seed}: ironclad {} : {} frostweaver (deaths {:?}) ratio {:.2}",
            b.kills[1],
            b.kills[2],
            &b.deaths[1..],
            ratio(&b)
        );
        for ((ta, tb), (ka, kb)) in a_total
            .iter_mut()
            .zip(b_total.iter_mut())
            .zip(a.kills.into_iter().zip(b.kills))
        {
            *ta += ka;
            *tb += kb;
        }
    }
    let a_ratio = a_total[1] as f64 / a_total[2].max(1) as f64;
    let b_ratio = b_total[2] as f64 / b_total[1].max(1) as f64;
    let c_ratio = c_total[1] as f64 / c_total[2].max(1) as f64;
    println!(
        "totals over {} seeds x {secs} s: ironclad:blade {}:{} ({a_ratio:.2}); frostweaver:ironclad {}:{} ({b_ratio:.2}); blade:frostweaver {}:{} ({c_ratio:.2})",
        seeds.len(),
        a_total[1],
        a_total[2],
        b_total[2],
        b_total[1],
        c_total[1],
        c_total[2]
    );
    assert!(
        c_ratio >= 1.3,
        "blade does not close the cycle against frostweaver: {}:{}",
        c_total[1],
        c_total[2]
    );
    assert!(
        a_total[1] + a_total[2] >= 30,
        "too few kills to decide: {a_total:?}"
    );
    assert!(
        a_ratio >= 1.3,
        "ironclad does not dominate blade: {}:{}",
        a_total[1],
        a_total[2]
    );
    assert!(
        b_ratio >= 1.3,
        "frostweaver does not counter ironclad: {}:{}",
        b_total[2],
        b_total[1]
    );
}

#[test]
fn mirror_match_is_even() {
    // Same build both sides: the map and the brains favour neither team (within noise).
    let bsp = Arc::new(Bsp::load(Path::new(MAP)).expect("arena built"));
    let mut total = [0u32; 3];
    for seed in [11u64, 12, 13, 14] {
        let o = play(&bsp, "blade", "blade", seed, 45);
        println!("seed {seed}: blade {} : {} blade", o.kills[1], o.kills[2]);
        for (t, k) in total.iter_mut().zip(o.kills) {
            *t += k;
        }
    }
    let r = total[1] as f64 / total[2].max(1) as f64;
    println!("mirror totals {}:{} ratio {r:.2}", total[1], total[2]);
    assert!(total[1] + total[2] >= 30, "too few kills: {total:?}");
    assert!(
        (0.7..=1.43).contains(&r),
        "the map or the brains are lopsided: {r:.2}"
    );
    let _ = Vec3::ZERO;
}

/// Every preset against every other, both sides, so the table is free of the team-1 edge
/// (`ARENA_SEEDS` seeds × `ARENA_SECS` s each, default 2 × 60). Prints the kill table and
/// the share of kills each build took against each other; run on demand:
/// `ARENA_ROUND_ROBIN=1 cargo test -p gm-bot --release --test arena round_robin -- --ignored --nocapture`.
#[test]
#[ignore]
fn round_robin() {
    let bsp = Arc::new(Bsp::load(Path::new(MAP)).expect("arena built"));
    let pack = gm_content::load_dir(Path::new(CONTENT), RATE).expect("content");
    let mut names: Vec<String> = pack.builds.iter().map(|b| b.name.clone()).collect();
    names.sort();
    let seeds: u64 = std::env::var("ARENA_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let secs: u32 = std::env::var("ARENA_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let n = names.len();
    // kills[a][b] = kills build a scored against build b, summed over both orientations.
    let mut kills = vec![vec![0u32; n]; n];
    for a in 0..n {
        for b in a..n {
            for seed in 1..=seeds {
                let o = play(&bsp, &names[a], &names[b], seed, secs);
                kills[a][b] += o.kills[1];
                kills[b][a] += o.kills[2];
                if a != b {
                    let o = play(&bsp, &names[b], &names[a], seed, secs);
                    kills[b][a] += o.kills[1];
                    kills[a][b] += o.kills[2];
                }
            }
            eprintln!(
                "{} v {}: {} : {}",
                names[a], names[b], kills[a][b], kills[b][a]
            );
        }
    }
    println!("\nkills (row scored on column), {seeds} seeds x {secs} s, both sides:");
    print!("| build |");
    for b in &names {
        print!(" {b} |");
    }
    println!(" total |");
    println!("|---|{}---|", "---|".repeat(n));
    for a in 0..n {
        print!("| {} |", names[a]);
        let mut for_ = 0;
        let mut against = 0;
        for (b, row) in kills.iter().enumerate() {
            print!(" {}:{} |", kills[a][b], row[a]);
            if a != b {
                for_ += kills[a][b];
                against += row[a];
            }
        }
        println!(" {for_}:{against} |");
    }
    println!("\nshare of kills (row's kills / all kills in the matchup):");
    print!("| build |");
    for b in &names {
        print!(" {b} |");
    }
    println!(" mean |");
    println!("|---|{}---|", "---|".repeat(n));
    for a in 0..n {
        print!("| {} |", names[a]);
        let mut sum = 0.0;
        for (b, row) in kills.iter().enumerate() {
            let tot = (kills[a][b] + row[a]).max(1) as f64;
            let share = kills[a][b] as f64 / tot;
            if a == b {
                print!(" - |");
            } else {
                print!(" {:.0}% |", share * 100.0);
                sum += share;
            }
        }
        println!(" {:.0}% |", sum / (n - 1) as f64 * 100.0);
    }
}
