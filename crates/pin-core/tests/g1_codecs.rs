use pin_core::codec::ErrorKind;
use pin_core::codec::bytes::{Reader, Writer, var_u32_len};
use pin_core::codec::positions::{self, Positions};

#[test]
fn varint_golden_boundaries_and_atomic_errors() {
    let cases: &[(u32, &[u8])] = &[
        (0, &[0]),
        (127, &[127]),
        (128, &[128, 1]),
        (16383, &[255, 127]),
        (16384, &[128, 128, 1]),
        (u32::MAX, &[255, 255, 255, 255, 15]),
    ];
    for &(value, expected) in cases {
        let mut output = [0; 5];
        let mut writer = Writer::new(&mut output);
        writer.var_u32(value).unwrap();
        assert_eq!(writer.len(), var_u32_len(value));
        assert_eq!(&output[..expected.len()], expected);
        let mut reader = Reader::new(expected);
        assert_eq!(reader.var_u32().unwrap(), value);
        reader.finish().unwrap();
        for prefix in 0..expected.len() {
            let mut reader = Reader::new(&expected[..prefix]);
            assert!(reader.var_u32().is_err());
            assert_eq!(reader.offset(), 0);
        }
    }
    for bytes in [
        &[128, 0][..],
        &[129, 0],
        &[255, 255, 255, 255, 16],
        &[128; 6],
    ] {
        let mut reader = Reader::new(bytes);
        assert!(reader.var_u32().is_err());
        assert_eq!(reader.offset(), 0);
    }
}

#[test]
fn position_golden_preserves_zero_gaps_and_extremes() {
    let mut bytes = [0; 64];
    let len = positions::encode(&[0, 1, 128, 300], &mut bytes, 4).unwrap();
    assert_eq!(&bytes[..len], &[4, 0, 0, 0, 0, 1, 127, 172, 1]);
    let positions = Positions::parse(&bytes[..len], 4).unwrap();
    assert_eq!(
        positions.iter().collect::<Result<Vec<_>, _>>().unwrap(),
        [0, 1, 128, 300]
    );
    for prefix in 0..len {
        assert!(Positions::parse(&bytes[..prefix], 4).is_err());
    }
    assert!(Positions::parse(&bytes[..len], 3).is_err());
    assert!(Positions::parse(&bytes[..len + 1], 4).is_err());
    assert!(positions::encode(&[1, 1], &mut bytes, 4).is_err());
    assert!(positions::encode(&[2, 1], &mut bytes, 4).is_err());
    let len = positions::encode(&[0, u32::MAX], &mut bytes, 4).unwrap();
    assert_eq!(
        Positions::parse(&bytes[..len], 4)
            .unwrap()
            .iter()
            .last()
            .unwrap()
            .unwrap(),
        u32::MAX
    );
    assert_eq!(
        Positions::parse(&[2, 0, 0, 0, 255, 255, 255, 255, 15, 1], 2)
            .unwrap_err()
            .kind,
        ErrorKind::Overflow
    );
}

#[test]
fn byte_extents_fail_without_advancing() {
    let mut reader = Reader::new(&[1, 2, 3]);
    assert_eq!(
        reader.take(usize::MAX).unwrap_err().kind,
        ErrorKind::Truncated
    );
    assert_eq!(reader.offset(), 0);
    reader.take(1).unwrap();
    assert_eq!(
        reader.take(usize::MAX).unwrap_err().kind,
        ErrorKind::Overflow
    );
    assert_eq!(reader.offset(), 1);
}

#[test]
fn deterministic_varint_roundtrips() {
    let mut seed = 0x9e3779b9u32;
    for _ in 0..20_000 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let mut bytes = [0; 5];
        let mut writer = Writer::new(&mut bytes);
        writer.var_u32(seed).unwrap();
        let len = writer.len();
        assert_eq!(Reader::new(&bytes[..len]).var_u32().unwrap(), seed);
    }
}
