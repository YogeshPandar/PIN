//! deterministic recheck ablation; timings are evidence only when this is executed.
//! run in release mode, preserve CSV, and qualify whole PostgreSQL workloads too.
#![forbid(unsafe_code)]

use pin_core::analysis::{AnalysisLimits, Analyzed};
use pin_core::oracle;
use pin_core::query::{Query, QueryLimits};
use pin_core::recheck::SingleTermMatcher;
use std::error::Error;
use std::hint::black_box;
use std::time::Instant;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy)]
enum Mode {
    Reference,
    Streaming,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Reference => "reference",
            Self::Streaming => "streaming",
        }
    }
}

struct Case {
    name: &'static str,
    text: String,
    query: Query,
}

fn evaluate(case: &Case, matcher: SingleTermMatcher<'_>, mode: Mode) -> Result<bool> {
    let text = black_box(case.text.as_str());
    let matched = match mode {
        Mode::Reference => {
            let document = Analyzed::analyze(text, AnalysisLimits::default())?;
            oracle::matches(&document, black_box(&case.query), 1 << 20, 1 << 24)?
        }
        Mode::Streaming => matcher.matches(text, AnalysisLimits::default(), 1 << 24)?,
    };
    Ok(black_box(matched))
}

fn measure(case: &Case, mode: Mode, iterations: usize) -> Result<(u128, usize)> {
    let matcher = SingleTermMatcher::new(&case.query).ok_or("benchmark requires one term")?;
    let mut hits = 0usize;
    let start = Instant::now();
    for _ in 0..iterations {
        hits += usize::from(evaluate(case, matcher, mode)?);
    }
    Ok((start.elapsed().as_nanos(), black_box(hits)))
}

fn positive(value: Option<String>, maximum: usize) -> Result<usize> {
    let value: usize = value.ok_or("missing option value")?.parse()?;
    if value == 0 || value > maximum {
        return Err(format!("option must be in 1..={maximum}").into());
    }
    Ok(value)
}

fn main() -> Result<()> {
    let mut iterations = 1000;
    let mut samples = 7;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--iterations" => iterations = positive(args.next(), 1_000_000)?,
            "--samples" => samples = positive(args.next(), 1000)?,
            "--help" => {
                println!("g6_recheck [--iterations 1..1000000] [--samples 1..1000]");
                return Ok(());
            }
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    let mut cases = Vec::new();
    for (name, text, source) in [
        ("empty", String::new(), "alpha"),
        ("ascii_short", "ALPHA beta can't 32.3".to_owned(), "alpha"),
        ("ascii_miss", "BETA gamma delta ".repeat(128), "alpha"),
        ("ascii_first", format!("ALPHA {}", "beta gamma ".repeat(512)), "alpha"),
        ("ascii_last", format!("{}ALPHA", "beta gamma ".repeat(512)), "alpha"),
        ("ascii_boundaries", "A_B a:b a.b can't 32.3 ".repeat(128), "can't"),
        ("unicode_nfc", "CAFÉ beta gamma ".repeat(128), "café"),
        ("unicode_decomposed", "CAFE\u{301} beta Σ K ".repeat(128), "café"),
        ("unicode_miss", "CAFE\u{301} Straße Σ K ".repeat(128), "alpha"),
    ] {
        cases.push(Case {
            name,
            text,
            query: Query::parse(source, QueryLimits::default())?,
        });
    }
    for case in &cases {
        let matcher = SingleTermMatcher::new(&case.query).ok_or("invalid benchmark query")?;
        let expected = evaluate(case, matcher, Mode::Reference)?;
        if evaluate(case, matcher, Mode::Streaming)? != expected {
            return Err(format!("oracle mismatch: {}", case.name).into());
        }
        for mode in [Mode::Reference, Mode::Streaming] {
            for _ in 0..32 {
                evaluate(case, matcher, mode)?;
            }
        }
    }
    println!("case,mode,sample,iterations,input_bytes,elapsed_ns,hits");
    for sample in 0..samples {
        for (index, case) in cases.iter().enumerate() {
            let modes = if (sample + index) % 2 == 0 {
                [Mode::Reference, Mode::Streaming]
            } else {
                [Mode::Streaming, Mode::Reference]
            };
            let mut previous = None;
            for mode in modes {
                let (elapsed, hits) = measure(case, mode, iterations)?;
                if previous.is_some_and(|expected| expected != hits) {
                    return Err(format!("timed result mismatch: {}", case.name).into());
                }
                previous = Some(hits);
                println!(
                    "{},{},{sample},{iterations},{},{elapsed},{hits}",
                    case.name,
                    mode.name(),
                    case.text.len()
                );
            }
        }
    }
    Ok(())
}
