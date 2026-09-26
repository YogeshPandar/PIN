// warm kernel benchmark; includes directory open in each independent seek.
use pin_core::codec::bytes::Reader;
use pin_core::codec::position_blocks::{self, PositionBlocks};
use pin_core::codec::positions::{self, Positions};
use std::hint::black_box;
use std::time::Instant;

fn measure(mut f: impl FnMut() -> Option<u32>, repetitions: usize) -> u128 {
    let start = Instant::now();
    for _ in 0..repetitions {
        black_box(f());
    }
    start.elapsed().as_nanos() / repetitions as u128
}

fn main() {
    println!(
        "count,target,round,repetitions,legacy_bytes,block_bytes,lazy_linear_ns,validated_linear_ns,open_seek_ns,reused_seek_ns,decoded_positions,decoded_bytes"
    );
    for (count, repetitions) in [(32, 10000), (10000, 1000), (1000000, 10)] {
        let values: Vec<u32> = (0..count).map(|i| i * 2).collect();
        let mut legacy = vec![0; count as usize * 5 + 4];
        let n = positions::encode(&values, &mut legacy, count).unwrap();
        legacy.truncate(n);
        let mut blocked = vec![0; count as usize * 21 + 8];
        let n = position_blocks::encode(&values, &mut blocked, count).unwrap();
        blocked.truncate(n);
        let view = PositionBlocks::open(&blocked, count).unwrap();
        view.validate_all().unwrap();
        for target in [0, count, count * 2 - 3, count * 2] {
            let expected = values.iter().copied().find(|&n| n >= target);
            assert_eq!(view.seek_ge(target).unwrap().position, expected);
            for round in 0..6 {
                let lazy = || {
                    let mut reader = Reader::new(black_box(&legacy));
                    let n = reader.u32().unwrap();
                    let mut value = 0u32;
                    for _ in 0..n {
                        value += reader.var_u32().unwrap();
                        if value >= black_box(target) {
                            return Some(value);
                        }
                    }
                    None
                };
                let validated = || {
                    Positions::parse(black_box(&legacy), count)
                        .unwrap()
                        .iter()
                        .map(Result::unwrap)
                        .find(|&n| n >= black_box(target))
                };
                let seek = || {
                    PositionBlocks::open(black_box(&blocked), count)
                        .unwrap()
                        .seek_ge(black_box(target))
                        .unwrap()
                        .position
                };
                let reused = || black_box(view).seek_ge(black_box(target)).unwrap().position;
                assert_eq!(lazy(), expected);
                assert_eq!(validated(), expected);
                let (a, b, c, d) = if round % 2 == 0 {
                    (
                        measure(lazy, repetitions),
                        measure(validated, repetitions),
                        measure(seek, repetitions),
                        measure(reused, repetitions),
                    )
                } else {
                    let d = measure(reused, repetitions);
                    let c = measure(seek, repetitions);
                    let b = measure(validated, repetitions);
                    let a = measure(lazy, repetitions);
                    (a, b, c, d)
                };
                let work = view.seek_ge(target).unwrap();
                println!(
                    "{count},{target},{round},{repetitions},{},{},{a},{b},{c},{d},{},{}",
                    legacy.len(),
                    blocked.len(),
                    work.decoded_positions,
                    work.decoded_bytes
                );
            }
        }
    }
}
