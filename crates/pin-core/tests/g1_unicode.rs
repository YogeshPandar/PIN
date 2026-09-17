use pin_core::analysis::{AnalysisLimits, Analyzed};
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

fn reference(text: &str) -> String {
    text.nfc().map(|value| unicode_case_mapping::case_folded(value).map_or(value, |mapped| char::from_u32(mapped.get()).unwrap())).nfc().collect()
}

fn compare(text: &str) {
    let limits = AnalysisLimits::default();
    let actual = Analyzed::analyze(text, limits).unwrap();
    let expected = reference(text);
    assert_eq!(actual.normalized(), expected);
    let words: Vec<_> = expected.unicode_words().collect();
    assert_eq!(actual.tokens().map(|token| token.term).collect::<Vec<_>>(), words);
    for (position, token) in actual.tokens().enumerate() { assert_eq!(token.position, position as u32); }
    assert!(actual.retained_bytes() <= actual.peak_bytes());
    assert!(actual.peak_bytes() <= limits.memory_bytes);
}

#[test]
fn ascii_fast_path_equals_general_profile_for_every_pair() {
    for a in 0..128u8 {
        for b in 0..128u8 {
            let bytes = [a, b];
            compare(std::str::from_utf8(&bytes).unwrap());
        }
    }
    compare(&"A can't 32.3 a_b x.y\n\t\rZ ".repeat(1024));
}

#[test]
fn normalization_handles_blocking_hangul_leading_marks_and_long_sequences() {
    for source in ["A\u{315}\u{300}", "a\u{301}\u{300}", "\u{315}\u{300}A", "\u{1100}\u{1161}\u{11a8}", "\u{1100}\u{ac00}\u{11a8}", "\u{0ddd}\u{334}", "Σ ς σ ẞ ß SS İ I ı", "ＡＢＣ abc", "can't 32.3 👨‍👩‍👧‍👦"] { compare(source); }
    let source = format!("A{} Z", "\u{315}\u{300}".repeat(2000));
    let limits = AnalysisLimits { term_bytes: 16_384, ..AnalysisLimits::default() };
    let actual = Analyzed::analyze(&source, limits).unwrap();
    assert_eq!(actual.normalized(), reference(&source));
    assert!(actual.peak_bytes() > actual.retained_bytes());
    assert!(Analyzed::analyze(&source, AnalysisLimits { memory_bytes: source.len() + 32, ..limits }).is_err());
}

#[test]
fn seeded_unicode_sequences_match_independent_normalization() {
    let alphabet = ['a', 'A', '\u{301}', '\u{315}', '\u{327}', '\u{300}', '\u{034f}', '\u{00c5}', '\u{212b}', '\u{1100}', '\u{1161}', '\u{11a8}', '\u{ac00}', 'Σ', 'ς', 'ẞ', ' ', '\u{200d}', '\u{1f469}', '\u{0dd9}', '\u{0dcf}', '\u{0dca}'];
    let mut seed = 0x47cd9178e5b9a123u64;
    let mut next = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; seed };
    for _ in 0..4096 {
        let text: String = (0..next() % 64).map(|_| alphabet[next() as usize % alphabet.len()]).collect();
        compare(&text);
    }
}

fn fixture(name: &str, header: &str) -> String {
    let directory = std::env::var("PIN_UNICODE_DATA").expect("PIN_UNICODE_DATA is required");
    let text = std::fs::read_to_string(std::path::Path::new(&directory).join(name)).unwrap();
    assert!(text.starts_with(header));
    text
}

#[test]
#[ignore = "requires hash-verified PIN_UNICODE_DATA; mandatory in G1 CI"]
fn unicode16_word_boundary_conformance() {
    let data = fixture("WordBreakTest.txt", "# WordBreakTest-16.0.0.txt");
    let mut cases = 0;
    for (line, raw) in data.lines().enumerate() {
        let values = raw.split('#').next().unwrap().trim();
        if values.is_empty() { continue; }
        let mut text = String::new();
        let mut boundaries = Vec::new();
        for value in values.split_whitespace() {
            match value {
                "÷" => boundaries.push(text.len()),
                "×" => {},
                code => text.push(char::from_u32(u32::from_str_radix(code, 16).unwrap()).unwrap()),
            }
        }
        let mut actual: Vec<_> = text.split_word_bound_indices().map(|(offset, _)| offset).collect();
        actual.push(text.len());
        assert_eq!(actual, boundaries, "line {}", line + 1);
        cases += 1;
    }
    assert!(cases > 1800);
    eprintln!("unicode16 word boundaries: {cases} vectors");
}

#[test]
#[ignore = "requires hash-verified PIN_UNICODE_DATA; mandatory in G1 CI"]
fn unicode16_simple_default_casefold_conformance() {
    let data = fixture("CaseFolding.txt", "# CaseFolding-16.0.0.txt");
    let mut expected = std::collections::BTreeMap::new();
    for raw in data.lines() {
        let values = raw.split('#').next().unwrap().trim();
        if values.is_empty() { continue; }
        let columns: Vec<_> = values.split(';').map(str::trim).collect();
        if !matches!(columns[1], "C" | "S") { continue; }
        let source = char::from_u32(u32::from_str_radix(columns[0], 16).unwrap()).unwrap();
        let target = char::from_u32(u32::from_str_radix(columns[2], 16).unwrap()).unwrap();
        assert!(expected.insert(source, target).is_none());
    }
    assert!(expected.len() > 1400);
    for code in 0..=0x10ffff {
        let Some(source) = char::from_u32(code) else { continue; };
        let actual = unicode_case_mapping::case_folded(source).map_or(source, |value| char::from_u32(value.get()).unwrap());
        assert_eq!(actual, *expected.get(&source).unwrap_or(&source), "scalar {code:x}");
    }
    eprintln!("unicode16 simple folding: {} mappings; all scalar defaults", expected.len());
}
