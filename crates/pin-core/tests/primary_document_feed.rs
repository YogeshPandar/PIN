use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::Error;
use pin_core::identity::{HeapLayout, RootTid};
use pin_core::primary::TermSortRecord;

#[test]
fn repeated_tokens_emit_one_normalized_record_per_term() {
    let analyzed = Analyzed::analyze(
        "Fast fast FASTER café CAFE\u{301}",
        AnalysisLimits::default(),
    )
    .unwrap();
    let root = RootTid::new(9, 4, HeapLayout::new(291).unwrap()).unwrap();
    let mut actual = Vec::new();
    let count = TermSortRecord::visit_document(&analyzed, root, 32 << 20, |record| {
        actual.push((record.term.to_owned(), record.root));
        Ok(())
    })
    .unwrap();
    assert_eq!(count, 3);
    assert_eq!(
        actual
            .iter()
            .map(|item| item.0.as_str())
            .collect::<Vec<_>>(),
        ["café", "fast", "faster"]
    );
    assert!(actual.iter().all(|item| item.1 == root));
}

#[test]
fn empty_budget_and_callback_failure_do_not_report_success() {
    let empty = Analyzed::analyze("", AnalysisLimits::default()).unwrap();
    let root = RootTid::new(0, 1, HeapLayout::new(291).unwrap()).unwrap();
    assert_eq!(
        TermSortRecord::visit_document(&empty, root, 32 << 20, |_| panic!("term")),
        Ok(0)
    );
    let analyzed = Analyzed::analyze("a b", AnalysisLimits::default()).unwrap();
    assert!(matches!(
        TermSortRecord::visit_document(&analyzed, root, 1, |_| Ok(())),
        Err(Error::Budget(_))
    ));
    let mut calls = 0;
    assert!(matches!(
        TermSortRecord::visit_document(&analyzed, root, 32 << 20, |_| {
            calls += 1;
            Err(Error::InvalidState)
        }),
        Err(Error::InvalidState)
    ));
    assert_eq!(calls, 1);
}
