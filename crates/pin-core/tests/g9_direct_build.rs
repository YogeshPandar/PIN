#[path = "support/mutable_store.rs"]
mod mutable_store;

use mutable_store::MemoryStore;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::Result;
use pin_core::identity::RootTid;
use pin_core::mutable::document::PreparedDocument;
use pin_core::mutable::grouped::{self, DirectCapture, GroupSort, SortRecord};
use pin_core::mutable::{self, PageStore};

#[derive(Default)]
struct Sort {
    rows: Vec<SortRecord>,
    position: usize,
}

impl GroupSort for Sort {
    fn put(&mut self, rows: &[SortRecord]) -> Result<()> {
        self.rows.extend_from_slice(rows);
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.rows.sort_unstable();
        Ok(())
    }

    fn read(&mut self, output: &mut [SortRecord]) -> Result<usize> {
        let count = output.len().min(self.rows.len() - self.position);
        output[..count].copy_from_slice(&self.rows[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

#[test]
fn direct_heap_capture_matches_canonical_rebuild_bytes() {
    for packed in [false, true] {
        for count in [0, 1, 300] {
            let mut reference = MemoryStore {
                packed_postings: packed,
                ..MemoryStore::default()
            };
            let mut direct = reference.clone();
            mutable::initialize(&mut reference).unwrap();
            mutable::initialize(&mut direct).unwrap();
            let mut capture = DirectCapture::new(Sort::default());
            for index in 0..count {
                let root =
                    RootTid::new(index / 40 + 1, (index % 40 + 1) as u16, direct.layout()).unwrap();
                let text = format!(
                    "shared item{} {}",
                    index % 9,
                    if index % 2 == 0 { "even" } else { "odd" }
                );
                let analyzed = Analyzed::analyze(&text, AnalysisLimits::default()).unwrap();
                let document = PreparedDocument::prepare(&analyzed, 8 << 20).unwrap();
                mutable::insert(&mut reference, root, &document).unwrap();
                mutable::insert_with_emit(
                    &mut direct,
                    root,
                    &document,
                    &mut |term, root, owner| capture.emit(term, root, owner),
                )
                .unwrap();
            }
            assert_eq!(reference.pages, direct.pages);
            let expected = grouped::rebuild(&mut reference, &mut Sort::default(), 8 << 20).unwrap();
            let actual = capture.publish(&mut direct, 8 << 20).unwrap();
            assert_eq!(expected, actual);
            assert_eq!(reference.pages, direct.pages);
        }
    }
}
