#![no_main]

use libfuzzer_sys::fuzz_target;
use pin_core::analysis::{AnalysisLimits, Analyzed};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

fuzz_target!(|data: &[u8]| {
    if data.len() > 4096 {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let limits = AnalysisLimits {
        input_bytes: 4096,
        normalized_bytes: 16_384,
        tokens: 8192,
        term_bytes: 16_384,
        memory_bytes: 1 << 20,
    };
    let actual = Analyzed::analyze(text, limits).unwrap();
    let expected: String = text
        .nfc()
        .map(|value| {
            unicode_case_mapping::case_folded(value)
                .map_or(value, |mapped| char::from_u32(mapped.get()).unwrap())
        })
        .nfc()
        .collect();
    assert_eq!(actual.normalized(), expected);
    assert_eq!(
        actual.tokens().map(|token| token.term).collect::<Vec<_>>(),
        expected.unicode_words().collect::<Vec<_>>()
    );
    assert!(actual.peak_bytes() <= limits.memory_bytes);
    assert!(actual.retained_bytes() <= actual.peak_bytes());
    for (index, token) in actual.tokens().enumerate() {
        assert_eq!(token.position, index as u32);
    }
    let small = AnalysisLimits {
        memory_bytes: data.first().copied().unwrap_or(0) as usize,
        ..limits
    };
    if let Ok(bounded) = Analyzed::analyze(text, small) {
        assert_eq!(bounded.normalized(), actual.normalized());
        assert!(bounded.peak_bytes() <= small.memory_bytes);
    }
});
