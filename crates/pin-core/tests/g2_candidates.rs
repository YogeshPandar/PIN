use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::candidate::CandidatePlan;
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};

fn covered(document: &Analyzed, plan: &CandidatePlan<'_>) -> bool {
    match plan {
        CandidatePlan::Empty => false,
        CandidatePlan::Universe => true,
        CandidatePlan::Terms(terms) => document.tokens().any(|token| terms.contains(&token.term)),
    }
}

#[test]
fn every_reference_match_is_covered() {
    let atoms = ["", "a", "b", "a*", "\"a a\"", "NOT a", "NOT (a OR b)"];
    let mut queries: Vec<String> = atoms.iter().map(|s| s.to_string()).collect();
    for left in &atoms[1..] {
        for right in &atoms[1..] {
            for op in ["AND", "OR"] {
                queries.push(format!("({left}) {op} ({right})"));
                queries.push(format!("NOT (({left}) {op} ({right}))"));
                queries.push(format!("a AND (({left}) {op} ({right}))"));
            }
        }
    }
    let documents = ["", "a", "b", "a a", "b a", "a b a", "alphabet", "c", "é A"];
    for source in queries {
        let query = Query::parse(&source, QueryLimits::default()).unwrap();
        let plan = CandidatePlan::build(&query, 1 << 20).unwrap();
        for text in documents {
            let document = Analyzed::analyze(text, AnalysisLimits::default()).unwrap();
            let exact = oracle::matches(&document, &query, 1 << 20, 1 << 20).unwrap();
            assert!(
                !exact || covered(&document, &plan),
                "{source:?} on {text:?}"
            );
        }
    }
}

#[test]
fn conjunction_selects_cover_and_disjunction_deduplicates() {
    let query = Query::parse("(a OR b) AND c", QueryLimits::default()).unwrap();
    assert_eq!(
        CandidatePlan::build(&query, 4096).unwrap(),
        CandidatePlan::Terms(vec!["c"])
    );
    let query = Query::parse("a OR a OR b", QueryLimits::default()).unwrap();
    assert_eq!(
        CandidatePlan::build(&query, 4096).unwrap(),
        CandidatePlan::Terms(vec!["a", "b"])
    );
}

#[test]
fn negation_includes_empty_documents_and_budget_never_truncates() {
    let query = Query::parse("NOT a", QueryLimits::default()).unwrap();
    assert_eq!(
        CandidatePlan::build(&query, 4096).unwrap(),
        CandidatePlan::Universe
    );
    assert!(CandidatePlan::build(&query, 0).is_err());
    let query = Query::parse("a OR b", QueryLimits::default()).unwrap();
    assert!(CandidatePlan::build(&query, 1).is_err());
}
