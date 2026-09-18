// official fixtures are fetched and hash-verified by tools/g1_unicode_data.py.

use super::*;
use std::collections::BTreeSet;
use unicode_normalization::UnicodeNormalization;

fn nfc(text: &str) -> String {
    let mut state = Nfc::default();
    let mut budget = MemoryBudget::new(1 << 20);
    let mut result = String::new();
    let mut output = |value, _: &mut MemoryBudget| {
        result.push(value);
        Ok(())
    };
    for value in text.chars() {
        state.push(value, &mut output, &mut budget).unwrap();
    }
    state.finish(&mut output, &mut budget).unwrap();
    state.release(&mut budget).unwrap();
    assert_eq!(budget.used(), 0);
    result
}

fn from_hex(text: &str) -> String {
    text.split_whitespace()
        .map(|code| char::from_u32(u32::from_str_radix(code, 16).unwrap()).unwrap())
        .collect()
}

#[test]
#[ignore = "requires hash-verified PIN_UNICODE_DATA; mandatory in G1 CI"]
fn unicode16_nfc_conformance() {
    let directory = std::env::var("PIN_UNICODE_DATA").expect("PIN_UNICODE_DATA is required");
    let text =
        std::fs::read_to_string(std::path::Path::new(&directory).join("NormalizationTest.txt"))
            .unwrap();
    assert!(text.starts_with("# NormalizationTest-16.0.0.txt"));
    let mut part_one = false;
    let mut listed = BTreeSet::new();
    let mut cases = 0;
    for (line, raw) in text.lines().enumerate() {
        let data = raw.split('#').next().unwrap().trim();
        if data.starts_with('@') {
            part_one = data.starts_with("@Part1");
            continue;
        }
        if data.is_empty() {
            continue;
        }
        let columns: Vec<_> = data.split(';').take(5).map(from_hex).collect();
        assert_eq!(columns.len(), 5);
        if part_one {
            assert_eq!(columns[0].chars().count(), 1);
            listed.insert(columns[0].chars().next().unwrap());
        }
        for input in &columns[..3] {
            assert_eq!(nfc(input), columns[1], "line {}", line + 1);
        }
        for input in &columns[3..] {
            assert_eq!(nfc(input), columns[3], "line {}", line + 1);
        }
        let expected: String = columns[0]
            .nfc()
            .map(|value| fold(value).unwrap())
            .nfc()
            .collect();
        let mut budget = MemoryBudget::new(1 << 20);
        assert_eq!(
            profile_text(&columns[0], 1 << 20, &mut budget).unwrap().0,
            expected
        );
        cases += 1;
    }
    assert!(cases > 19_000);
    let mut identity_cases = 0;
    for code in 0..=0x10ffff {
        let Some(value) = char::from_u32(code) else {
            continue;
        };
        if !listed.contains(&value) {
            let text = value.to_string();
            assert_eq!(nfc(&text), text, "unlisted scalar {code:x}");
            identity_cases += 1;
        }
    }
    eprintln!("unicode16 nfc: {cases} vectors; {identity_cases} unlisted scalar identities");
}
