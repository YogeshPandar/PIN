use super::{Extent, PageDescriptor, PrimaryArena, encode_directory, encode_offsets};
use crate::error::{Error, Result};
use crate::grouped::GroupKey;
use crate::mutable::{
    PageStore,
    page::{NO_BLOCK, PRIMARY_PAYLOAD_BYTES},
};

// At least four bytes are needed for every non-singleton offsets payload.
// This holds one full posting page worth of group directories in the worst case.
const MAX_QUEUED_DIRECTORIES: usize = PRIMARY_PAYLOAD_BYTES / 4;
const MAX_DIRECTORY_CALLBACKS: usize = 107;

/// writes sorted reducer runs into packed immutable v2 posting and directory pages.
///
/// Callbacks receive only durable directory extents and run with no outstanding
/// extension. This does not publish a manifest or reclaim partial output.
pub struct PrimaryBuildWriter<'a, S: PageStore> {
    store: &'a mut S,
    relation: crate::identity::Generation,
    segment: crate::identity::SegmentId,
    term: String,
    term_id: u64,
    last_base: Option<u32>,
    last_page: Option<u8>,
    key: Option<GroupKey>,
    entries: Vec<PageDescriptor>,
    posting_arena: Option<PrimaryArena>,
    posting_block: u32,
    directory_arena: Option<PrimaryArena>,
    directory_block: u32,
    queued_directories: Vec<QueuedDirectory>,
    callbacks: Vec<DirectoryCallback>,
    failed: bool,
}

struct QueuedDirectory {
    term: String,
    key: GroupKey,
    term_id: u64,
    bytes: Vec<u8>,
}

struct DirectoryCallback {
    term: String,
    key: GroupKey,
    term_id: u64,
    extent: Extent,
}

impl<'a, S: PageStore> PrimaryBuildWriter<'a, S> {
    /// requires an initialized, empty v2 relation whose root owns block zero.
    pub fn new(
        store: &'a mut S,
        relation: crate::identity::Generation,
        segment: crate::identity::SegmentId,
    ) -> Result<Self> {
        let root = super::read_root(store, relation)?;
        if root.layout != store.layout()
            || root.segment_count != 0
            || root.manifest_block != NO_BLOCK
        {
            return Err(Error::InvalidState);
        }
        let mut term = String::new();
        term.try_reserve_exact(crate::mutable::document::MAX_TERM_BYTES)
            .map_err(|_| Error::Allocation)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(256)
            .map_err(|_| Error::Allocation)?;
        let mut queued_directories = Vec::new();
        queued_directories
            .try_reserve_exact(MAX_QUEUED_DIRECTORIES)
            .map_err(|_| Error::Allocation)?;
        let mut callbacks = Vec::new();
        callbacks
            .try_reserve_exact(MAX_DIRECTORY_CALLBACKS)
            .map_err(|_| Error::Allocation)?;
        Ok(Self {
            store,
            relation,
            segment,
            term,
            term_id: 0,
            last_base: None,
            last_page: None,
            key: None,
            entries,
            posting_arena: None,
            posting_block: NO_BLOCK,
            directory_arena: None,
            directory_block: NO_BLOCK,
            queued_directories,
            callbacks,
            failed: false,
        })
    }

    /// accepts one sorted run emitted by `BuildReducer`.
    ///
    /// The callback may allocate and commit through the supplied store. It is
    /// called only after the directory extent's containing page is committed.
    pub fn push(
        &mut self,
        term: &str,
        base: u32,
        page: u8,
        offsets: &[u16],
        mut emit: impl FnMut(&mut S, &str, GroupKey, u64, Extent) -> Result<()>,
    ) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidState);
        }
        let result = self.push_inner(term, base, page, offsets, &mut emit);
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn push_inner(
        &mut self,
        term: &str,
        base: u32,
        page: u8,
        offsets: &[u16],
        emit: &mut impl FnMut(&mut S, &str, GroupKey, u64, Extent) -> Result<()>,
    ) -> Result<()> {
        if term.is_empty()
            || term.len() > crate::mutable::document::MAX_TERM_BYTES
            || term.as_bytes().contains(&0)
        {
            return Err(Error::InvalidParameters);
        }
        let key = GroupKey::new(self.relation, self.segment, base, self.store.layout())?;
        let ordering = if self.term.is_empty() {
            std::cmp::Ordering::Greater
        } else {
            term.cmp(&self.term)
        };
        if ordering == std::cmp::Ordering::Less {
            return Err(Error::InvalidState);
        }
        if ordering == std::cmp::Ordering::Greater {
            self.finish_group(emit)?;
            self.term_id = self.term_id.checked_add(1).ok_or(Error::InvalidState)?;
            self.term.clear();
            self.term.push_str(term);
            self.last_base = None;
            self.last_page = None;
        }
        if self.last_base.is_some_and(|old| base < old)
            || (self.last_base == Some(base) && self.last_page.is_some_and(|old| page <= old))
        {
            return Err(Error::InvalidState);
        }
        if self.key.is_some_and(|old| old != key) {
            self.finish_group(emit)?;
        }
        let (kind, payload) = encode_offsets(self.store.layout().max_offset(), offsets)?;
        let descriptor = if offsets.len() == 1 {
            PageDescriptor::singleton(page, offsets[0])
        } else {
            let extent = self.push_posting_payload(&payload, emit)?;
            PageDescriptor {
                page,
                kind,
                count: u16::try_from(offsets.len()).map_err(|_| Error::InvalidState)?,
                extent,
            }
        };
        if self.entries.len() >= 256 {
            return Err(Error::InvalidState);
        }
        self.entries.push(descriptor);
        self.key = Some(key);
        self.last_base = Some(base);
        self.last_page = Some(page);
        Ok(())
    }

    fn push_posting_payload(
        &mut self,
        payload: &[u8],
        emit: &mut impl FnMut(&mut S, &str, GroupKey, u64, Extent) -> Result<()>,
    ) -> Result<Extent> {
        if payload.is_empty() || payload.len() > PRIMARY_PAYLOAD_BYTES {
            return Err(Error::InvalidParameters);
        }
        if self.posting_arena.is_none() {
            self.flush_queued_directories(emit)?;
            self.flush_directory_page(emit)?;
            self.start_posting_arena()?;
        }
        if let Some(extent) = self
            .posting_arena
            .as_mut()
            .ok_or(Error::InvalidState)?
            .try_push(payload)?
        {
            return Ok(extent);
        }
        self.flush_posting_page()?;
        self.flush_queued_directories(emit)?;
        self.start_posting_arena()?;
        self.posting_arena
            .as_mut()
            .ok_or(Error::InvalidState)?
            .try_push(payload)?
            .ok_or(Error::InvalidState)
    }

    fn allocate_block(&mut self) -> Result<u32> {
        self.store.interrupt()?;
        let expected = self.store.blocks()?;
        let block = self.store.extend()?;
        if block != expected || block == 0 || block == NO_BLOCK {
            return Err(Error::InvalidState);
        }
        Ok(block)
    }

    fn start_posting_arena(&mut self) -> Result<()> {
        let block = self.allocate_block()?;
        self.posting_arena = Some(PrimaryArena::new(block)?);
        self.posting_block = block;
        Ok(())
    }

    fn flush_posting_page(&mut self) -> Result<()> {
        let Some(arena) = self.posting_arena.take() else {
            return Ok(());
        };
        let page = arena.finish()?;
        if page.block() != self.posting_block {
            return Err(Error::InvalidState);
        }
        self.store.commit(&[&page])?;
        self.posting_block = NO_BLOCK;
        Ok(())
    }

    fn finish_group(
        &mut self,
        emit: &mut impl FnMut(&mut S, &str, GroupKey, u64, Extent) -> Result<()>,
    ) -> Result<()> {
        let Some(key) = self.key.take() else {
            return Ok(());
        };
        if self.queued_directories.len() == MAX_QUEUED_DIRECTORIES {
            self.flush_posting_page()?;
            self.flush_queued_directories(emit)?;
        }
        let bytes = encode_directory(key, self.term_id, &self.entries)?;
        let mut term = String::new();
        term.try_reserve_exact(self.term.len())
            .map_err(|_| Error::Allocation)?;
        term.push_str(&self.term);
        self.queued_directories.push(QueuedDirectory {
            term,
            key,
            term_id: self.term_id,
            bytes,
        });
        self.entries.clear();
        Ok(())
    }

    fn flush_queued_directories(
        &mut self,
        emit: &mut impl FnMut(&mut S, &str, GroupKey, u64, Extent) -> Result<()>,
    ) -> Result<()> {
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(MAX_QUEUED_DIRECTORIES)
            .map_err(|_| Error::Allocation)?;
        let queued = std::mem::replace(&mut self.queued_directories, replacement);
        for directory in queued {
            let bytes = &directory.bytes;
            if self.directory_arena.is_none() {
                let block = self.allocate_block()?;
                self.directory_arena = Some(PrimaryArena::new(block)?);
                self.directory_block = block;
            }
            let extent = match self
                .directory_arena
                .as_mut()
                .ok_or(Error::InvalidState)?
                .try_push(bytes)?
            {
                Some(extent) => extent,
                None => {
                    self.flush_directory_page(emit)?;
                    let block = self.allocate_block()?;
                    self.directory_arena = Some(PrimaryArena::new(block)?);
                    self.directory_block = block;
                    self.directory_arena
                        .as_mut()
                        .ok_or(Error::InvalidState)?
                        .try_push(bytes)?
                        .ok_or(Error::InvalidState)?
                }
            };
            let mut term = String::new();
            term.try_reserve_exact(directory.term.len())
                .map_err(|_| Error::Allocation)?;
            term.push_str(&directory.term);
            self.callbacks.push(DirectoryCallback {
                term,
                key: directory.key,
                term_id: directory.term_id,
                extent,
            });
        }
        self.queued_directories.clear();
        Ok(())
    }

    fn flush_directory_page(
        &mut self,
        emit: &mut impl FnMut(&mut S, &str, GroupKey, u64, Extent) -> Result<()>,
    ) -> Result<()> {
        let Some(arena) = self.directory_arena.take() else {
            return Ok(());
        };
        let page = arena.finish()?;
        if page.block() != self.directory_block {
            return Err(Error::InvalidState);
        }
        self.store.commit(&[&page])?;
        self.directory_block = NO_BLOCK;
        for callback in self.callbacks.drain(..) {
            emit(
                self.store,
                &callback.term,
                callback.key,
                callback.term_id,
                callback.extent,
            )?;
        }
        Ok(())
    }

    fn flush_all(
        &mut self,
        emit: &mut impl FnMut(&mut S, &str, GroupKey, u64, Extent) -> Result<()>,
    ) -> Result<()> {
        self.flush_posting_page()?;
        self.flush_queued_directories(emit)?;
        self.flush_directory_page(emit)
    }

    /// commits all remaining pages and returns the number of distinct terms.
    pub fn finish(
        mut self,
        mut emit: impl FnMut(&mut S, &str, GroupKey, u64, Extent) -> Result<()>,
    ) -> Result<u64> {
        if self.failed {
            return Err(Error::InvalidState);
        }
        let result = self
            .finish_group(&mut emit)
            .and_then(|_| self.flush_all(&mut emit));
        if result.is_err() {
            self.failed = true;
            return result.map(|_| self.term_id);
        }
        Ok(self.term_id)
    }
}
