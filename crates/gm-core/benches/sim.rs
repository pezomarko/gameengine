use criterion::{Criterion, criterion_group, criterion_main};
use glam::Vec3;
use gm_core::collide::BoxWorld;
use gm_core::movement::{MoveInput, MoveVars, PlayerState, player_move};
use gm_core::sim::{Input, Zone, buttons, test_content};
use gm_core::tick::TickRate;
use std::hint::black_box;

fn bench(c: &mut Criterion) {
    let world = BoxWorld::floor();
    let dt = TickRate::COMBAT.dt();
    c.bench_function("player_move_1000_ticks_box_world", |b| {
        b.iter(|| {
            let mut st = PlayerState::new(Vec3::new(0.0, 0.0, 24.0));
            let input = MoveInput {
                yaw: 30.0,
                forward: 1.0,
                side: 0.3,
                jump: false,
            };
            for _ in 0..1000 {
                player_move(&world, &MoveVars::QUAKE, &mut st, &input, dt);
            }
            black_box(st)
        })
    });

    // 16 players running around and swinging: one server tick of the whole zone.
    let pack = test_content::pack(TickRate::COMBAT);
    let build = test_content::phase2_build(&pack);
    let mut zone = Zone::new(TickRate::COMBAT, 1, vec![], pack);
    let ids: Vec<u32> = (0..16)
        .map(|i| {
            let a = i as f32 / 16.0 * std::f32::consts::TAU;
            zone.add_player_at(
                build.clone(),
                0,
                Vec3::new(a.cos() * 200.0, a.sin() * 200.0, 24.0),
                a.to_degrees() + 180.0,
            )
        })
        .collect();
    let mut t = 0u32;
    c.bench_function("zone_step_16_players", |b| {
        b.iter(|| {
            t += 1;
            for (i, &id) in ids.iter().enumerate() {
                let input = Input {
                    buttons: if (t + i as u32).is_multiple_of(24) {
                        buttons::PRIMARY
                    } else {
                        0
                    },
                    yaw: (t as f32 * 3.0 + i as f32 * 22.5) % 360.0,
                    pitch: 0.0,
                    forward: 1.0,
                    side: 0.0,
                    ability: 0,
                    held: 0,
                    target: 0,
                    use_slot: 0,
                };
                zone.queue_input(id, t, input, 0);
            }
            zone.step(&world);
            zone.events.clear();
            black_box(zone.tick)
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
