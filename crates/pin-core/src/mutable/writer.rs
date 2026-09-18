//! Multi-record publication under the adapter's per-index writer interlock.
//! All payload bytes are prepared before entry. Only the final owner update
//! admits a document to scans; transaction visibility remains the host's job.

use super::document::PreparedDocument;
use super::page::{FRAGMENT_BYTES, INLINE_BYTES, NO_BLOCK, OwnerChange, OwnerRef, Page, PageKind};
use super::{PageStore, Stage, allocate, find_term, load};
use crate::error::{Error, Result};
use crate::identity::RootTid;

/// Creates the metapage of a new, empty relation under the writer interlock.
pub fn initialize<S: PageStore>(store: &mut S) -> Result<()> {
    if store.blocks()? != 0 || allocate(store)? != 0 {
        return Err(Error::InvalidState);
    }
    let meta = Page::metadata(store.layout())?;
    store.commit(&[&meta])
}

/// Publishes one complete document; failure can leave reclaimable preparation.
///
/// # Errors
/// Any storage, format or capacity error aborts insertion. Success means all
/// dictionary references and exact payload bytes precede the publication record.
/// This is index publication, not transaction commit or snapshot visibility.
pub fn insert<S: PageStore>(
    store: &mut S,
    root: RootTid,
    document: &PreparedDocument,
) -> Result<OwnerRef> {
    let mut meta = load(store, 0, PageKind::Meta)?;
    let owner = reserve_owner(store, &mut meta, root, document)?;
    store.event(Stage::OwnerReserved)?;
    let mut data_head = NO_BLOCK;
    if document.bytes().len() > INLINE_BYTES {
        let count = document.bytes().len().div_ceil(FRAGMENT_BYTES);
        for index in (0..count).rev() {
            let offset = index * FRAGMENT_BYTES;
            let end = (offset + FRAGMENT_BYTES).min(document.bytes().len());
            let free = meta.free_head()?;
            let block = if free == NO_BLOCK {
                allocate(store)?
            } else {
                let page = load(store, free, PageKind::Free)?;
                meta.set_free_head(page.next()?)?;
                free
            };
            let page = Page::fragment(
                block,
                owner,
                data_head,
                offset as u32,
                &document.bytes()[offset..end],
            )?;
            if free == NO_BLOCK {
                store.commit(&[&page])?;
            } else {
                // removal from the free list and new ownership are one WAL batch.
                store.commit(&[&meta, &page])?;
            }
            data_head = block;
            store.event(Stage::FragmentStored)?;
        }
    }
    let mut owners = load(store, owner.page, PageKind::Owners)?;
    owners.change_owner(owner, OwnerChange::PayloadReady(data_head), store.layout())?;
    store.commit(&[&owners])?;
    store.event(Stage::PayloadReady)?;
    for term in document.terms() {
        link_term(store, &mut meta, term?.term, owner)?;
        store.event(Stage::TermLinked)?;
    }
    // reload after all preparation; no page borrow crosses host I/O.
    let mut owners = load(store, owner.page, PageKind::Owners)?;
    owners.change_owner(owner, OwnerChange::Publish, store.layout())?;
    store.commit(&[&owners])?;
    store.event(Stage::Published)?;
    Ok(owner)
}

fn reserve_owner<S: PageStore>(
    store: &mut S,
    meta: &mut Page,
    root: RootTid,
    document: &PreparedDocument,
) -> Result<OwnerRef> {
    let incarnation = meta.reserve_incarnation()?;
    let (head, tail) = meta.owner_chain()?;
    if head == NO_BLOCK {
        let mut page = Page::owners(allocate(store)?)?;
        let owner = page
            .append_owner(
                incarnation,
                root,
                document.token_count(),
                document.term_count(),
                document.bytes(),
            )?
            .ok_or(Error::InvalidState)?;
        meta.set_owner_chain(page.block(), page.block())?;
        store.commit(&[meta, &page])?;
        return Ok(owner);
    }
    let mut page = load(store, tail, PageKind::Owners)?;
    if let Some(owner) = page.append_owner(
        incarnation,
        root,
        document.token_count(),
        document.term_count(),
        document.bytes(),
    )? {
        store.commit(&[meta, &page])?;
        return Ok(owner);
    }
    let mut next = Page::owners(allocate(store)?)?;
    let owner = next
        .append_owner(
            incarnation,
            root,
            document.token_count(),
            document.term_count(),
            document.bytes(),
        )?
        .ok_or(Error::InvalidState)?;
    page.set_next(next.block())?;
    meta.set_owner_chain(head, next.block())?;
    store.commit(&[meta, &page, &next])?;
    Ok(owner)
}

fn link_term<S: PageStore>(
    store: &mut S,
    meta: &mut Page,
    text: &str,
    owner: OwnerRef,
) -> Result<()> {
    if let Some((mut dictionary, reference)) = find_term(store, meta, text)? {
        let term = dictionary.term(reference)?;
        let (head, tail) = (term.head, term.tail);
        if head == NO_BLOCK {
            let mut page = Page::postings(allocate(store)?, reference)?;
            if !page.append_posting(owner)? {
                return Err(Error::InvalidState);
            }
            dictionary.set_posting_chain(reference, page.block(), page.block())?;
            store.commit(&[&dictionary, &page])?;
            return Ok(());
        }
        let mut page = load(store, tail, PageKind::Postings)?;
        if page.posting_term()? != reference {
            return Err(Error::InvalidState);
        }
        if page.append_posting(owner)? {
            return store.commit(&[&page]);
        }
        let mut next = Page::postings(allocate(store)?, reference)?;
        if !next.append_posting(owner)? {
            return Err(Error::InvalidState);
        }
        page.set_next(next.block())?;
        dictionary.set_posting_chain(reference, head, next.block())?;
        return store.commit(&[&dictionary, &page, &next]);
    }
    let bucket = super::page::bucket_for(text);
    let (head, tail) = meta.bucket(bucket)?;
    if head == NO_BLOCK {
        let mut page = Page::dictionary(allocate(store)?)?;
        page.append_term(text, owner)?.ok_or(Error::InvalidState)?;
        meta.set_bucket(bucket, page.block(), page.block())?;
        return store.commit(&[meta, &page]);
    }
    let mut page = load(store, tail, PageKind::Dictionary)?;
    if page.append_term(text, owner)?.is_some() {
        return store.commit(&[&page]);
    }
    let mut next = Page::dictionary(allocate(store)?)?;
    next.append_term(text, owner)?.ok_or(Error::InvalidState)?;
    page.set_next(next.block())?;
    meta.set_bucket(bucket, head, next.block())?;
    store.commit(&[meta, &page, &next])
}
