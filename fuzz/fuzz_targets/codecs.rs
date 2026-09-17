#![no_main]

use libfuzzer_sys::fuzz_target;
use pin_core::codec::dictionary::{self, Dictionary, DictionaryLimits, TermId};
use pin_core::codec::offsets::{Encoding, OffsetSet};
use pin_core::codec::positions::{self, Positions};
use pin_core::codec::records::{self, DocumentRecord, Manifest};
use pin_core::identity::HeapLayout;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 { return; }
    let mut output = [0; 8192];
    let domain = data.get(..2).map_or(1, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]).saturating_sub(1) % 512 + 1);
    let layout = HeapLayout::new(domain).unwrap();
    let encodings = [Encoding::Sparse, Encoding::Bitmap, Encoding::Runs];
    if let Ok(set) = OffsetSet::parse(data, layout) {
        let encoding = encodings[data[4] as usize];
        let len = set.encode_as(encoding, &mut output).unwrap();
        assert_eq!(&output[..len], data);
    }
    if let Ok(positions) = Positions::parse(data, 512) {
        let values: Vec<_> = positions.iter().map(Result::unwrap).collect();
        let len = positions::encode(&values, &mut output, 512).unwrap();
        assert_eq!(&output[..len], data);
    }
    if let Ok(record) = DocumentRecord::parse(data, layout) {
        let len = record.encode(&mut output).unwrap();
        assert_eq!(&output[..len], data);
    }
    if let Ok(value) = records::decode_value(data, 4096) {
        let len = records::encode_value(value, &mut output, 4096).unwrap();
        assert_eq!(&output[..len], data);
    }
    let limits = DictionaryLimits { max_bytes: 4096, max_terms: 128, max_term_bytes: 1024 };
    if let Ok(dictionary) = Dictionary::parse(data, limits) {
        let terms: Vec<_> = (0..dictionary.len()).map(|index| dictionary.term(TermId(index)).unwrap()).collect();
        for (index, term) in terms.iter().enumerate() {
            assert_eq!(dictionary.lookup(term).unwrap(), Some(TermId(index as u32)));
            for (end, _) in term.char_indices().chain(std::iter::once((term.len(), '\0'))) {
                let prefix = &term[..end];
                let matches: Vec<_> = terms.iter().enumerate().filter(|(_, term)| term.starts_with(prefix)).map(|(index, _)| index as u32).collect();
                assert_eq!(dictionary.prefix(prefix, 128).unwrap().collect::<Vec<_>>(), matches);
            }
        }
        let len = dictionary::encode(dictionary.profile(), &terms, &mut output, limits).unwrap();
        assert_eq!(&output[..len], data);
    }
    if let Ok(manifest) = Manifest::parse(data, 128, 4096) {
        let sources: Vec<_> = (0..manifest.len()).map(|index| manifest.source(index).unwrap()).collect();
        let len = records::encode_manifest(manifest.generation, &sources, &mut output, 128).unwrap();
        assert_eq!(&output[..len], data);
    }
    let mut left = OffsetSet::new(layout);
    let mut right = OffsetSet::new(layout);
    let mut lref = [false; 513];
    let mut rref = [false; 513];
    for (index, bytes) in data.chunks_exact(2).take(512).enumerate() {
        let offset = u16::from_le_bytes([bytes[0], bytes[1]]) % domain + 1;
        if index % 2 == 0 { left.insert(offset).unwrap(); lref[offset as usize] = true; }
        else { right.insert(offset).unwrap(); rref[offset as usize] = true; }
    }
    for (set, selector) in [(&mut left, data.first()), (&mut right, data.last())] {
        let encoding = encodings[selector.copied().unwrap_or(0) as usize % 3];
        let len = set.encode_as(encoding, &mut output).unwrap();
        *set = OffsetSet::parse(&output[..len], layout).unwrap();
    }
    let union = left.union(&right).unwrap();
    let intersection = left.intersection(&right).unwrap();
    let difference = left.difference(&right).unwrap();
    for offset in 1..=domain {
        let l = lref[offset as usize]; let r = rref[offset as usize];
        assert_eq!(union.contains(offset), l || r);
        assert_eq!(intersection.contains(offset), l && r);
        assert_eq!(difference.contains(offset), l && !r);
    }
    let mut values = Vec::new();
    let mut position = 0u32;
    for &byte in data.iter().take(512) { position += u32::from(byte) + 1; values.push(position); }
    let len = positions::encode(&values, &mut output, 512).unwrap();
    assert_eq!(Positions::parse(&output[..len], 512).unwrap().iter().map(Result::unwrap).collect::<Vec<_>>(), values);
});
