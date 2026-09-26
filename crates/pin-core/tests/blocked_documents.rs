#[path = "support/mutable_store.rs"]
mod mutable_store;
use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::identity::RootTid;
use pin_core::mutable::document::{self, PreparedDocument, SelectedPositions};
use pin_core::mutable::{self, PageStore};

fn prepare(text: &str, blocked: bool) -> PreparedDocument {
    let analyzed = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
    if blocked {
        PreparedDocument::prepare_blocked(&analyzed, 64 << 20)
    } else {
        PreparedDocument::prepare(&analyzed, 64 << 20)
    }
    .unwrap()
}

#[test]
fn both_document_formats_match_positions_and_independent_phrase_oracle() {
    for count in [0, 1, 127, 128, 255, 256, 257, 1000, 10_000] {
        let text = format!("{}omega zulu", "alpha beta alpha ".repeat(count));
        let words: Vec<&str> = text.split_whitespace().collect();
        let delta = prepare(&text, false);
        let blocked = prepare(&text, true);
        let delta_terms: Vec<_> = delta
            .terms()
            .map(|term| {
                let term = term.unwrap();
                (
                    term.term.to_owned(),
                    term.positions()
                        .unwrap()
                        .iter()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                )
            })
            .collect();
        let blocked_terms: Vec<_> = blocked
            .terms()
            .map(|term| {
                let term = term.unwrap();
                (
                    term.term.to_owned(),
                    term.positions()
                        .unwrap()
                        .iter()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                )
            })
            .collect();
        assert_eq!(delta_terms, blocked_terms);
        for doc in [&delta, &blocked] {
            assert_eq!(
                document::validate(doc.bytes(), 64 << 20).unwrap(),
                (words.len() as u32, doc.term_count())
            );
            for phrase in [
                vec!["alpha", "beta"],
                vec!["alpha", "alpha"],
                vec!["beta", "alpha", "alpha"],
                vec!["omega", "zulu"],
                vec!["zulu", "omega"],
                vec!["missing", "alpha"],
            ] {
                let expected = words.windows(phrase.len()).any(|window| window == phrase);
                let query: Vec<String> = phrase.into_iter().map(String::from).collect();
                assert_eq!(
                    document::phrase_matches(
                        doc.bytes(),
                        doc.token_count(),
                        doc.term_count(),
                        &query,
                        64 << 20
                    )
                    .unwrap(),
                    expected
                );
                let selected = SelectedPositions::read(
                    doc.bytes(),
                    doc.token_count(),
                    doc.term_count(),
                    &query,
                )
                .unwrap();
                assert_eq!(selected.phrase_matches().unwrap(), expected);
                if doc.bytes().starts_with(b"PD03") {
                    let result = mutable::block_phrase::matches(
                        doc.bytes().len(),
                        doc.token_count(),
                        doc.term_count(),
                        &query,
                        1 << 20,
                        &mut Vec::new(),
                        |offset, output| {
                            output.copy_from_slice(&doc.bytes()[offset..offset + output.len()]);
                            Ok(())
                        },
                    )
                    .unwrap();
                    assert_eq!(result, Some(expected));
                }
            }
        }
    }
}

#[test]
fn blocked_documents_validate_truncation_flags_and_resource_limits() {
    let text = "alpha beta ".repeat(256);
    let doc = prepare(&text, true);
    for length in 0..doc.bytes().len() {
        assert!(document::validate(&doc.bytes()[..length], 1 << 20).is_err());
    }
    let mut wrong = doc.bytes().to_vec();
    wrong[18..20].copy_from_slice(&2u16.to_le_bytes());
    assert!(document::validate(&wrong, 1 << 20).is_err());
    let mut wrong = doc.bytes().to_vec();
    wrong[..4].copy_from_slice(b"PD02");
    assert!(document::validate(&wrong, 1 << 20).is_err());
    let analyzed = Analyzed::analyze(&text, AnalysisLimits::default()).unwrap();
    assert!(PreparedDocument::prepare_blocked(&analyzed, 0).is_err());
    assert!(document::validate(doc.bytes(), 0).is_err());
    let empty = prepare("", true);
    assert_eq!(document::validate(empty.bytes(), 0).unwrap(), (0, 0));
}

#[test]
fn pd03_cannot_be_published_without_a_storage_capability() {
    let mut store = MemoryStore::default();
    mutable::initialize(&mut store).unwrap();
    let before = store.blocks().unwrap();
    let root = RootTid::new(1, 1, store.layout()).unwrap();
    let doc = prepare(&"echo ".repeat(300), true);
    assert!(mutable::insert(&mut store, root, &doc).is_err());
    assert_eq!(store.blocks().unwrap(), before);
}

#[test]
fn pd03_rejects_cross_term_duplicate_positions_even_with_valid_blocks() {
    let doc = prepare(&"alpha beta ".repeat(256), true);
    let mut bytes = doc.bytes().to_vec();
    let streams: Vec<_> = bytes
        .windows(4)
        .enumerate()
        .filter_map(|(i, b)| (b == b"PB01").then_some(i))
        .collect();
    assert_eq!(streams.len(), 2);
    let second = streams[1];
    // shift every beta block onto alpha's positions; each block remains valid.
    for block in 0..2 {
        for field in [0, 4] {
            let offset = second + 8 + block * 16 + field;
            let value = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            bytes[offset..offset + 4].copy_from_slice(&(value - 1).to_le_bytes());
        }
    }
    assert!(document::validate(&bytes, 1 << 20).is_err());
}

fn scan_exact(store: &mut MemoryStore, source: &str, budget: usize) -> Vec<(RootTid, bool)> {
    use pin_core::query::{Query, QueryLimits};
    let query = Query::parse(source, QueryLimits::default()).unwrap();
    let mut rows = Vec::new();
    mutable::scan_query_with_options(store, &query, budget, true, |root, recheck| {
        rows.push((root, recheck));
        Ok(())
    })
    .unwrap();
    rows
}

#[test]
fn inline_and_mapped_pd03_queries_and_persisted_capability() {
    let mut store = MemoryStore {
        blocked_positions: true,
        ..Default::default()
    };
    mutable::initialize(&mut store).unwrap();
    store.blocked_positions = false;
    for (i, count) in [1, 300, 60_000].into_iter().enumerate() {
        let text = format!("{}omega zulu", "echo ".repeat(count));
        let root = RootTid::new(1, i as u16 + 1, store.layout()).unwrap();
        let doc = prepare(&text, true);
        mutable::insert(&mut store, root, &doc).unwrap();
    }
    for phrase in ["\"omega zulu\"", "\"echo omega\""] {
        let rows = scan_exact(&mut store, phrase, 1 << 20);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|(_, recheck)| !recheck));
    }
    assert!(scan_exact(&mut store, "\"omega echo\"", 1 << 20).is_empty());
    assert_eq!(scan_exact(&mut store, "\"echo echo\"", 1 << 20).len(), 2);
    assert!(
        scan_exact(&mut store, "\"omega zulu\"", 16 << 10)
            .iter()
            .all(|(_, recheck)| *recheck)
    );
}

#[test]
fn late_negative_phrase_does_not_fetch_the_dense_delta_payload() {
    use pin_core::codec::position_blocks::PositionDirectory;
    let doc = prepare(&format!("{}omega zulu", "echo ".repeat(60_000)), true);
    let stream_start = 16 + 8 + 4;
    let directory_len =
        PositionDirectory::encoded_len(&doc.bytes()[stream_start..stream_start + 8], 60_002)
            .unwrap();
    let stream_size = u32::from_le_bytes(doc.bytes()[20..24].try_into().unwrap()) as usize;
    let delta_start = stream_start + directory_len;
    let delta_end = stream_start + stream_size;
    let mut reads = Vec::new();
    let query = vec!["omega".to_owned(), "echo".to_owned()];
    let result = mutable::block_phrase::matches(
        doc.bytes().len(),
        doc.token_count(),
        doc.term_count(),
        &query,
        1 << 20,
        &mut Vec::new(),
        |offset, output| {
            reads.push(offset..offset + output.len());
            output.copy_from_slice(&doc.bytes()[offset..offset + output.len()]);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(result, Some(false));
    assert!(
        reads
            .iter()
            .all(|range| range.end <= delta_start || range.start >= delta_end)
    );
    assert!(
        mutable::block_phrase::matches(
            doc.bytes().len(),
            doc.token_count(),
            doc.term_count(),
            &query,
            1 << 20,
            &mut Vec::new(),
            |_, _| Err(pin_core::error::Error::Limit("cancel"))
        )
        .is_err()
    );
}

#[test]
fn pd03_publication_failure_recovery_and_vacuum_preserve_exact_queries() {
    use pin_core::mutable::Stage;
    let mut base = MemoryStore {
        blocked_positions: true,
        ..Default::default()
    };
    mutable::initialize(&mut base).unwrap();
    let root = RootTid::new(1, 1, base.layout()).unwrap();
    let doc = prepare(&format!("{}omega zulu", "echo ".repeat(24_000)), true);
    let mut complete = base.clone();
    complete.events.clear();
    mutable::insert(&mut complete, root, &doc).unwrap();
    for step in 0..complete.events.len() {
        let mut store = base.clone();
        store.events.clear();
        store.fail_at = Some(step);
        assert!(mutable::insert(&mut store, root, &doc).is_err());
        store.fail_at = None;
        let published = store.events.last() == Some(&Stage::Published);
        assert_eq!(
            !scan_exact(&mut store, "\"omega zulu\"", 1 << 20).is_empty(),
            published
        );
        mutable::vacuum(&mut store, |_| Ok(true)).unwrap();
        assert!(scan_exact(&mut store, "\"omega zulu\"", 1 << 20).is_empty());
        mutable::insert(&mut store, root, &doc).unwrap();
        assert_eq!(
            scan_exact(&mut store, "\"omega zulu\"", 1 << 20),
            vec![(root, false)]
        );
    }
}
