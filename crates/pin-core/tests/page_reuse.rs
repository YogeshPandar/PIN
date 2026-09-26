use pin_core::error::Error;
use pin_core::identity::HeapLayout;
use pin_core::mutable::page::{CAPACITY, NO_BLOCK, Page, PageKind};

#[test]
fn reload_reuses_address_and_matches_fresh_reads_across_page_kinds() {
    let layout = HeapLayout::new(291).unwrap();
    let mut page = Page::owners(1).unwrap();
    let address = page.bytes().as_ptr();
    for source in [
        Page::metadata(layout).unwrap(),
        Page::owners(2).unwrap(),
        Page::free(3, NO_BLOCK).unwrap(),
    ] {
        let bytes = source.bytes();
        page.reload_with(source.block(), |out| {
            assert_eq!(out.as_ptr(), address);
            assert!(out.iter().all(|&byte| byte == 0));
            out[..bytes.len()].copy_from_slice(bytes);
            Ok(bytes.len())
        })
        .unwrap();
        page.validate(layout).unwrap();
        assert_eq!(page.bytes(), bytes);
        assert_eq!(page.kind(), source.kind());
        assert!(!page.initializes_storage());
    }
    page.reload_with(4, |_| Ok(0)).unwrap();
    assert_eq!(page.kind(), PageKind::Zero);
    assert!(page.bytes().is_empty());
}

#[test]
fn failed_reload_invalidates_image_and_later_valid_read_recovers() {
    let source = Page::owners(2).unwrap();
    let mut page = source.clone();
    for mode in 0..4 {
        let result = page.reload_with(2, |out| match mode {
            0 => {
                out[0] = 9;
                Err(Error::InvalidParameters)
            }
            1 => Ok(CAPACITY + 1),
            2 => {
                out[..4].copy_from_slice(b"BAD!");
                Ok(16)
            }
            _ => {
                out[..source.bytes().len()].copy_from_slice(source.bytes());
                out[8] = 3;
                Ok(source.bytes().len())
            }
        });
        assert!(result.is_err());
        assert_eq!(page.block(), NO_BLOCK);
        assert!(page.bytes().is_empty());
    }
    assert!(
        page.reload_with(NO_BLOCK, |_| panic!("invalid block must not read"))
            .is_err()
    );
    page.reload_with(2, |out| {
        out[..source.bytes().len()].copy_from_slice(source.bytes());
        Ok(source.bytes().len())
    })
    .unwrap();
    assert_eq!(page.bytes(), source.bytes());
}
