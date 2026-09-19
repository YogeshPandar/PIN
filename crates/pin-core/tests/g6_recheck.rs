#![forbid(unsafe_code)]

use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::error::{Error, Result};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use pin_core::recheck::SingleTermMatcher;

fn query(source: &str) -> Query {
    Query::parse(source, QueryLimits::default()).unwrap()
}

fn reference(text: &str, query: &Query, limits: AnalysisLimits, steps: usize) -> Result<bool> {
    let document = Analyzed::analyze(text, limits)?;
    oracle::matches(&document, query, 1 << 20, steps)
}

fn compare(text: &str, query: &Query, limits: AnalysisLimits, steps: usize) {
    let matcher = SingleTermMatcher::new(query).unwrap();
    assert_eq!(
        matcher.matches(text, limits, steps),
        reference(text, query, limits, steps),
        "text={text:?}, query={query:?}, limits={limits:?}, steps={steps}"
    );
}

#[test]
fn all_ascii_byte_pairs_preserve_word_boundaries_and_folding() {
    let queries: Vec<_> = ["a", "b", "z", "0", "9", "a_b", "32.3", "can't"]
        .into_iter()
        .map(query)
        .collect();
    for left in 0..=127u8 {
        for right in 0..=127u8 {
            for (prefix, suffix) in [("A", "Z"), ("0", "9"), ("_", "_")] {
                let left_char = char::from(left);
                let right_char = char::from(right);
                let text = format!("{prefix}{left_char}{right_char}{suffix}");
                let document = Analyzed::analyze(&text, AnalysisLimits::default()).unwrap();
                for query in &queries {
                    let matcher = SingleTermMatcher::new(query).unwrap();
                    assert_eq!(
                        matcher.matches(&text, AnalysisLimits::default(), 100),
                        oracle::matches(&document, query, 1 << 20, 100),
                        "left={left}, right={right}, text={text:?}, query={query:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn unicode_normalization_is_not_replaced_by_ascii_folding() {
    let documents = [
        "CAFÉ cafe\u{301}",
        "Σ σ ς",
        "ẞ ß STRASSE Straße",
        "K Kelvin K k",
        "İ I ı i",
        "ＡＢＣ abc",
        "\u{1100}\u{1161} 가",
        "中文 العربية हिन्\u{200d}दी",
        "alpha\u{301} alpha 👩\u{200d}💻 beta",
        "a\u{315}\u{300} à\u{315}",
        "can't can’t 32.3 a_b a:b a.b",
        "",
        "!? 👩\u{200d}💻",
    ];
    let queries = [
        "café", "σ", "ß", "strasse", "straße", "k", "i", "abc", "가", "alpha",
    ];
    for source in queries {
        let query = query(source);
        for text in documents {
            compare(text, &query, AnalysisLimits::default(), 1 << 20);
        }
    }
}

#[test]
fn unsupported_query_shapes_keep_the_oracle() {
    for source in [
        "",
        "alpha OR beta",
        "alpha AND alpha",
        "NOT alpha",
        "alp*",
        "\"alpha beta\"",
    ] {
        assert!(SingleTermMatcher::new(&query(source)).is_none(), "{source}");
    }
    assert!(SingleTermMatcher::new(&query("((ALPHA))")).is_some());
}

#[test]
fn work_accounting_matches_the_oracle_for_first_last_absent_and_empty() {
    let query = query("alpha");
    for text in [
        "",
        "! ?",
        "alpha beta gamma",
        "beta alpha gamma",
        "beta gamma alpha",
        "beta gamma",
    ] {
        for steps in 0..=8 {
            compare(text, &query, AnalysisLimits::default(), steps);
        }
    }
}

#[test]
fn early_hits_do_not_hide_invalid_tails_or_analysis_errors() {
    let query = query("a");
    for text in ["a enormous", "A ENORMOUS", "a éééé", "a b c", "a b c\u{301}"] {
        for term_bytes in 0..=10 {
            for tokens in 0..=4 {
                let limits = AnalysisLimits {
                    tokens,
                    term_bytes,
                    ..AnalysisLimits::default()
                };
                for steps in [0, 1, 2, 10] {
                    compare(text, &query, limits, steps);
                }
            }
        }
    }
    let matcher = SingleTermMatcher::new(&query).unwrap();
    let limits = AnalysisLimits {
        term_bytes: 1,
        ..AnalysisLimits::default()
    };
    assert_eq!(
        matcher.matches("a oversized", limits, 0),
        Err(Error::Limit("term bytes"))
    );
    let limits = AnalysisLimits {
        tokens: 1,
        ..AnalysisLimits::default()
    };
    assert_eq!(
        matcher.matches("a b", limits, 0),
        Err(Error::Limit("element count"))
    );
}

#[test]
fn input_and_normalized_limits_match_the_reference() {
    let query = query("a");
    for text in ["", "A", "A b", "A café", "A cafe\u{301}", "A İ"] {
        for bytes in 0..=text.len() + 2 {
            compare(
                text,
                &query,
                AnalysisLimits {
                    input_bytes: bytes,
                    ..AnalysisLimits::default()
                },
                100,
            );
            compare(
                text,
                &query,
                AnalysisLimits {
                    normalized_bytes: bytes,
                    ..AnalysisLimits::default()
                },
                100,
            );
        }
    }
}

#[test]
fn ascii_needs_no_private_buffer_budget_and_unicode_stays_budgeted() {
    let query = query("alpha");
    let matcher = SingleTermMatcher::new(&query).unwrap();
    let limits = AnalysisLimits {
        memory_bytes: 0,
        ..AnalysisLimits::default()
    };
    assert_eq!(matcher.matches("ALPHA alpha beta", limits, 100), Ok(true));
    assert_eq!(matcher.matches("BETA gamma", limits, 100), Ok(false));
    assert_eq!(matcher.matches("", limits, 1), Ok(false));
    assert!(matches!(
        matcher.matches("alpha café", limits, 100),
        Err(Error::Budget(_))
    ));
    assert_eq!(matcher.matches("alpha", limits, 2), Ok(true));
}

#[test]
fn repeated_calls_and_query_bindings_do_not_retain_document_state() {
    let alpha = query("alpha");
    let beta = query("beta");
    let a = SingleTermMatcher::new(&alpha).unwrap();
    let b = SingleTermMatcher::new(&beta).unwrap();
    let limits = AnalysisLimits::default();
    for _ in 0..32 {
        assert_eq!(
            a.matches("alpha", limits, 0),
            Err(Error::Limit("search work"))
        );
        assert_eq!(a.matches("BETA", limits, 100), Ok(false));
        assert_eq!(b.matches("BETA", limits, 100), Ok(true));
        assert_eq!(a.matches("ALPHA café", limits, 100), Ok(true));
        assert_eq!(b.matches("ALPHA café", limits, 100), Ok(false));
    }
}

#[test]
fn deterministic_mixed_documents_match_the_reference() {
    let atoms = [
        "ALPHA",
        "beta",
        "cafe\u{301}",
        "café",
        "Σ",
        "ς",
        "K",
        "k",
        "can't",
        "32.3",
        "_",
        "👩\u{200d}💻",
    ];
    let separators = [" ", "\n", "\t", "-", ".", ":", "'", "\u{301}", "\u{200d}"];
    let queries: Vec<_> = ["alpha", "beta", "café", "σ", "k", "can't", "32.3"]
        .into_iter()
        .map(query)
        .collect();
    let mut state = 0x9e37_79b9u32;
    for _ in 0..2048 {
        let mut text = String::new();
        for _ in 0..16 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            text.push_str(atoms[state as usize % atoms.len()]);
            text.push_str(separators[(state >> 16) as usize % separators.len()]);
        }
        for query in &queries {
            compare(&text, query, AnalysisLimits::default(), 1 << 20);
        }
    }
}
