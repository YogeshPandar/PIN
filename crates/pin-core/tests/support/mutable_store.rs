use pin_core::error::{Error, Result};
use pin_core::identity::HeapLayout;
use pin_core::mutable::page::{MAX_WAL_PAGES, Page};
use pin_core::mutable::{PageStore, Stage};

#[derive(Clone, Default)]
pub struct MemoryStore {
    pub pages: Vec<Vec<u8>>,
    pub direct_documents: bool,
    pub events: Vec<Stage>,
    pub fail_at: Option<usize>,
}

impl PageStore for MemoryStore {
    fn direct_documents(&self) -> bool {
        self.direct_documents
    }
    fn layout(&self) -> HeapLayout {
        HeapLayout::new(291).unwrap()
    }

    fn blocks(&mut self) -> Result<u32> {
        Ok(self.pages.len() as u32)
    }

    fn read(&mut self, block: u32) -> Result<Page> {
        let bytes = self.pages.get(block as usize).ok_or(Error::InvalidState)?;
        Page::read_with(block, |output| {
            if bytes.len() > output.len() {
                return Err(Error::InvalidState);
            }
            output[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }

    fn read_into(&mut self, block: u32, page: &mut Page) -> Result<()> {
        let bytes = self.pages.get(block as usize).ok_or(Error::InvalidState)?;
        page.reload_with(block, |output| {
            if bytes.len() > output.len() {
                return Err(Error::InvalidState);
            }
            output[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
    }

    fn extend(&mut self) -> Result<u32> {
        let block = self.pages.len() as u32;
        self.pages.push(Vec::new());
        Ok(block)
    }

    fn commit(&mut self, pages: &[&Page]) -> Result<()> {
        assert!(!pages.is_empty() && pages.len() <= MAX_WAL_PAGES);
        for (index, page) in pages.iter().enumerate() {
            page.validate(self.layout())?;
            assert!((page.block() as usize) < self.pages.len());
            assert!(
                pages[..index]
                    .iter()
                    .all(|other| other.block() != page.block())
            );
        }
        // one test-store commit represents one indivisible generic WAL replay record.
        let copies: Vec<_> = pages
            .iter()
            .map(|page| (page.block(), page.bytes().to_vec()))
            .collect();
        for (block, bytes) in copies {
            self.pages[block as usize] = bytes;
        }
        Ok(())
    }

    fn remove_owners(&mut self, page: &Page) -> Result<()> {
        // this store has no concurrent readers or host buffer pins.
        self.commit(&[page])
    }

    fn event(&mut self, stage: Stage) -> Result<()> {
        self.events.push(stage);
        if self.fail_at == Some(self.events.len() - 1) {
            return Err(Error::InvalidState);
        }
        Ok(())
    }
}
