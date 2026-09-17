use pin_core::analysis::{AnalysisLimits, Analyzed, PROFILE_ID};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};

fn document(text: &str) -> Analyzed {
    Analyzed::analyze(text, AnalysisLimits::default()).unwrap()
}

fn parse(text: &str) -> Query {
    Query::parse(text, QueryLimits::default()).unwrap()
}

fn matches(text: &str, query: &str) -> bool {
    oracle::matches(&document(text), &parse(query), 1 << 20, 1_000_000).unwrap()
}

#[test]
fn unicode_profile_has_frozen_normalization_and_simple_folding() {
    let doc = document("CAFÉ cafe\u{301} Σ σ ς ẞ ß STRASSE Straße");
    let terms: Vec<_> = doc.tokens().map(|token| token.term).collect();
    assert_eq!(terms, ["café", "café", "σ", "σ", "σ", "ß", "ß", "strasse", "straße"]);
    assert_eq!(doc.profile(), PROFILE_ID);
    assert_eq!(doc.tokens().map(|token| token.position).collect::<Vec<_>>(), (0..9).collect::<Vec<_>>());
    assert!(matches("Cafe\u{301}", "CAFÉ"));
    assert!(!matches("Straße", "STRASSE"));
    assert!(!matches("ＡＢＣ", "abc"));
    assert!(matches("can't jump 32.3 feet", "\"CAN'T JUMP 32.3\""));
    assert_eq!(document("!? 👩‍💻").len(), 0);
}

#[test]
fn boolean_precedence_matches_complete_truth_tables() {
    for bits in 0..8 {
        let mut text = String::new();
        for (index, term) in ["a", "b", "c"].into_iter().enumerate() {
            if bits & (1 << index) != 0 { text.push_str(term); text.push(' '); }
        }
        let a = bits & 1 != 0;
        let b = bits & 2 != 0;
        let c = bits & 4 != 0;
        assert_eq!(matches(&text, "a OR b AND NOT c"), a || (b && !c));
        assert_eq!(matches(&text, "(a OR b) AND NOT c"), (a || b) && !c);
        assert_eq!(matches(&text, "NOT (a AND (b OR c))"), !(a && (b || c)));
        assert_eq!(matches(&text, "NOT NOT a"), a);
    }
}

#[test]
fn phrase_repetition_negation_empty_and_null_are_distinct() {
    assert!(matches("a a a", "\"a a\""));
    assert!(!matches("a", "\"a a\""));
    assert!(!matches("a x a", "\"a a\""));
    assert!(matches("a b a b a", "\"b a b\""));
    assert!(!matches("a b", "\"b a\""));
    assert!(matches("alpha alpine", "alp*"));
    assert!(!matches("alpha", "alpha* AND z*"));
    assert!(matches("", "NOT missing"));
    assert!(!matches("", ""));
    assert!(!matches("anything", "\"\""));
    assert!(matches("anything", "NOT \"\""));
    assert_eq!(oracle::matches_nullable(None, &parse("NOT a"), 1000, 1000).unwrap(), None);
    assert!(matches("a \"b\" \\ c", r#""a \"b\" \\ c""#));
}

#[test]
fn invalid_syntax_and_complexity_fail_explicitly() {
    for input in ["a b", "AND a", "a AND", "a NOT b", "()", "(a", "a)", "a(b)", "\"a", "*", "a*b", "a**", "a\\b", "\"a\\q\"", "NOT", "a OR OR b"] {
        assert!(Query::parse(input, QueryLimits::default()).is_err(), "{input}");
    }
    for (input, limits) in [
        ("abc", QueryLimits { bytes: 2, ..QueryLimits::default() }),
        ("a OR b", QueryLimits { nodes: 2, ..QueryLimits::default() }),
        ("NOT NOT a", QueryLimits { depth: 2, ..QueryLimits::default() }),
        ("((a))", QueryLimits { depth: 1, ..QueryLimits::default() }),
        ("\"a b\"", QueryLimits { terms: 1, ..QueryLimits::default() }),
        ("abc", QueryLimits { term_bytes: 2, ..QueryLimits::default() }),
        ("a", QueryLimits { memory_bytes: 0, ..QueryLimits::default() }),
    ] {
        assert!(Query::parse(input, limits).is_err(), "{input}");
    }
    let deep = format!("{}a{}", "(".repeat(10_000), ")".repeat(10_000));
    assert!(Query::parse(&deep, QueryLimits { bytes: 30_000, ..QueryLimits::default() }).is_err());
    assert!(oracle::matches(&document("a"), &parse("a"), 1024, 0).is_err());
    assert!(Analyzed::analyze("a b", AnalysisLimits { tokens: 1, ..AnalysisLimits::default() }).is_err());
    assert!(Analyzed::analyze("abc", AnalysisLimits { normalized_bytes: 2, ..AnalysisLimits::default() }).is_err());
    assert!(Analyzed::analyze("a", AnalysisLimits { memory_bytes: 0, ..AnalysisLimits::default() }).is_err());
}

#[test]
fn query_wire_roundtrip_revalidates_versions_bounds_and_utf8() {
    let mut bytes = [0; 4096];
    for source in ["", "a OR NOT b", "\"CAFÉ cafe\u{301}\"", "(a AND b) OR alp*"] {
        let query = parse(source);
        let len = query.encode(&mut bytes, 4096).unwrap();
        let restored = Query::decode(&bytes[..len], QueryLimits::default()).unwrap();
        assert_eq!(restored.source(), source);
        assert_eq!(restored.node_count(), query.node_count());
        for cut in 0..len { assert!(Query::decode(&bytes[..cut], QueryLimits::default()).is_err()); }
        assert!(Query::decode(&bytes[..len + 1], QueryLimits::default()).is_err());
    }
    let query = parse("a");
    let len = query.encode(&mut bytes, 4096).unwrap();
    assert_eq!(&bytes[..len], b"PIN1\x05\0\x01\0\0\0\0\0\x0d\0\0\0\x01\0\0\0\x01\0\0\0\x01\0\0\0a");
    for (offset, value) in [(5, 1), (6, 2), (8, 1), (16, 2), (20, 2), (22, 1), (28, 255)] {
        query.encode(&mut bytes, 4096).unwrap();
        bytes[offset] = value;
        assert!(Query::decode(&bytes[..len], QueryLimits::default()).is_err());
    }
}
