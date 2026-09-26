use pin_core::identity::{HeapLayout, RootTid};
use pin_core::primary::{MAX_SORT_RECORD_BYTES, TermSortRecord, decode_sort_record};

#[test]
fn byte_order_matches_term_and_root_order() {
    let layout = HeapLayout::new(291).unwrap();
    let roots = [
        RootTid::new(5, 2, layout).unwrap(),
        RootTid::new(4, 3, layout).unwrap(),
    ];
    let records = [
        TermSortRecord {
            term: "βeta",
            root: roots[0],
        },
        TermSortRecord {
            term: "alpha",
            root: roots[0],
        },
        TermSortRecord {
            term: "alpha",
            root: roots[1],
        },
        TermSortRecord {
            term: "alphabet",
            root: roots[0],
        },
        TermSortRecord {
            term: "alpha",
            root: roots[0],
        },
    ];
    let mut encoded = Vec::new();
    for record in records {
        let mut buffer = [0u8; MAX_SORT_RECORD_BYTES];
        let len = record.encode(&mut buffer).unwrap();
        encoded.push(buffer[..len].to_vec());
    }
    encoded.sort();
    let actual: Vec<_> = encoded
        .iter()
        .map(|bytes| decode_sort_record(bytes, layout).unwrap())
        .collect();
    let mut expected = records.to_vec();
    expected.sort_by(|a, b| a.term.cmp(b.term).then(a.root.cmp(&b.root)));
    assert_eq!(actual, expected);
}

#[test]
fn malformed_sort_records_fail_closed() {
    let layout = HeapLayout::new(291).unwrap();
    let record = TermSortRecord {
        term: "hello",
        root: RootTid::new(0, 1, layout).unwrap(),
    };
    let mut buffer = [0u8; MAX_SORT_RECORD_BYTES];
    let len = record.encode(&mut buffer).unwrap();
    assert!(record.encode(&mut buffer[..len - 1]).is_err());
    assert!(decode_sort_record(&buffer[..len - 1], layout).is_err());
    buffer[5] = 1;
    assert!(decode_sort_record(&buffer[..len], layout).is_err());
    let bad = TermSortRecord {
        term: "a\0b",
        ..record
    };
    assert!(bad.encode(&mut buffer).is_err());
}
