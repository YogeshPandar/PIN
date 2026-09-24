//! Logical compaction tests; publication and PostgreSQL recovery remain host gates.

use pin_core::error::Error;
use pin_core::grouped::{
    Bitmap, GroupKey, HEADER_BYTES, Member, Members, MergePlan, Node, QueryScratch, SegmentGroup,
    encode_members, evaluate, retire,
};
use pin_core::identity::{Generation, HeapLayout, Incarnation, RootTid, SegmentId};
use std::collections::BTreeSet;

fn key(segment: u64) -> GroupKey {
    GroupKey::new(
        Generation::new(1).unwrap(),
        SegmentId::new(segment).unwrap(),
        0,
        HeapLayout::new(512).unwrap(),
    )
    .unwrap()
}

fn member(block: u32, offset: u16, incarnation: u64) -> Member {
    Member {
        root: RootTid::new(block, offset, key(1).layout()).unwrap(),
        incarnation: Incarnation::new(incarnation).unwrap(),
    }
}

struct Fixture {
    owners: Vec<u8>,
    live: Vec<u8>,
    terms: [Vec<u8>; 2],
}

impl Fixture {
    fn new(segment: u64, docs: &[Member], terms: [&[Member]; 2]) -> Self {
        let mut owners = vec![0; HEADER_BYTES + docs.len() * 16];
        encode_members(key(segment), docs, &mut owners).unwrap();
        let map = Members::open(&owners).unwrap();
        let mut live = vec![0; 20_000];
        let len = map.encode_liveness(&mut live).unwrap();
        live.truncate(len);
        let terms = terms.map(|docs| {
            let mut bytes = vec![0; 20_000];
            let len = map.encode_posting(docs, &mut bytes).unwrap();
            bytes.truncate(len);
            bytes
        });
        Self {
            owners,
            live,
            terms,
        }
    }

    fn retire(&mut self, member: Member) {
        let map = Members::open(&self.owners).unwrap();
        assert!(retire(&map, &mut self.live, member).unwrap());
    }

    fn group(&self) -> SegmentGroup<'_> {
        SegmentGroup::open(&self.owners, &self.live).unwrap()
    }

    fn matches(&self, query: &[Node]) -> BTreeSet<(u64, u64)> {
        let group = self.group();
        let terms = self
            .terms
            .each_ref()
            .map(|bytes| Bitmap::open(bytes).unwrap());
        let mut matches = BTreeSet::new();
        evaluate(
            &group,
            &terms,
            query,
            &mut QueryScratch::default(),
            || Ok(()),
            |block, offsets| {
                for (word, &value) in offsets.iter().enumerate() {
                    let mut value = value;
                    while value != 0 {
                        let offset = word * 64 + value.trailing_zeros() as usize + 1;
                        let root = RootTid::new(block, offset as u16, key(1).layout()).unwrap();
                        let doc = group.members().find(root)?.unwrap();
                        matches.insert((root.key(), doc.incarnation.get()));
                        value &= value - 1;
                    }
                }
                Ok(())
            },
        )
        .unwrap();
        matches
    }
}

fn merge(sources: &[&Fixture]) -> Fixture {
    let groups: Vec<_> = sources.iter().map(|source| source.group()).collect();
    let refs: Vec<_> = groups.iter().collect();
    let plan = MergePlan::new(key(100), &refs, || Ok(())).unwrap();
    let mut owners = vec![0; HEADER_BYTES + plan.len() as usize * 16];
    plan.encode_members(&mut owners, || Ok(())).unwrap();
    let mut live = vec![0; 20_000];
    let len = plan.encode_liveness(&mut live, || Ok(())).unwrap();
    live.truncate(len);
    let terms = std::array::from_fn(|term| {
        let terms: Vec<_> = sources
            .iter()
            .map(|source| Some(Bitmap::open(&source.terms[term]).unwrap()))
            .collect();
        let mut bytes = vec![0; 20_000];
        let len = plan.encode_posting(&terms, &mut bytes, || Ok(())).unwrap();
        bytes.truncate(len);
        bytes
    });
    Fixture {
        owners,
        live,
        terms,
    }
}

#[test]
fn retired_generation_is_filtered_before_union_not_after() {
    let old = member(9, 512, 10);
    let new = member(9, 512, 11);
    let mut first = Fixture::new(1, &[old], [&[old], &[]]);
    let second = Fixture::new(2, &[new], [&[], &[new]]);
    first.retire(old);
    let merged = merge(&[&first, &second]);
    assert_eq!(merged.group().members().get(0).unwrap(), new);
    assert!(merged.matches(&[Node::Term(0)]).is_empty());
    assert_eq!(merged.matches(&[Node::Term(1)]).len(), 1);
    let query = [Node::Term(0), Node::Term(1), Node::And(0, 1)];
    assert!(merged.matches(&query).is_empty());
}

#[test]
fn conflicting_live_incarnations_and_reused_segment_ids_are_rejected() {
    let first = Fixture::new(1, &[member(9, 1, 10)], [&[], &[]]);
    let second = Fixture::new(2, &[member(9, 1, 11)], [&[], &[]]);
    let groups = [first.group(), second.group()];
    let refs = [&groups[0], &groups[1]];
    assert!(MergePlan::new(key(3), &refs, || Ok(())).is_err());
    assert!(MergePlan::new(key(1), &refs[..1], || Ok(())).is_err());
    assert!(MergePlan::new(key(3), &[&groups[0]; 2], || Ok(())).is_err());
    assert!(MergePlan::new(key(3), &[&groups[0]; 17], || Ok(())).is_err());
    let foreign = GroupKey::new(
        Generation::new(2).unwrap(),
        SegmentId::new(3).unwrap(),
        0,
        key(1).layout(),
    )
    .unwrap();
    assert!(MergePlan::new(foreign, &refs[..1], || Ok(())).is_err());
}

#[test]
fn logical_merge_preserves_full_boolean_results_and_duplicate_coverage() {
    let shared = member(63, 65, 3);
    let one = [member(0, 1, 1), member(0, 512, 2), shared];
    let two = [shared, member(64, 1, 4), member(255, 512, 5)];
    let first = Fixture::new(1, &one, [&[one[0], shared], &[one[1], shared]]);
    let mut second = Fixture::new(2, &two, [&[shared, two[2]], &[shared, two[1]]]);
    second.retire(two[2]);
    let merged = merge(&[&first, &second]);
    assert_eq!(merged.group().members().len(), 4);
    for query in [
        vec![Node::All],
        vec![Node::Term(0)],
        vec![Node::Term(0), Node::Not(0)],
        vec![Node::Term(0), Node::Term(1), Node::And(0, 1)],
        vec![Node::Term(0), Node::Term(1), Node::Or(0, 1)],
        vec![Node::Term(0), Node::Term(1), Node::Difference(0, 1)],
    ] {
        let expected = first
            .matches(&query)
            .union(&second.matches(&query))
            .copied()
            .collect();
        assert_eq!(merged.matches(&query), expected);
    }
}

#[test]
fn absent_terms_empty_sources_and_capacity_failures_are_explicit() {
    let empty = merge(&[]);
    assert!(empty.group().members().is_empty());
    assert!(empty.matches(&[Node::All]).is_empty());
    let doc = member(0, 1, 1);
    let fixture = Fixture::new(1, &[doc], [&[doc], &[]]);
    let group = fixture.group();
    let refs = [&group];
    let plan = MergePlan::new(key(2), &refs, || Ok(())).unwrap();
    assert_eq!(plan.key(), key(2));
    assert!(!plan.is_empty());
    let mut output = [0xa5; HEADER_BYTES];
    assert!(plan.encode_members(&mut output, || Ok(())).is_err());
    assert!(plan.encode_liveness(&mut output, || Ok(())).is_err());
    assert_eq!(output, [0xa5; HEADER_BYTES]);
    let term = Some(Bitmap::open(&fixture.terms[0]).unwrap());
    assert!(
        plan.encode_posting(&[term], &mut output, || Ok(()))
            .is_err()
    );
    assert!(plan.encode_posting(&[], &mut output, || Ok(())).is_err());
    assert_eq!(output, [0xa5; HEADER_BYTES]);
    let len = plan
        .encode_posting(&[None], &mut output, || Ok(()))
        .unwrap();
    assert_eq!(len, HEADER_BYTES);
    assert_eq!(*Bitmap::open(&output).unwrap().pages(), [0; 4]);
}

#[test]
fn malformed_or_foreign_term_fails_before_target_mutation() {
    let doc = member(0, 1, 1);
    let fixture = Fixture::new(1, &[doc], [&[doc], &[]]);
    let foreign = Fixture::new(2, &[doc], [&[doc], &[]]);
    let group = fixture.group();
    let refs = [&group];
    let plan = MergePlan::new(key(3), &refs, || Ok(())).unwrap();
    let mut broken = fixture.terms[0].clone();
    *broken.last_mut().unwrap() = 0;
    let mut output = [0xa5; 256];
    for bytes in [&broken, &foreign.terms[0], &fixture.live] {
        let term = Bitmap::open(bytes).unwrap();
        assert!(
            plan.encode_posting(&[Some(term)], &mut output, || Ok(()))
                .is_err()
        );
        assert_eq!(output, [0xa5; 256]);
    }
}

#[test]
fn cancellation_reaches_long_retired_runs_and_aborts_private_outputs() {
    let docs: Vec<_> = (1..=512)
        .map(|offset| member(0, offset, u64::from(offset)))
        .collect();
    let mut fixture = Fixture::new(1, &docs, [&docs, &[]]);
    for &doc in &docs {
        fixture.retire(doc);
    }
    let group = fixture.group();
    let refs = [&group];
    let mut checks = 0;
    let plan = MergePlan::new(key(2), &refs, || {
        checks += 1;
        if checks == 3 {
            Err(Error::Limit("cancelled"))
        } else {
            Ok(())
        }
    });
    assert!(matches!(plan, Err(Error::Limit("cancelled"))));
    assert_eq!(checks, 3);
    let plan = MergePlan::new(key(2), &refs, || Ok(())).unwrap();
    let mut output = [0xa5; HEADER_BYTES];
    let cancel = || Err(Error::Limit("cancelled"));
    assert_eq!(
        plan.encode_members(&mut output, cancel),
        Err(Error::Limit("cancelled"))
    );
    assert_eq!(
        plan.encode_liveness(&mut output, cancel),
        Err(Error::Limit("cancelled"))
    );
    assert_eq!(
        plan.encode_posting(&[None], &mut output, cancel),
        Err(Error::Limit("cancelled"))
    );
    assert_eq!(output, [0xa5; HEADER_BYTES]);
}
