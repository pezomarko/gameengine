use criterion::{Criterion, criterion_group, criterion_main};
use gm_net::input::{InputDatagram, InputFrame, buttons};
use gm_net::snapshot::{EntityState, Snapshot, SpawnInfo, flags};
use std::hint::black_box;

fn scene(tick: u32, shift: i32) -> Snapshot {
    let mut s = Snapshot::new(tick);
    for i in 1..17u32 {
        s.entities.push(EntityState {
            id: i,
            spawn: SpawnInfo::Player {
                frame: 1,
                team: 1,
                aspects: 1,
                armour: 2,
            },
            pos: [i as i32 * 100 + shift, -400 + shift / 2, 96],
            yaw: (1234 + i as u16 * 7) % 3600,
            pitch: 900,
            vel: (i == 1).then_some([1000, 0, -40]),
            anim: 2,
            acting: 0,
            health: (i == 1).then_some(100),
            flags: flags::ALIVE | flags::ON_GROUND,
            status: 0,
        });
    }
    s
}

fn bench(c: &mut Criterion) {
    let base = scene(10, 0);
    let mut next = scene(11, 20);
    next.baseline_tick = 10;
    let bytes = next.encode(Some(&base));
    c.bench_function("snapshot_encode_delta_16", |b| {
        b.iter(|| black_box(next.encode(Some(&base))))
    });
    c.bench_function("snapshot_decode_delta_16", |b| {
        b.iter(|| black_box(Snapshot::decode(&bytes, |_| Some(&base)).unwrap()))
    });
    let mut d = InputDatagram::new(100, 96, 500);
    for i in 0..3 {
        d.push(InputFrame {
            buttons: buttons::JUMP,
            yaw: 100 + i,
            pitch: 900,
            forward: 127,
            side: 0,
            ability: 0,
            held: 0,
            target: 0,
            use_slot: 0,
        });
    }
    let ib = d.encode();
    c.bench_function("input_encode_3", |b| b.iter(|| black_box(d.encode())));
    c.bench_function("input_decode_3", |b| {
        b.iter(|| black_box(InputDatagram::decode(&ib).unwrap()))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
