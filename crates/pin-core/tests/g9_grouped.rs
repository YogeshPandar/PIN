//! Independent incarnation-set oracle and malformed logical-group fixtures.
//! Private-image replay is not PostgreSQL crash or WAL qualification.

use pin_core::error::{Error, Result};
use pin_core::grouped::{
    Bitmap, BitmapKind, GroupKey, HEADER_BYTES, Member, Members, Node, PageOffsets, QueryScratch,
    QueryStats, SegmentGroup, encode_bitmap, encode_members, evaluate, retire,
};
use pin_core::identity::{Generation, HeapLayout, Incarnation, RootTid, SegmentId};
use std::collections::{BTreeMap, BTreeSet};

fn key(base: u32, domain: u16, segment: u64) -> GroupKey {
    GroupKey::new(
        Generation::new(7).unwrap(),
        SegmentId::new(segment).unwrap(),
        base,
        HeapLayout::new(domain).unwrap(),
    )
    .unwrap()
}

fn member(key: GroupKey, page: u8, offset: u16, incarnation: u64) -> Member {
    Member {
        root: RootTid::new(key.base() | u32::from(page), offset, key.layout()).unwrap(),
        incarnation: Incarnation::new(incarnation).unwrap(),
    }
}

fn images(key: GroupKey, members: &[Member]) -> (Vec<u8>, Vec<u8>) {
    let mut owners = vec![0; HEADER_BYTES + members.len() * 16];
    let len = encode_members(key, members, &mut owners).unwrap();
    owners.truncate(len);
    let mut live = vec![0; 20_000];
    let len = Members::open(&owners)
        .unwrap()
        .encode_liveness(&mut live)
        .unwrap();
    live.truncate(len);
    (owners, live)
}

fn posting(key: GroupKey, roots: impl IntoIterator<Item = RootTid>) -> Vec<u8> {
    let mut pages = BTreeMap::new();
    for root in roots {
        let words = pages.entry(root.block() as u8).or_insert([0u64; 8]);
        let bit = usize::from(root.offset() - 1);
        words[bit / 64] |= 1 << (bit % 64);
    }
    let pages: Vec<_> = pages
        .into_iter()
        .map(|(page, offsets)| PageOffsets { page, offsets })
        .collect();
    let mut bytes = vec![0; 20_000];
    let len = encode_bitmap(key, BitmapKind::Posting, &pages, &mut bytes).unwrap();
    bytes.truncate(len);
    bytes
}

fn run(
    owners: &[u8],
    live: &[u8],
    terms: &[Vec<u8>],
    program: &[Node],
) -> Result<(BTreeSet<u64>, QueryStats)> {
    let segment = SegmentGroup::open(owners, live)?;
    let terms: Vec<_> = terms
        .iter()
        .map(|bytes| Bitmap::open(bytes))
        .collect::<Result<_>>()?;
    let mut found = BTreeSet::new();
    let stats = evaluate(
        &segment,
        &terms,
        program,
        &mut QueryScratch::default(),
        || Ok(()),
        |block, words| {
            for offset in 1..=segment.key().layout().max_offset() {
                let bit = usize::from(offset - 1);
                if words[bit / 64] & (1 << (bit % 64)) != 0 {
                    let root = RootTid::new(block, offset, segment.key().layout()).unwrap();
                    let owner = segment.members().find(root)?.unwrap();
                    assert!(found.insert(owner.incarnation.get()));
                }
            }
            Ok(())
        },
    )?;
    Ok((found, stats))
}

fn oracle(program: &[Node], terms: &[BTreeSet<u64>], live: &BTreeSet<u64>) -> BTreeSet<u64> {
    let mut values: Vec<BTreeSet<u64>> = Vec::new();
    for node in program {
        values.push(match *node {
            Node::Term(term) => terms[term].intersection(live).copied().collect(),
            Node::All => live.clone(),
            Node::And(left, right) => values[left].intersection(&values[right]).copied().collect(),
            Node::Or(left, right) => values[left].union(&values[right]).copied().collect(),
            Node::Difference(left, right) => {
                values[left].difference(&values[right]).copied().collect()
            }
            Node::Not(child) => live.difference(&values[child]).copied().collect(),
        });
    }
    values.pop().unwrap()
}

fn next(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn nested_boolean_programs_match_independent_incarnation_sets() {
    let programs = [
        vec![Node::Term(0), Node::Term(1), Node::And(0, 1)],
        vec![Node::Term(0), Node::Term(1), Node::Or(0, 1)],
        vec![Node::Term(0), Node::Term(1), Node::Difference(0, 1)],
        vec![Node::Term(0), Node::Not(0)],
        vec![
            Node::Term(0),
            Node::Term(1),
            Node::And(0, 1),
            Node::Term(2),
            Node::Or(2, 3),
        ],
        vec![Node::Term(0), Node::Term(1), Node::Or(0, 1), Node::Not(2)],
        vec![Node::All, Node::Not(0)],
        vec![Node::All],
    ];
    for domain in [1, 8, 32, 64, 65, 128, 291, 512] {
        for sample in 1..=12u64 {
            let key = key(if sample % 2 == 0 { 256 } else { 0 }, domain, sample);
            let mut seed = sample;
            let mut roots = BTreeSet::new();
            for _ in 0..160 {
                let page = next(&mut seed) as u8;
                let offset = (next(&mut seed) % u64::from(domain)) as u16 + 1;
                roots.insert(
                    RootTid::new(key.base() | u32::from(page), offset, key.layout()).unwrap(),
                );
            }
            let members: Vec<_> = roots
                .into_iter()
                .enumerate()
                .map(|(index, root)| Member {
                    root,
                    incarnation: Incarnation::new(index as u64 + 1).unwrap(),
                })
                .collect();
            let (owners, mut live) = images(key, &members);
            let map = Members::open(&owners).unwrap();
            let mut alive: BTreeSet<_> = members.iter().map(|m| m.incarnation.get()).collect();
            let mut sets = vec![BTreeSet::new(); 3];
            for member in &members {
                for set in &mut sets {
                    if next(&mut seed) & 3 != 0 {
                        set.insert(member.incarnation.get());
                    }
                }
                if next(&mut seed) & 3 == 0 {
                    assert!(retire(&map, &mut live, *member).unwrap());
                    alive.remove(&member.incarnation.get());
                }
            }
            let terms: Vec<_> = sets
                .iter()
                .map(|set| {
                    posting(
                        key,
                        members
                            .iter()
                            .filter(|m| set.contains(&m.incarnation.get()))
                            .map(|m| m.root),
                    )
                })
                .collect();
            for program in &programs {
                let (actual, stats) = run(&owners, &live, &terms, program).unwrap();
                assert_eq!(actual, oracle(program, &sets, &alive));
                assert_eq!(stats.emitted_tids as usize, actual.len());
            }
        }
    }
}

#[test]
fn reused_tid_cannot_create_a_cross_generation_and_match() {
    let old_key = key(0, 291, 1);
    let new_key = key(0, 291, 2);
    let old = member(old_key, 9, 1, 10);
    let new = member(new_key, 9, 1, 11);
    assert_eq!(old.root, new.root);
    let (old_map, old_live) = images(old_key, &[old]);
    let (new_map, mut new_live) = images(new_key, &[new]);
    let old_terms = [posting(old_key, [old.root]), posting(old_key, [])];
    let new_terms = [posting(new_key, []), posting(new_key, [new.root])];
    let query = [Node::Term(0), Node::Term(1), Node::And(0, 1)];
    assert!(
        run(&old_map, &old_live, &old_terms, &query)
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        run(&new_map, &new_live, &new_terms, &query)
            .unwrap()
            .0
            .is_empty()
    );
    assert!(
        run(
            &old_map,
            &old_live,
            &[old_terms[0].clone(), new_terms[1].clone()],
            &query
        )
        .is_err()
    );
    let before = new_live.clone();
    assert!(retire(&Members::open(&new_map).unwrap(), &mut new_live, old).is_err());
    assert_eq!(before, new_live);
    assert!(retire(&Members::open(&old_map).unwrap(), &mut new_live, old).is_err());
    assert_eq!(before, new_live);
}

#[test]
fn duplicate_coordinate_is_rejected_before_overwriting_output() {
    let key = key(0, 512, 1);
    let one = member(key, 0, 1, 1);
    let two = member(key, 0, 1, 2);
    let mut output = [0xa5; 256];
    assert_eq!(
        encode_members(key, &[one, two], &mut output),
        Err(Error::DuplicateDocument)
    );
    assert_eq!(output, [0xa5; 256]);
}

#[test]
fn page_pruning_does_not_decode_a_rejected_payload() {
    let key = key(0, 65, 1);
    let docs = [member(key, 0, 1, 1), member(key, 200, 65, 2)];
    let (owners, live) = images(key, &docs);
    let mut common = posting(key, docs.iter().map(|m| m.root));
    // the first payload is noncanonical, but its page cannot survive this and.
    common[HEADER_BYTES + 2 * 4] = 0;
    assert!(Bitmap::open(&common).unwrap().validate_all().is_err());
    let rare = posting(key, [docs[1].root]);
    let terms = [common, rare];
    let query = [Node::Term(0), Node::Term(1), Node::And(0, 1)];
    let (found, stats) = run(&owners, &live, &terms, &query).unwrap();
    assert_eq!(found, BTreeSet::from([2]));
    assert_eq!(stats.candidate_pages, 1);
    assert_eq!(stats.term_payloads, 2);
    assert_eq!(stats.term_bytes, 18);
    assert!(
        run(
            &owners,
            &live,
            &terms,
            &[Node::Term(0), Node::Term(1), Node::Or(0, 1)]
        )
        .is_err()
    );
}

#[test]
fn shared_retirement_is_clear_only_idempotent_and_replayable() {
    let key = key(0, 512, 1);
    let docs = [
        member(key, 0, 1, 1),
        member(key, 0, 512, 2),
        member(key, 255, 64, 3),
    ];
    let (owners, mut live) = images(key, &docs);
    let map = Members::open(&owners).unwrap();
    let original = live.clone();
    let terms = [posting(key, docs.iter().map(|m| m.root))];
    for doc in docs {
        let before = live.clone();
        assert!(retire(&map, &mut live, doc).unwrap());
        assert_eq!(&before[..HEADER_BYTES], &live[..HEADER_BYTES]);
        assert!(
            live.iter()
                .zip(before)
                .all(|(&after, before)| after & !before == 0)
        );
        let durable_image = live.clone();
        assert!(!retire(&map, &mut live, doc).unwrap());
        assert_eq!(durable_image, live);
    }
    let (found, stats) = run(&owners, &live, &terms, &[Node::Term(0)]).unwrap();
    assert!(found.is_empty());
    assert_eq!(stats.term_payloads, 0);
    assert_eq!(stats.live_pages, 0);
    assert_eq!(
        run(&owners, &original, &terms, &[Node::Term(0)])
            .unwrap()
            .0
            .len(),
        3
    );
}

#[test]
fn difference_retains_left_offsets_when_both_terms_share_a_page() {
    let key = key(0, 64, 1);
    let docs = [member(key, 4, 1, 1), member(key, 4, 2, 2)];
    let (owners, live) = images(key, &docs);
    let terms = [posting(key, [docs[0].root]), posting(key, [docs[1].root])];
    let query = [Node::Term(0), Node::Term(1), Node::Difference(0, 1)];
    assert_eq!(
        run(&owners, &live, &terms, &query).unwrap().0,
        BTreeSet::from([1])
    );
}

#[test]
fn empty_universe_and_missing_terms_never_invent_offsets() {
    let key = key(0, 291, 1);
    let (owners, live) = images(key, &[]);
    let terms = [posting(key, [])];
    for query in [vec![Node::All], vec![Node::Term(0), Node::Not(0)]] {
        assert!(run(&owners, &live, &terms, &query).unwrap().0.is_empty());
    }
    let (owners, live) = images(key, &[member(key, 10, 291, 1)]);
    assert_eq!(
        run(&owners, &live, &terms, &[Node::Term(0), Node::Not(0)])
            .unwrap()
            .0,
        BTreeSet::from([1])
    );
    assert!(
        run(&owners, &live, &terms, &[Node::Term(0)])
            .unwrap()
            .0
            .is_empty()
    );
}

#[test]
fn final_heap_group_and_every_offset_domain_round_trip() {
    for domain in [1, 7, 8, 63, 64, 65, 291, 511, 512] {
        let key = key(u32::MAX - 255, domain, 1);
        let docs = [member(key, 0, 1, 1), member(key, 254, domain, 2)];
        let (owners, live) = images(key, &docs);
        let terms = [posting(key, docs.iter().map(|m| m.root))];
        assert_eq!(
            run(&owners, &live, &terms, &[Node::Term(0)]).unwrap().0,
            BTreeSet::from([1, 2])
        );
        let mut output = [0xa5; 256];
        let invalid = PageOffsets {
            page: 255,
            offsets: [1, 0, 0, 0, 0, 0, 0, 0],
        };
        assert!(encode_bitmap(key, BitmapKind::Posting, &[invalid], &mut output).is_err());
        assert_eq!(output, [0xa5; 256]);
    }
}

#[test]
fn malformed_headers_directories_and_truncations_fail_closed() {
    let key = key(0, 65, 1);
    let doc = member(key, 0, 65, 1);
    let (owners, live) = images(key, &[doc]);
    let bytes = posting(key, [doc.root]);
    for end in 0..bytes.len() {
        assert!(Bitmap::open(&bytes[..end]).is_err());
    }
    for end in 0..owners.len() {
        assert!(Members::open(&owners[..end]).is_err());
    }
    for (offset, value) in [
        (0, 0),
        (4, 2),
        (6, 9),
        (7, 1),
        (12, 1),
        (16, 0),
        (24, 0),
        (32, 0),
        (34, 2),
        (36, 1),
        (40, 0),
        (72, 1),
        (74, 0),
        (75, 1),
    ] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        assert!(
            Bitmap::open(&bad).is_err(),
            "accepted changed header byte {offset}"
        );
    }
    let mut bad_tail = bytes.clone();
    *bad_tail.last_mut().unwrap() = 2;
    assert!(Bitmap::open(&bad_tail).unwrap().offsets(0).is_err());
    let mut bad_owner = owners.clone();
    bad_owner[HEADER_BYTES + 8..HEADER_BYTES + 16].fill(0);
    assert!(Members::open(&bad_owner).is_err());
    let mut phantom = live.clone();
    phantom[HEADER_BYTES + 4] |= 2;
    assert!(SegmentGroup::open(&owners, &phantom).is_err());
    let before = phantom.clone();
    assert!(retire(&Members::open(&owners).unwrap(), &mut phantom, doc).is_err());
    assert_eq!(phantom, before);
    let mut extra = bytes.clone();
    extra.push(0);
    let len = extra.len() as u32;
    extra[8..12].copy_from_slice(&len.to_le_bytes());
    assert!(Bitmap::open(&extra).is_err());
}

#[test]
fn bad_programs_and_cancellation_do_not_report_success() {
    let key = key(0, 64, 1);
    let docs = [member(key, 0, 1, 1), member(key, 1, 1, 2)];
    let (owners, live) = images(key, &docs);
    let terms = [posting(key, docs.iter().map(|m| m.root))];
    for bad in [
        vec![],
        vec![Node::Term(1)],
        vec![Node::Not(0)],
        vec![Node::And(0, 0)],
        vec![Node::All; 65],
    ] {
        assert!(run(&owners, &live, &terms, &bad).is_err());
    }
    let segment = SegmentGroup::open(&owners, &live).unwrap();
    let term = Bitmap::open(&terms[0]).unwrap();
    let mut calls = 0;
    let mut emitted = 0;
    let result = evaluate(
        &segment,
        &[term],
        &[Node::Term(0)],
        &mut QueryScratch::default(),
        || {
            calls += 1;
            if calls == 3 {
                Err(Error::Limit("cancelled"))
            } else {
                Ok(())
            }
        },
        |_, _| {
            emitted += 1;
            Ok(())
        },
    );
    assert_eq!(result, Err(Error::Limit("cancelled")));
    assert_eq!(emitted, 1);
    let result = evaluate(
        &segment,
        &[term],
        &[Node::Term(0)],
        &mut QueryScratch::default(),
        || Ok(()),
        |_, _| Err(Error::InvalidState),
    );
    assert_eq!(result, Err(Error::InvalidState));
    assert_eq!(std::mem::size_of::<QueryScratch>(), 6144);
}

#[test]
fn sealing_checks_incarnations_before_discarding_owner_identity() {
    let key = key(256, 512, 1);
    let docs = [
        member(key, 1, 1, 10),
        member(key, 1, 512, 20),
        member(key, 255, 65, 30),
    ];
    let (owners, live) = images(key, &docs);
    let map = Members::open(&owners).unwrap();
    let mut output = vec![0xa5; 20_000];
    let stale = Member {
        root: docs[0].root,
        incarnation: Incarnation::new(9).unwrap(),
    };
    for invalid in [
        vec![stale],
        vec![docs[0], docs[0]],
        vec![docs[2], docs[0]],
        vec![member(key, 5, 5, 40)],
    ] {
        assert!(map.encode_posting(&invalid, &mut output).is_err());
        assert!(output.iter().all(|&byte| byte == 0xa5));
    }
    let len = map
        .encode_posting(&[docs[0], docs[2]], &mut output)
        .unwrap();
    output.truncate(len);
    assert_eq!(output, posting(key, [docs[0].root, docs[2].root]));
    assert_eq!(
        run(&owners, &live, &[output], &[Node::Term(0)]).unwrap().0,
        BTreeSet::from([10, 30])
    );
    let mut short = [0xa5; HEADER_BYTES];
    assert!(map.encode_posting(&docs, &mut short).is_err());
    assert_eq!(short, [0xa5; HEADER_BYTES]);
}

#[test]
fn nested_pruning_avoids_dead_subtrees_and_unreferenced_nodes() {
    let key = key(0, 65, 1);
    let docs = [member(key, 1, 65, 1), member(key, 2, 65, 2)];
    let (owners, live) = images(key, &docs);
    let mut broken = posting(key, [docs[0].root]);
    *broken.last_mut().unwrap() = 0;
    let terms = [
        broken,
        posting(key, [docs[1].root]),
        posting(key, [docs[0].root]),
    ];
    // the broken term is beneath a page-empty and, while the last term matches.
    let query = [
        Node::Term(0),
        Node::Term(1),
        Node::And(0, 1),
        Node::Term(2),
        Node::Or(2, 3),
    ];
    let (found, stats) = run(&owners, &live, &terms, &query).unwrap();
    assert_eq!(found, BTreeSet::from([1]));
    assert_eq!(stats.term_payloads, 1);
    assert_eq!(stats.term_bytes, 9);
    let (found, stats) = run(&owners, &live, &terms, &[Node::Term(0), Node::All]).unwrap();
    assert_eq!(found, BTreeSet::from([1, 2]));
    assert_eq!(stats.term_payloads, 0);
    // not must use the universe, not invert a coarse page summary.
    let query = [Node::Term(0), Node::Term(1), Node::And(0, 1), Node::Not(2)];
    assert_eq!(
        run(&owners, &live, &terms, &query).unwrap().0,
        BTreeSet::from([1, 2])
    );
}

#[test]
fn dense_group_directory_and_borrowed_unaligned_records_round_trip() {
    let key = key(0, 512, 1);
    let pages: Vec<_> = (0..=255u8)
        .map(|page| PageOffsets {
            page,
            offsets: [u64::MAX; 8],
        })
        .collect();
    let mut encoded = vec![0; 17_480];
    assert_eq!(
        encode_bitmap(key, BitmapKind::Posting, &pages, &mut encoded).unwrap(),
        17_480
    );
    for shift in 0..8 {
        let mut unaligned = vec![0xa5; shift];
        unaligned.extend_from_slice(&encoded);
        let view = Bitmap::open(&unaligned[shift..]).unwrap();
        view.validate_all().unwrap();
        for page in 0..=255u8 {
            assert_eq!(view.offsets(page).unwrap(), [u64::MAX; 8]);
        }
    }
    let mut short = vec![0xa5; encoded.len() - 1];
    assert!(encode_bitmap(key, BitmapKind::Posting, &pages, &mut short).is_err());
    assert!(short.iter().all(|&byte| byte == 0xa5));
}

#[test]
fn arbitrary_byte_mutations_never_panic_or_escape_the_domain() {
    let key = key(0, 65, 1);
    let docs = [
        member(key, 0, 1, 1),
        member(key, 63, 65, 2),
        member(key, 255, 64, 3),
    ];
    let (owners, live) = images(key, &docs);
    let term = posting(key, docs.iter().map(|m| m.root));
    let mut seed = 17;
    for original in [&owners, &live, &term] {
        for _ in 0..2_000 {
            let mut bytes = original.clone();
            let at = next(&mut seed) as usize % bytes.len();
            bytes[at] ^= next(&mut seed) as u8;
            if let Ok(view) = Bitmap::open(&bytes) {
                for page in 0..=255u8 {
                    if let Ok(words) = view.offsets(page) {
                        let domain = usize::from(view.key().layout().max_offset());
                        for bit in domain..512 {
                            assert_eq!(words[bit / 64] & (1 << (bit % 64)), 0);
                        }
                    }
                }
            }
            let _ = Members::open(&bytes);
            let _ = SegmentGroup::open(&owners, &bytes);
        }
    }
}

#[test]
fn query_work_scales_with_surviving_pages_not_common_term_length() {
    let key = key(0, 65, 1);
    let docs: Vec<_> = (0..=255u8)
        .map(|page| member(key, page, 65, u64::from(page) + 1))
        .collect();
    let (owners, live) = images(key, &docs);
    let terms = [
        posting(key, docs.iter().map(|doc| doc.root)),
        posting(key, [docs[255].root]),
        posting(key, []),
    ];
    let query = [Node::Term(0), Node::Term(1), Node::And(0, 1)];
    let (found, stats) = run(&owners, &live, &terms, &query).unwrap();
    assert_eq!(found, BTreeSet::from([256]));
    assert_eq!(stats.candidate_pages, 1);
    assert_eq!(stats.term_payloads, 2);
    assert_eq!(stats.term_bytes, 18);
    let query = [Node::Term(0), Node::Term(1), Node::Or(0, 1)];
    let (found, stats) = run(&owners, &live, &terms, &query).unwrap();
    assert_eq!(found.len(), 256);
    assert_eq!(stats.term_payloads, 257);
    assert_eq!(stats.term_bytes, 2313);
    let query = [Node::Term(0), Node::Term(2), Node::And(0, 1)];
    let (found, stats) = run(&owners, &live, &terms, &query).unwrap();
    assert!(found.is_empty());
    assert_eq!(stats, QueryStats::default());
}

#[test]
fn logical_bitmap_has_a_fixed_little_endian_golden_image() {
    let key = key(256, 65, 2);
    let bytes = posting(key, [member(key, 255, 65, 1).root]);
    // page 255 is bit 63 of word three; offset 65 is bit zero of byte eight.
    let golden = b"PNG9\x01\x00\x01\x00\x55\x00\x00\x00\x00\x01\x00\x00\
        \x07\x00\x00\x00\x00\x00\x00\x00\x02\x00\x00\x00\x00\x00\x00\x00\
        \x41\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
        \x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
        \x00\x00\x00\x00\x00\x00\x00\x80\x00\x00\x09\x00\x00\x00\x00\x00\
        \x00\x00\x00\x00\x01";
    assert_eq!(bytes.as_slice(), golden);
    assert_eq!(Bitmap::open(golden).unwrap().offsets(255).unwrap()[1], 1);
}

#[test]
fn offset_word_decoder_matches_bit_oracle_at_every_width_and_tail() {
    for domain in [1u16, 7, 8, 63, 64, 65, 291, 511, 512] {
        let key = key(0, domain, 1);
        for last in 1..=domain {
            let mut expected = [0u64; 8];
            for offset in 1..=last {
                if offset == last || offset % 3 == 0 {
                    let bit = usize::from(offset - 1);
                    expected[bit / 64] |= 1 << (bit % 64);
                }
            }
            let mut bytes = [0; 20_000];
            let pages = [PageOffsets {
                page: 255,
                offsets: expected,
            }];
            let len = encode_bitmap(key, BitmapKind::Posting, &pages, &mut bytes).unwrap();
            let view = Bitmap::open(&bytes[..len]).unwrap();
            assert_eq!(
                view.offsets(255).unwrap(),
                expected,
                "domain={domain}, last={last}"
            );
            assert_eq!(view.offsets(0).unwrap(), [0; 8]);
            assert_eq!(view.payload_bytes(255), usize::from(last).div_ceil(8));
        }
    }
}
