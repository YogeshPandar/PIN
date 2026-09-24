#![forbid(unsafe_code)]

use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use pin_core::recheck::PhraseMatcher;

fn query(terms: &str) -> Query {
    Query::parse(&format!("\"{terms}\""), QueryLimits::default()).unwrap()
}

fn reference(text: &str, query: &Query, limits: AnalysisLimits, steps: usize) -> Result<bool> {
    let document = Analyzed::analyze(text, limits)?;
    oracle::matches(&document, query, 1 << 20, steps)
}

fn compare(text: &str, query: &Query, limits: AnalysisLimits, steps: usize) {
    assert_eq!(
        PhraseMatcher::new(query)
            .unwrap()
            .matches(text, limits, steps),
        reference(text, query, limits, steps),
        "text={text:?}, query={query:?}, limits={limits:?}, steps={steps}"
    );
}

#[test]
fn every_short_binary_document_matches_the_independent_window_oracle() {
    for phrase in ["a a", "a b", "b a", "b b", "a b a", "a a b a", "b b b b"] {
        let query = query(phrase);
        for len in 0..=8 {
            for bits in 0..1u32 << len {
                let text = (0..len)
                    .map(|index| if bits & (1 << index) == 0 { "A" } else { "b" })
                    .collect::<Vec<_>>()
                    .join(" ");
                for steps in 0..=len * 4 + 2 {
                    compare(&text, &query, AnalysisLimits::default(), steps);
                }
            }
        }
    }
}

#[test]
fn punctuation_and_unicode_keep_the_existing_analysis_profile() {
    for (phrase, texts) in [
        ("a b", ["A,,,B", "a b c", "a_b", "a:b", "a 👩‍💻 b"]),
        (
            "café k",
            ["CAFÉ K", "cafe\u{301} K", "CAFÉ x K", "café", "x café k"],
        ),
        ("σ ß", ["Σ ẞ", "ς ß", "σ SS", "σ x ß", "σ ß tail"]),
        (
            "can't 32.3",
            [
                "CAN'T 32.3",
                "can’t 32.3",
                "can't 32 3",
                "can't",
                "can't 32.3 x",
            ],
        ),
    ] {
        let query = query(phrase);
        for text in texts {
            for steps in 0..=20 {
                compare(text, &query, AnalysisLimits::default(), steps);
            }
        }
    }
}

#[test]
fn early_matches_and_work_errors_do_not_hide_invalid_tails() {
    let query = query("a b");
    for text in ["A B oversized", "a b c", "a b éééé", "x a b oversized", "a"] {
        for tokens in 0..=4 {
            for term_bytes in 0..=10 {
                let limits = AnalysisLimits {
                    tokens,
                    term_bytes,
                    ..AnalysisLimits::default()
                };
                for steps in 0..=12 {
                    compare(text, &query, limits, steps);
                }
            }
        }
    }
    assert_eq!(
        PhraseMatcher::new(&query).unwrap().matches(
            "a b oversized",
            AnalysisLimits {
                term_bytes: 1,
                ..AnalysisLimits::default()
            },
            0
        ),
        Err(Error::Limit("term bytes"))
    );
}

#[test]
fn input_and_normalized_byte_limits_match_analysis() {
    let query = query("a b");
    for text in ["", "A", "A B", "A cafe\u{301}", "A İ", "A B tail"] {
        for limit in 0..=text.len() + 2 {
            compare(
                text,
                &query,
                AnalysisLimits {
                    input_bytes: limit,
                    ..AnalysisLimits::default()
                },
                100,
            );
            compare(
                text,
                &query,
                AnalysisLimits {
                    normalized_bytes: limit,
                    ..AnalysisLimits::default()
                },
                100,
            );
        }
    }
}

#[test]
fn bounded_window_wraps_at_64_terms_and_longer_phrases_keep_the_oracle() {
    let terms = (0..64)
        .map(|index| format!("w{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    let query64 = query(&terms);
    for text in [&terms, &format!("noise {terms}"), &format!("{terms} tail")] {
        compare(text, &query64, AnalysisLimits::default(), 1000);
    }
    assert!(PhraseMatcher::new(&query(&format!("{terms} more"))).is_none());
    for source in ["a", "a*", "a AND b", "NOT a", "\"a b\" OR c", ""] {
        let query = Query::parse(source, QueryLimits::default()).unwrap();
        assert!(PhraseMatcher::new(&query).is_none(), "{source}");
    }
}

#[test]
fn ascii_phrase_needs_no_owned_document_allocation() {
    let query = query("alpha beta");
    let limits = AnalysisLimits {
        memory_bytes: 0,
        ..AnalysisLimits::default()
    };
    assert_eq!(
        PhraseMatcher::new(&query)
            .unwrap()
            .matches("ALPHA BETA tail", limits, 100),
        Ok(true)
    );
    assert!(reference("ALPHA BETA tail", &query, limits, 100).is_err());
}
