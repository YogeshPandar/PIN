#![no_main]

use libfuzzer_sys::fuzz_target;
use pin_core::analysis::{AnalysisLimits, Analyzed, PROFILE_ID};
use pin_core::codec::records::{DocumentRecord, Publication};
use pin_core::identity::{
    Generation, HeapLayout, Incarnation, RelationGeneration, RootTid, SegmentId,
};
use pin_core::index::{Document, IndexLimits, ReferenceIndex, SearchLimits};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use pin_core::rank::RankRequest;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    let limits = QueryLimits {
        bytes: 4096,
        nodes: 128,
        depth: 32,
        terms: 64,
        term_bytes: 4096,
        memory_bytes: 1 << 20,
    };
    if let Ok(query) = Query::decode(data, limits) {
        let mut encoded = [0; 8192];
        let len = query.encode(&mut encoded, 8192).unwrap();
        assert_eq!(&encoded[..len], data);
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut pieces = text.split('\0');
    let source = pieces.next().unwrap();
    let Ok(query) = Query::parse(source, limits) else {
        return;
    };
    let mut wire = [0; 8192];
    let len = query.encode(&mut wire, 8192).unwrap();
    let restored = Query::decode(&wire[..len], limits).unwrap();
    assert_eq!(restored.source(), source);
    let analysis = AnalysisLimits {
        input_bytes: 4096,
        normalized_bytes: 16_384,
        tokens: 8192,
        term_bytes: 16_384,
        memory_bytes: 1 << 20,
    };
    let mut texts: Vec<_> = pieces
        .take(4)
        .map(|text| Analyzed::analyze(text, analysis).unwrap())
        .collect();
    texts.push(Analyzed::analyze("a a b alpha alpine café Σ", analysis).unwrap());
    texts.push(Analyzed::analyze("", analysis).unwrap());
    let relation = RelationGeneration::new(1, 2, 3, Generation::new(1).unwrap()).unwrap();
    let layout = HeapLayout::new(128).unwrap();
    let docs: Vec<_> = texts
        .iter()
        .enumerate()
        .map(|(index, text)| {
            Document::new(
                relation,
                DocumentRecord {
                    segment: SegmentId::new(1).unwrap(),
                    incarnation: Incarnation::new(index as u64 + 1).unwrap(),
                    root: RootTid::new(index as u32, 1, layout).unwrap(),
                    token_count: text.len(),
                    profile: PROFILE_ID,
                    publication: Publication::Published,
                    live: true,
                },
                text,
            )
            .unwrap()
        })
        .collect();
    let index = ReferenceIndex::build(
        relation,
        &docs,
        IndexLimits {
            term_bytes: 16_384,
            ..IndexLimits::default()
        },
    )
    .unwrap();
    let search = SearchLimits {
        expanded_terms: 1_000_000,
        memory_bytes: 8 << 20,
        work_steps: 100_000_000,
    };
    let expected: Vec<_> = docs
        .iter()
        .filter(|doc| oracle::matches(doc.analyzed(), &query, 1 << 20, 100_000_000).unwrap())
        .map(|doc| doc.identity())
        .collect();
    let actual: Vec<_> = index
        .search(&restored, search)
        .unwrap()
        .map(|hit| hit.unwrap().document)
        .collect();
    assert_eq!(actual, expected);
    let epoch = index.statistics(1).unwrap();
    let eligible = |id: pin_core::identity::DocumentRef| Ok(id.root.block() % 2 == 0);
    let full = index
        .rank(
            &query,
            epoch,
            RankRequest {
                limit: docs.len(),
                search,
                ..RankRequest::default()
            },
            eligible,
        )
        .unwrap();
    let k = data.first().copied().unwrap_or(0) as usize % (docs.len() + 1);
    let offset = data.last().copied().unwrap_or(0) as usize % (docs.len() + 1);
    let top = index
        .rank(
            &query,
            epoch,
            RankRequest {
                limit: k,
                offset,
                search,
                ..RankRequest::default()
            },
            eligible,
        )
        .unwrap();
    assert_eq!(
        top,
        full.into_iter().skip(offset).take(k).collect::<Vec<_>>()
    );
});
