use super::build_record::TermSortRecord;
use crate::error::{Error, Result};
use crate::identity::{HeapLayout, RootTid};
use crate::mutable::document::MAX_TERM_BYTES;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReduceWork {
    pub records: u64,
    pub duplicate_roots: u64,
    pub page_runs: u64,
}

/// consumes sorted term/root pairs with at most one heap page of offsets resident.
pub struct BuildReducer {
    layout: HeapLayout,
    term: String,
    base: u32,
    page: u8,
    offsets: [u16; 512],
    len: usize,
    last_root: Option<RootTid>,
    failed: bool,
    work: ReduceWork,
}

impl BuildReducer {
    pub fn new(layout: HeapLayout) -> Result<Self> {
        let mut term = String::new();
        term.try_reserve_exact(MAX_TERM_BYTES)
            .map_err(|_| Error::Allocation)?;
        Ok(Self {
            layout,
            term,
            base: 0,
            page: 0,
            offsets: [0; 512],
            len: 0,
            last_root: None,
            failed: false,
            work: ReduceWork::default(),
        })
    }

    fn flush(&mut self, emit: &mut impl FnMut(&str, u32, u8, &[u16]) -> Result<()>) -> Result<()> {
        if self.len != 0 {
            emit(&self.term, self.base, self.page, &self.offsets[..self.len])?;
            self.work.page_runs += 1;
            self.len = 0;
        }
        Ok(())
    }

    pub fn push(
        &mut self,
        record: TermSortRecord<'_>,
        mut emit: impl FnMut(&str, u32, u8, &[u16]) -> Result<()>,
    ) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidState);
        }
        let result = self.push_inner(record, &mut emit);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(
        &mut self,
        record: TermSortRecord<'_>,
        emit: &mut impl FnMut(&str, u32, u8, &[u16]) -> Result<()>,
    ) -> Result<()> {
        if record.term.is_empty()
            || record.term.len() > MAX_TERM_BYTES
            || record.term.as_bytes().contains(&0)
            || RootTid::new(record.root.block(), record.root.offset(), self.layout).is_err()
        {
            return Err(Error::InvalidParameters);
        }
        self.work.records = self
            .work
            .records
            .checked_add(1)
            .ok_or(Error::InvalidState)?;
        match record.term.cmp(&self.term) {
            std::cmp::Ordering::Less if !self.term.is_empty() => return Err(Error::InvalidState),
            std::cmp::Ordering::Equal => {
                if let Some(last) = self.last_root {
                    if record.root < last {
                        return Err(Error::InvalidState);
                    }
                    if record.root == last {
                        self.work.duplicate_roots += 1;
                        return Ok(());
                    }
                }
            }
            _ => {
                self.flush(emit)?;
                self.term.clear();
                self.term.push_str(record.term);
                self.last_root = None;
            }
        }
        let base = record.root.block() & !255;
        let page = record.root.block() as u8;
        if self.len != 0 && (self.base != base || self.page != page) {
            self.flush(emit)?;
        }
        self.base = base;
        self.page = page;
        if self.len >= usize::from(self.layout.max_offset()) {
            return Err(Error::InvalidState);
        }
        self.offsets[self.len] = record.root.offset();
        self.len += 1;
        self.last_root = Some(record.root);
        Ok(())
    }

    pub fn finish(
        mut self,
        mut emit: impl FnMut(&str, u32, u8, &[u16]) -> Result<()>,
    ) -> Result<ReduceWork> {
        if self.failed {
            return Err(Error::InvalidState);
        }
        self.flush(&mut emit)?;
        Ok(self.work)
    }
}
