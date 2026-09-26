use pin_core::codec::position_blocks::{self, PositionBlocks};

fn encoded(values: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0; 8 + values.len() * 21];
    let len = position_blocks::encode(values, &mut bytes, u32::MAX).unwrap();
    bytes.truncate(len);
    bytes
}

#[test]
fn seek_matches_independent_sorted_vector_oracle() {
    let mut seed = 71u64;
    for count in [0, 1, 2, 127, 128, 129, 255, 256, 257, 10000] {
        let mut values = Vec::new();
        let mut value = 0;
        for _ in 0..count {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            value += 1 + ((seed >> 32) as u32 % 1000);
            values.push(value);
        }
        let bytes = encoded(&values);
        let view = PositionBlocks::open(&bytes, count).unwrap();
        assert_eq!(view.len(), count);
        view.validate_all().unwrap();
        for target in values
            .iter()
            .flat_map(|&n| [n - 1, n, n + 1])
            .chain([0, u32::MAX])
        {
            let actual = view.seek_ge(target).unwrap();
            let expected = values.iter().copied().find(|&n| n >= target);
            assert_eq!(actual.position, expected, "count={count} target={target}");
            assert!(actual.decoded_positions <= 128);
            assert!(actual.decoded_bytes <= 635);
        }
    }
}

#[test]
fn extremes_empty_and_output_errors() {
    for values in [vec![], vec![0], vec![u32::MAX], vec![0, u32::MAX]] {
        let bytes = encoded(&values);
        let view = PositionBlocks::open(&bytes, 2).unwrap();
        view.validate_all().unwrap();
        for target in [0, 1, u32::MAX] {
            assert_eq!(
                view.seek_ge(target).unwrap().position,
                values.iter().copied().find(|&n| n >= target)
            );
        }
        for prefix in 0..bytes.len() {
            assert!(PositionBlocks::open(&bytes[..prefix], 2).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(PositionBlocks::open(&trailing, 2).is_err());
    }
    let mut output = [0xaa; 32];
    for values in [&[1, 1][..], &[2, 1], &[1, 2, 3]] {
        assert!(position_blocks::encode(values, &mut output, 2).is_err());
        assert_eq!(output, [0xaa; 32]);
    }
    assert!(position_blocks::encode(&[1], &mut output[..8], 2).is_err());
    assert_eq!(output, [0xaa; 32]);
    assert!(PositionBlocks::open(&encoded(&[1, 2]), 1).is_err());
}

#[test]
fn consumed_block_corruption_is_rejected_skipped_blocks_require_full_validation() {
    let mut bytes = encoded(&(0..256).collect::<Vec<_>>());
    // first delta of block two: duplicate position; directory still well formed.
    bytes[8 + 2 * 16 + 127] = 0;
    let view = PositionBlocks::open(&bytes, 256).unwrap();
    assert_eq!(view.seek_ge(0).unwrap().position, Some(0));
    assert!(view.seek_ge(128).is_err());
    assert!(view.validate_all().is_err());
    // past-end seeks read no payload and do not certify skipped bytes.
    assert_eq!(view.seek_ge(256).unwrap().decoded_positions, 0);
}

#[test]
fn malformed_directory_and_payload_never_panic() {
    let bytes = encoded(&(0..257).map(|i| i * 129).collect::<Vec<_>>());
    for index in 0..bytes.len() {
        for replacement in [0, 127, 128, 255] {
            let mut changed = bytes.clone();
            changed[index] = replacement;
            if let Ok(view) = PositionBlocks::open(&changed, 257) {
                let _ = view.validate_all();
                for target in [0, 1, 128, 129, 16384, 33024, u32::MAX] {
                    let _ = view.seek_ge(target);
                }
            }
        }
    }
}

#[test]
fn selected_block_checks_tail_even_after_early_match() {
    let mut bytes = encoded(&[0, 1, 2]);
    *bytes.last_mut().unwrap() = 0;
    assert!(PositionBlocks::open(&bytes, 3).unwrap().seek_ge(0).is_err());
}

#[test]
fn detached_directory_reads_only_the_selected_payload_extent() {
    use pin_core::codec::position_blocks::PositionDirectory;
    let values: Vec<u32> = (0..60_000).map(|n| n * 3).collect();
    let bytes = encoded(&values);
    let size = PositionDirectory::encoded_len(&bytes[..8], 60_000).unwrap();
    let directory = PositionDirectory::open(&bytes[..size], bytes.len() - size, 60_000).unwrap();
    for target in [
        0,
        1,
        381,
        382,
        383,
        384,
        179_000,
        179_997,
        179_998,
        u32::MAX,
    ] {
        let mut payload_read = 0;
        let actual = directory.select(target).unwrap().and_then(|request| {
            let range = request.byte_range();
            payload_read += range.len();
            // copy only the selected extent to simulate a separate storage read.
            let fetched = bytes[size + range.start..size + range.end].to_vec();
            request.seek_ge(&fetched, target).unwrap().position
        });
        assert_eq!(actual, values.iter().copied().find(|&n| n >= target));
        assert!(payload_read <= 635);
        if target > *values.last().unwrap() {
            assert_eq!(payload_read, 0);
        }
    }
}

#[test]
fn detached_reader_rejects_wrong_extent_lengths_and_corruption() {
    use pin_core::codec::position_blocks::PositionDirectory;
    let bytes = encoded(&(0..257).collect::<Vec<_>>());
    let size = PositionDirectory::encoded_len(&bytes[..8], 257).unwrap();
    for end in 0..size {
        assert!(PositionDirectory::open(&bytes[..end], bytes.len() - size, 257).is_err());
    }
    for extra in [1, 10] {
        assert!(PositionDirectory::open(&bytes[..size], bytes.len() - size + extra, 257).is_err());
    }
    let directory = PositionDirectory::open(&bytes[..size], bytes.len() - size, 257).unwrap();
    let request = directory.select(128).unwrap().unwrap();
    let range = request.byte_range();
    let payload = &bytes[size + range.start..size + range.end];
    for length in 0..payload.len() {
        assert!(request.seek_ge(&payload[..length], 128).is_err());
    }
    let mut oversized = payload.to_vec();
    oversized.push(1);
    assert!(request.seek_ge(&oversized, 128).is_err());
    for index in 0..payload.len() {
        let mut corrupt = payload.to_vec();
        corrupt[index] = 0;
        assert!(request.seek_ge(&corrupt, 128).is_err());
    }
    // a singleton has no delta payload, but still has one checked position.
    let singleton = directory.select(256).unwrap().unwrap();
    assert!(singleton.byte_range().is_empty());
    assert_eq!(singleton.seek_ge(&[], 256).unwrap().position, Some(256));
}

#[test]
fn external_fetch_preserves_errors_and_never_reads_for_bounds_only_answers() {
    use pin_core::codec::position_blocks::PositionDirectory;
    let bytes = encoded(&(0..257).collect::<Vec<_>>());
    let size = PositionDirectory::encoded_len(&bytes[..8], 257).unwrap();
    let directory = PositionDirectory::open(&bytes[..size], bytes.len() - size, 257).unwrap();
    let mut reads = 0;
    for target in [0, 127, 128, 255, 256, 257] {
        let result = directory
            .seek_with::<pin_core::error::Error>(target, |range, output| {
                reads += 1;
                assert_eq!(output.len(), range.len());
                output.copy_from_slice(&bytes[size + range.start..size + range.end]);
                Ok(())
            })
            .unwrap();
        assert_eq!(result.position, (target < 257).then_some(target));
    }
    assert_eq!(reads, 4);
    let error = directory
        .seek_with::<pin_core::error::Error>(0, |_, _| {
            Err(pin_core::error::Error::Limit(
                "injected storage cancellation",
            ))
        })
        .unwrap_err();
    assert_eq!(
        error,
        pin_core::error::Error::Limit("injected storage cancellation")
    );
}
