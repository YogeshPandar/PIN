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
