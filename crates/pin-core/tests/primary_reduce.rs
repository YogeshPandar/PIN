use pin_core::error::Error;
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::primary::{BuildReducer, TermSortRecord};

#[test]
fn sorted_terms_stream_into_bounded_heap_page_runs() {
    let layout = HeapLayout::new(291).unwrap();
    let samples = [
        ("a", 0, 1),
        ("a", 0, 1),
        ("a", 0, 2),
        ("a", 256, 5),
        ("b", 0, 1),
    ];
    let mut builder = BuildReducer::new(layout).unwrap();
    let mut actual = Vec::new();
    for (term, block, offset) in samples {
        let record = TermSortRecord {
            term,
            root: RootTid::new(block, offset, layout).unwrap(),
        };
        builder
            .push(record, |term, base, page, offsets| {
                actual.push((term.to_owned(), base, page, offsets.to_vec()));
                Ok(())
            })
            .unwrap();
    }
    let work = builder
        .finish(|term, base, page, offsets| {
            actual.push((term.to_owned(), base, page, offsets.to_vec()));
            Ok(())
        })
        .unwrap();
    assert_eq!(work.records, 5);
    assert_eq!(work.duplicate_roots, 1);
    assert_eq!(work.page_runs, 3);
    assert_eq!(
        actual,
        vec![
            ("a".to_owned(), 0, 0, vec![1, 2]),
            ("a".to_owned(), 256, 0, vec![5]),
            ("b".to_owned(), 0, 0, vec![1]),
        ]
    );
}

#[test]
fn invalid_order_or_emit_failure_poison_the_stream() {
    let layout = HeapLayout::new(291).unwrap();
    let a = TermSortRecord {
        term: "a",
        root: RootTid::new(0, 2, layout).unwrap(),
    };
    let smaller = TermSortRecord {
        root: RootTid::new(0, 1, layout).unwrap(),
        ..a
    };
    let mut builder = BuildReducer::new(layout).unwrap();
    builder.push(a, |_, _, _, _| Ok(())).unwrap();
    assert!(builder.push(smaller, |_, _, _, _| Ok(())).is_err());
    assert!(builder.push(a, |_, _, _, _| Ok(())).is_err());
    assert!(builder.finish(|_, _, _, _| Ok(())).is_err());

    let mut builder = BuildReducer::new(layout).unwrap();
    builder.push(a, |_, _, _, _| Ok(())).unwrap();
    let b = TermSortRecord { term: "b", ..a };
    assert!(
        builder
            .push(b, |_, _, _, _| Err(Error::InvalidState))
            .is_err()
    );
    assert!(builder.finish(|_, _, _, _| Ok(())).is_err());

    let wider = HeapLayout::new(512).unwrap();
    let invalid = TermSortRecord {
        term: "c",
        root: RootTid::new(0, 400, wider).unwrap(),
    };
    let mut builder = BuildReducer::new(layout).unwrap();
    assert!(builder.push(invalid, |_, _, _, _| Ok(())).is_err());
}
