//! Streaming replacement of posting chains under a host reader/writer barrier.
//! Dictionary/boundary swaps publish output; journal pages are never searchable.
//! Canonical owners retain all liveness and incarnation authority.
//! PG18 WAL and Rust 1.98.1 contracts are recorded in docs/api-evidence.md.

use super::page::{
    BUCKETS, NO_BLOCK, OwnerRef, Page, PageKind, RewriteJournal, RewritePhase, SealedBuilder,
    TermRef,
};
use super::reader::resolve;
use super::{PageStore, Stage, allocate, following, load, load_posting, posting_next};
use crate::error::{Error, Result};

/// Selects the reference rewrite or reader-quiescent sealed-prefix retention.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CompactMode {
    #[default]
    Copy,
    RetainSealedPrefix,
    DirectTid,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CompactStats {
    pub rewritten_terms: u64,
    pub removed_postings: u64,
    pub written_pages: u32,
    pub reused_pages: u32,
    pub reclaimed_pages: u32,
    /// Existing sealed pages kept reachable, not allocations from the free list.
    pub retained_pages: u32,
}

#[derive(Default)]
struct ChainStats {
    mutable_pages: u32,
    indirect_pages: u32,
    dead: u64,
    prefix: Option<RetainedPrefix>,
}

#[derive(Clone, Copy)]
struct RetainedPrefix {
    boundary: u32,
    rewrite_head: u32,
    pages: u32,
}

/// Rewrites mutable or deletion-bearing chains into sealed posting payloads.
///
/// The host must exclude writers and all readers before entry and retain both
/// barriers until return. Scratch is a fixed number of page images, independent
/// of relation size. Interrupts are checked during every page traversal.
///
/// # Errors
/// Rejects corrupt chains, unordered owners and any host failure. Completed term
/// replacements remain durable on failure. Recovery discards unpublished output
/// or finishes retirement; it never rolls back an already published replacement.
pub fn compact<S: PageStore>(store: &mut S) -> Result<CompactStats> {
    compact_with_mode(store, CompactMode::Copy)
}

/// Compacts with optional retention of an entirely live sealed prefix.
///
/// Retention rewrites the last live sealed page with the suffix to coalesce small
/// appends. It does not share extents between readers or skip liveness inspection.
/// Both modes require the same exclusive reader/writer barriers as [`compact`].
/// Scratch remains a fixed number of page images, with no heap-sized work list.
///
/// # Errors
/// Has the same failure/recovery contract as [`compact`]. A retained boundary is
/// revalidated before one atomic dictionary, boundary and journal publication.
pub fn compact_with_mode<S: PageStore>(store: &mut S, mode: CompactMode) -> Result<CompactStats> {
    let mut stats = CompactStats {
        reclaimed_pages: recover(store)?,
        ..CompactStats::default()
    };
    let mut meta = load(store, 0, PageKind::Meta)?;
    for bucket in 0..BUCKETS {
        store.interrupt()?;
        let (head, tail) = meta.bucket(bucket)?;
        if head == NO_BLOCK {
            continue;
        }
        let mut block = head;
        loop {
            let dictionary = load(store, block, PageKind::Dictionary)?;
            for entry in dictionary.terms()? {
                let entry = entry?;
                if super::page::bucket_for(entry.term) != bucket {
                    return Err(Error::InvalidState);
                }
                if entry.head == NO_BLOCK {
                    continue;
                }
                if let Some(second) = entry.inline_second {
                    let mut cache = None;
                    if super::reader::resolve(store, &mut cache, second)?.is_none() {
                        super::grouped::invalidate_frontier(store, &mut meta)?;
                        let mut current = load(store, block, PageKind::Dictionary)?;
                        if current.term(entry.reference)?.inline_second != Some(second) {
                            return Err(Error::InvalidState);
                        }
                        current.set_inline_second(entry.reference, None)?;
                        store.commit(&[&current])?;
                        stats.removed_postings += 1;
                        stats.rewritten_terms += 1;
                    }
                    continue;
                }
                let summary = inspect(
                    store,
                    entry.reference,
                    entry.head,
                    entry.tail,
                    Some(entry.first),
                    true,
                )?;
                if summary.mutable_pages == 0
                    && summary.dead == 0
                    && !(mode == CompactMode::DirectTid && summary.indirect_pages != 0)
                {
                    continue;
                }
                let prefix = match mode {
                    CompactMode::Copy | CompactMode::DirectTid => None,
                    CompactMode::RetainSealedPrefix => summary.prefix,
                };
                super::grouped::invalidate_frontier(store, &mut meta)?;
                let (written, reused) = rewrite(
                    store,
                    &mut meta,
                    entry.reference,
                    entry.head,
                    entry.tail,
                    prefix,
                    mode == CompactMode::DirectTid,
                )?;
                stats.retained_pages = stats
                    .retained_pages
                    .checked_add(prefix.map_or(0, |prefix| prefix.pages))
                    .ok_or(Error::Limit("compaction pages"))?;
                stats.written_pages = stats
                    .written_pages
                    .checked_add(written)
                    .ok_or(Error::Limit("compaction pages"))?;
                stats.reused_pages = stats
                    .reused_pages
                    .checked_add(reused)
                    .ok_or(Error::Limit("compaction pages"))?;
                stats.removed_postings = stats
                    .removed_postings
                    .checked_add(summary.dead)
                    .ok_or(Error::Limit("compaction postings"))?;
                stats.rewritten_terms += 1;
                stats.reclaimed_pages = stats
                    .reclaimed_pages
                    .checked_add(recover(store)?)
                    .ok_or(Error::Limit("compaction pages"))?;
                // recovery advances the free list; never reuse a stale metapage image.
                meta = load(store, 0, PageKind::Meta)?;
            }
            match following(&dictionary, tail)? {
                Some(next) => block = next,
                None => break,
            }
        }
    }
    Ok(stats)
}

// validates complete, terminal chains before publishing or reclaiming any page.
fn inspect<S: PageStore>(
    store: &mut S,
    term: TermRef,
    head: u32,
    tail: u32,
    mut previous: Option<OwnerRef>,
    liveness: bool,
) -> Result<ChainStats> {
    let mut stats = ChainStats::default();
    if head == NO_BLOCK {
        if tail != NO_BLOCK {
            return Err(Error::InvalidState);
        }
        return Ok(stats);
    }
    let mut remaining = store.blocks()?;
    let mut block = head;
    let mut cache = None;
    let mut prefix_open = liveness;
    let mut prefix_tail = None;
    let mut prefix_pages = 0u32;
    loop {
        let page = load_posting(store, block, term)?;
        stats.mutable_pages += u32::from(page.kind() == PageKind::Postings);
        stats.indirect_pages += u32::from(page.kind() != PageKind::DirectPostings);
        let mut page_live = page.kind() == PageKind::SealedPostings;
        for reference in page.posting_refs()? {
            let reference = reference?;
            if previous.is_some_and(|previous| {
                reference.page < previous.page
                    || (reference.page == previous.page && reference.slot <= previous.slot)
                    || reference.incarnation.get() <= previous.incarnation.get()
            }) {
                return Err(Error::InvalidState);
            }
            previous = Some(reference);
            if liveness && resolve(store, &mut cache, reference)?.is_none() {
                stats.dead += 1;
                page_live = false;
            }
        }
        if prefix_open && page_live {
            // leave the last live sealed page in the suffix to coalesce its tail.
            stats.prefix = prefix_tail.map(|boundary| RetainedPrefix {
                boundary,
                rewrite_head: block,
                pages: prefix_pages,
            });
            prefix_tail = Some(block);
            prefix_pages = prefix_pages
                .checked_add(1)
                .ok_or(Error::Limit("compaction pages"))?;
        } else {
            prefix_open = false;
        }
        match posting_next(&page, tail, &mut remaining)? {
            Some(next) => block = next,
            None => {
                if page.next()? != NO_BLOCK {
                    return Err(Error::InvalidState);
                }
                return Ok(stats);
            }
        }
    }
}

fn rewrite<S: PageStore>(
    store: &mut S,
    meta: &mut Page,
    term: TermRef,
    head: u32,
    tail: u32,
    prefix: Option<RetainedPrefix>,
    direct: bool,
) -> Result<(u32, u32)> {
    if meta.rewrite_journal()?.is_some() {
        return Err(Error::InvalidState);
    }
    let rewrite_head = prefix.map_or(head, |prefix| prefix.rewrite_head);
    let mut remaining = store.blocks()?;
    let mut block = rewrite_head;
    let mut cache = None;
    let mut output: Option<SealedBuilder> = None;
    let mut written = 0u32;
    let mut reused = 0u32;
    loop {
        let page = load_posting(store, block, term)?;
        for reference in page.posting_refs()? {
            let reference = reference?;
            let Some(root) = resolve(store, &mut cache, reference)? else {
                continue;
            };
            if let Some(builder) = output.as_mut() {
                if if direct {
                    builder.push_direct(reference, root)?
                } else {
                    builder.push(reference)?
                } {
                    continue;
                }
                let full = output.take().ok_or(Error::InvalidState)?.finish()?;
                persist_output(store, meta, full)?;
                written += 1;
            }
            reused += u32::from(meta.free_head()? != NO_BLOCK);
            let block = output_block(store, meta)?;
            let mut builder = if direct {
                SealedBuilder::new_direct(block, term)?
            } else {
                SealedBuilder::new(block, term)?
            };
            if !(if direct {
                builder.push_direct(reference, root)?
            } else {
                builder.push(reference)?
            }) {
                return Err(Error::InvalidState);
            }
            output = Some(builder);
        }
        match posting_next(&page, tail, &mut remaining)? {
            Some(next) => block = next,
            None => break,
        }
    }
    if let Some(builder) = output {
        persist_output(store, meta, builder.finish()?)?;
        written += 1;
    }
    let replacement = meta.rewrite_journal()?;
    let mut dictionary = load(store, term.page, PageKind::Dictionary)?;
    let current = dictionary.term(term)?;
    if (current.head, current.tail) != (head, tail) {
        return Err(Error::InvalidState);
    }
    let (new_head, new_tail) = match replacement {
        Some(journal) if journal.phase == RewritePhase::Building => (journal.head, journal.tail),
        None => (NO_BLOCK, NO_BLOCK),
        _ => return Err(Error::InvalidState),
    };
    meta.set_rewrite_journal(Some(RewriteJournal {
        head: rewrite_head,
        tail,
        phase: RewritePhase::Retiring,
    }))?;
    if let Some(prefix) = prefix {
        let mut boundary = load_posting(store, prefix.boundary, term)?;
        if boundary.kind() != PageKind::SealedPostings || boundary.next()? != rewrite_head {
            return Err(Error::InvalidState);
        }
        boundary.set_next(new_head)?;
        let tail = if new_tail == NO_BLOCK {
            prefix.boundary
        } else {
            new_tail
        };
        dictionary.set_posting_chain(term, head, tail)?;
        // the prefix stays owned; only its old suffix enters the retirement journal.
        store.commit(&[meta, &dictionary, &boundary])?;
    } else {
        dictionary.set_posting_chain(term, new_head, new_tail)?;
        // one WAL record switches coverage and records the complete retired source.
        store.commit(&[meta, &dictionary])?;
    }
    store.event(Stage::ReplacementPublished)?;
    Ok((written, reused))
}

// free-list removal becomes durable only with the new output page and journal.
fn output_block<S: PageStore>(store: &mut S, meta: &mut Page) -> Result<u32> {
    let free = meta.free_head()?;
    if free == NO_BLOCK {
        allocate(store)
    } else {
        let page = load(store, free, PageKind::Free)?;
        meta.set_free_head(page.next()?)?;
        Ok(free)
    }
}

fn persist_output<S: PageStore>(store: &mut S, meta: &mut Page, page: Page) -> Result<()> {
    match meta.rewrite_journal()? {
        None => {
            meta.set_rewrite_journal(Some(RewriteJournal {
                head: page.block(),
                tail: page.block(),
                phase: RewritePhase::Building,
            }))?;
            store.commit(&[meta, &page])?;
        }
        Some(journal) if journal.phase == RewritePhase::Building => {
            let mut previous = load_posting(store, journal.tail, page.posting_term()?)?;
            if previous.next()? != NO_BLOCK
                || !matches!(
                    previous.kind(),
                    PageKind::SealedPostings | PageKind::DirectPostings
                )
            {
                return Err(Error::InvalidState);
            }
            previous.set_next(page.block())?;
            meta.set_rewrite_journal(Some(RewriteJournal {
                tail: page.block(),
                ..journal
            }))?;
            store.commit(&[meta, &previous, &page])?;
        }
        _ => return Err(Error::InvalidState),
    }
    store.event(Stage::SegmentStored)
}

/// Reclaims journal-owned pages after an interrupted build or publication.
///
/// The host holds its writer interlock. A retiring journal can only be installed
/// after reader quiescence; later readers can capture only the replacement chain.
/// No reader barrier is needed for an unpublished output chain.
///
/// # Errors
/// Fails closed if the journal overlaps the active chain, has a nonterminal tail,
/// crosses term identities, contains a cycle, or any host operation fails.
pub fn recover<S: PageStore>(store: &mut S) -> Result<u32> {
    let mut meta = load(store, 0, PageKind::Meta)?;
    let Some(journal) = meta.rewrite_journal()? else {
        return Ok(0);
    };
    if meta
        .grouped_state()?
        .active
        .is_some_and(|active| active.frontier_valid)
    {
        return Err(Error::InvalidState);
    }
    let first = super::load_any(store, journal.head)?;
    let term = first.posting_term()?;
    let dictionary = load(store, term.page, PageKind::Dictionary)?;
    let active = dictionary.term(term)?;
    inspect(store, term, journal.head, journal.tail, None, false)?;
    inspect(
        store,
        term,
        active.head,
        active.tail,
        Some(active.first),
        false,
    )?;
    // two finite terminal singly linked chains intersect iff they share a tail.
    if active.tail == journal.tail {
        return Err(Error::InvalidState);
    }
    let mut reclaimed = 0u32;
    let mut remaining = store.blocks()?;
    let mut block = journal.head;
    loop {
        let page = load_posting(store, block, term)?;
        let next = posting_next(&page, journal.tail, &mut remaining)?;
        let free = Page::free(block, meta.free_head()?)?;
        let Some(second_block) = next else {
            meta.set_rewrite_journal(None)?;
            meta.set_free_head(block)?;
            store.commit(&[&meta, &free])?;
            reclaimed += 1;
            store.event(Stage::SegmentReclaimed)?;
            return Ok(reclaimed);
        };

        let second = load_posting(store, second_block, term)?;
        let next = posting_next(&second, journal.tail, &mut remaining)?;
        let second_free = Page::free(second_block, block)?;
        meta.set_rewrite_journal(next.map(|head| RewriteJournal { head, ..journal }))?;
        meta.set_free_head(second_block)?;
        // two retired pages plus the journal fit one atomic three-page WAL batch.
        store.commit(&[&meta, &free, &second_free])?;
        reclaimed += 2;
        store.event(Stage::SegmentReclaimed)?;
        match next {
            Some(next) => block = next,
            None => return Ok(reclaimed),
        }
    }
}
