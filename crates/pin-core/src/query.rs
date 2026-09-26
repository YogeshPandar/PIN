// bounded iterative parsing into a flat tree; dropping deep queries never recurses.
// syntax and profile meaning are fixed in docs/g1-semantics.md.

use crate::analysis::{AnalysisLimits, Analyzed, PROFILE_ID};
use crate::budget::MemoryBudget;
use crate::codec::{self, bytes::Reader, records};
use crate::error::{Error, Result};
use crate::memory::{copy_text, reserve, vector};
use std::mem::size_of;

#[derive(Clone, Copy, Debug)]
pub struct QueryLimits {
    pub bytes: usize,
    pub nodes: usize,
    pub depth: u16,
    pub terms: usize,
    pub term_bytes: usize,
    pub memory_bytes: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            bytes: 16_384,
            nodes: 1024,
            depth: 64,
            terms: 256,
            term_bytes: 1024,
            memory_bytes: 1 << 20,
        }
    }
}

#[derive(Debug)]
pub(crate) enum Kind {
    None,
    Term(String),
    Prefix(String),
    Phrase(Vec<String>),
    Not(usize),
    And(usize, usize),
    Or(usize, usize),
}

#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) kind: Kind,
    depth: u16,
}

#[derive(Debug)]
pub struct Query {
    pub(crate) nodes: Vec<Node>,
    pub(crate) root: usize,
    source: String,
    terms: usize,
    retained_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperatorKind {
    Open,
    Not,
    And,
    Or,
}

#[derive(Clone, Copy, Debug)]
struct Operator {
    kind: OperatorKind,
    offset: usize,
}

impl Operator {
    fn precedence(self) -> u8 {
        match self.kind {
            OperatorKind::Open => 0,
            OperatorKind::Or => 1,
            OperatorKind::And => 2,
            OperatorKind::Not => 3,
        }
    }
}

struct Builder {
    limits: QueryLimits,
    budget: MemoryBudget,
    nodes: Vec<Node>,
    values: Vec<usize>,
    operators: Vec<Operator>,
    terms: usize,
}

impl Builder {
    fn add_node(&mut self, kind: Kind) -> Result<()> {
        let depth = match &kind {
            Kind::Not(child) => self.nodes[*child].depth.checked_add(1),
            Kind::And(left, right) | Kind::Or(left, right) => self.nodes[*left]
                .depth
                .max(self.nodes[*right].depth)
                .checked_add(1),
            _ => Some(1),
        }
        .ok_or(Error::Limit("query depth"))?;
        if depth > self.limits.depth {
            return Err(Error::Limit("query depth"));
        }
        reserve(&mut self.nodes, 1, self.limits.nodes, &mut self.budget)?;
        reserve(&mut self.values, 1, self.limits.nodes, &mut self.budget)?;
        self.values.push(self.nodes.len());
        self.nodes.push(Node { kind, depth });
        Ok(())
    }

    fn reduce(&mut self, operator: Operator) -> Result<()> {
        let error = Error::QuerySyntax {
            offset: operator.offset,
        };
        let right = self.values.pop().ok_or(error)?;
        let kind = match operator.kind {
            OperatorKind::Not => Kind::Not(right),
            OperatorKind::And => Kind::And(self.values.pop().ok_or(error)?, right),
            OperatorKind::Or => Kind::Or(self.values.pop().ok_or(error)?, right),
            OperatorKind::Open => return Err(error),
        };
        self.add_node(kind)
    }

    fn operator(&mut self, operator: Operator) -> Result<()> {
        if matches!(operator.kind, OperatorKind::And | OperatorKind::Or) {
            while self
                .operators
                .last()
                .is_some_and(|top| top.precedence() >= operator.precedence())
            {
                let top = self.operators.pop().ok_or(Error::QuerySyntax {
                    offset: operator.offset,
                })?;
                self.reduce(top)?;
            }
        }
        reserve(&mut self.operators, 1, self.limits.nodes, &mut self.budget)?;
        self.operators.push(operator);
        Ok(())
    }

    fn close(&mut self, offset: usize) -> Result<()> {
        while let Some(operator) = self.operators.pop() {
            if operator.kind == OperatorKind::Open {
                return Ok(());
            }
            self.reduce(operator)?;
        }
        Err(Error::QuerySyntax { offset })
    }

    fn literal(&mut self, text: &str, quoted: bool, prefix: bool, offset: usize) -> Result<()> {
        let normalized_bytes = self
            .limits
            .bytes
            .checked_mul(4)
            .ok_or(Error::Limit("query bytes"))?;
        let analyzed = Analyzed::analyze(
            text,
            AnalysisLimits {
                input_bytes: self.limits.bytes,
                normalized_bytes,
                tokens: u32::try_from(self.limits.terms)
                    .map_err(|_| Error::Limit("query terms"))?,
                term_bytes: self.limits.term_bytes,
                memory_bytes: self.budget.remaining(),
            },
        )?;
        let count = analyzed.len() as usize;
        if !quoted && count != 1 {
            return Err(Error::QuerySyntax { offset });
        }
        let terms = self
            .terms
            .checked_add(count)
            .ok_or(Error::Limit("query terms"))?;
        if terms > self.limits.terms {
            return Err(Error::Limit("query terms"));
        }
        let bytes = analyzed.retained_bytes();
        self.budget.charge(bytes)?;
        let kind = if quoted {
            if count == 0 {
                Kind::None
            } else {
                let mut terms = vector(count, &mut self.budget)?;
                for token in analyzed.tokens() {
                    terms.push(copy_text(token.term, &mut self.budget)?);
                }
                Kind::Phrase(terms)
            }
        } else {
            let token = analyzed.token(0).ok_or(Error::QuerySyntax { offset })?;
            let term = copy_text(token.term, &mut self.budget)?;
            if prefix {
                Kind::Prefix(term)
            } else {
                Kind::Term(term)
            }
        };
        drop(analyzed);
        self.budget.release(bytes)?;
        self.terms = terms;
        self.add_node(kind)
    }
}

fn quoted(
    input: &str,
    cursor: &mut usize,
    limit: usize,
    budget: &mut MemoryBudget,
) -> Result<String> {
    let begin = *cursor;
    *cursor += 1;
    let mut bytes = Vec::new();
    while *cursor < input.len() {
        let byte = input.as_bytes()[*cursor];
        if byte == b'"' {
            *cursor += 1;
            return String::from_utf8(bytes).map_err(|_| Error::InvalidDocument);
        }
        if byte == b'\\' {
            *cursor += 1;
            let next = input
                .as_bytes()
                .get(*cursor)
                .copied()
                .ok_or(Error::QuerySyntax { offset: begin })?;
            if !matches!(next, b'"' | b'\\') {
                return Err(Error::QuerySyntax { offset: *cursor });
            }
            reserve(&mut bytes, 1, limit, budget)?;
            bytes.push(next);
            *cursor += 1;
        } else {
            let scalar = input[*cursor..]
                .chars()
                .next()
                .ok_or(Error::QuerySyntax { offset: *cursor })?;
            let end = *cursor + scalar.len_utf8();
            reserve(&mut bytes, scalar.len_utf8(), limit, budget)?;
            bytes.extend_from_slice(&input.as_bytes()[*cursor..end]);
            *cursor = end;
        }
    }
    Err(Error::QuerySyntax { offset: begin })
}

impl Query {
    // rejects malformed syntax and resource excess; no partial query is returned.
    pub fn parse(input: &str, limits: QueryLimits) -> Result<Self> {
        if input.len() > limits.bytes {
            return Err(Error::Limit("query bytes"));
        }
        let mut builder = Builder {
            limits,
            budget: MemoryBudget::new(limits.memory_bytes),
            nodes: Vec::new(),
            values: Vec::new(),
            operators: Vec::new(),
            terms: 0,
        };
        let mut cursor = 0;
        let mut need_operand = true;
        let mut parentheses = 0u16;
        while cursor < input.len() {
            let byte = input.as_bytes()[cursor];
            if byte.is_ascii_whitespace() {
                cursor += 1;
                continue;
            }
            let offset = cursor;
            if byte == b'(' {
                if !need_operand {
                    return Err(Error::QuerySyntax { offset });
                }
                parentheses = parentheses
                    .checked_add(1)
                    .ok_or(Error::Limit("query depth"))?;
                if parentheses > limits.depth {
                    return Err(Error::Limit("query depth"));
                }
                builder.operator(Operator {
                    kind: OperatorKind::Open,
                    offset,
                })?;
                cursor += 1;
                continue;
            }
            if byte == b')' {
                if need_operand {
                    return Err(Error::QuerySyntax { offset });
                }
                builder.close(offset)?;
                parentheses = parentheses
                    .checked_sub(1)
                    .ok_or(Error::QuerySyntax { offset })?;
                cursor += 1;
                continue;
            }
            if byte == b'"' {
                if !need_operand {
                    return Err(Error::QuerySyntax { offset });
                }
                let text = quoted(input, &mut cursor, limits.bytes, &mut builder.budget)?;
                builder.literal(&text, true, false, offset)?;
                let bytes = text.capacity();
                drop(text);
                builder.budget.release(bytes)?;
                need_operand = false;
                continue;
            }
            while cursor < input.len() {
                let byte = input.as_bytes()[cursor];
                if byte.is_ascii_whitespace() || matches!(byte, b'(' | b')' | b'"') {
                    break;
                }
                cursor += 1;
            }
            let raw = &input[offset..cursor];
            let operator = match raw {
                "NOT" => Some(OperatorKind::Not),
                "AND" => Some(OperatorKind::And),
                "OR" => Some(OperatorKind::Or),
                _ => None,
            };
            if let Some(kind) = operator {
                if (kind == OperatorKind::Not) != need_operand {
                    return Err(Error::QuerySyntax { offset });
                }
                builder.operator(Operator { kind, offset })?;
                need_operand = true;
                continue;
            }
            if !need_operand {
                return Err(Error::QuerySyntax { offset });
            }
            let (text, prefix) = raw
                .strip_suffix('*')
                .map_or((raw, false), |text| (text, true));
            if text.is_empty() || text.contains(['*', '\\']) {
                return Err(Error::QuerySyntax { offset });
            }
            builder.literal(text, false, prefix, offset)?;
            need_operand = false;
        }
        if builder.nodes.is_empty() && builder.operators.is_empty() {
            builder.add_node(Kind::None)?;
        } else if need_operand {
            return Err(Error::QuerySyntax {
                offset: input.len(),
            });
        }
        while let Some(operator) = builder.operators.pop() {
            builder.reduce(operator)?;
        }
        if builder.values.len() != 1 || parentheses != 0 {
            return Err(Error::QuerySyntax {
                offset: input.len(),
            });
        }
        let root = builder.values[0];
        let scratch = builder.values.capacity() * size_of::<usize>()
            + builder.operators.capacity() * size_of::<Operator>();
        drop(builder.values);
        drop(builder.operators);
        builder.budget.release(scratch)?;
        let source = copy_text(input, &mut builder.budget)?;
        Ok(Self {
            nodes: builder.nodes,
            root,
            source,
            terms: builder.terms,
            retained_bytes: builder.budget.used(),
        })
    }

    pub const fn profile(&self) -> u32 {
        PROFILE_ID
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn depth(&self) -> u16 {
        self.nodes[self.root].depth
    }
    pub const fn term_count(&self) -> usize {
        self.terms
    }
    pub fn is_single_term(&self) -> bool {
        matches!(&self.nodes[self.root].kind, Kind::Term(_))
    }

    /// Writes normalized terms in source order when the complete expression is
    /// a conjunction of exact terms, returning the number written.
    ///
    /// Parsing produces a flat postorder node list in which every operand is
    /// reachable from the root. Therefore checking the node kinds is enough
    /// to reject OR, NOT, phrase, prefix, and empty expressions without an
    /// auxiliary stack or allocation.
    pub fn exact_conjunction_terms<'a>(&'a self, output: &mut [&'a str]) -> Option<usize> {
        if !matches!(self.nodes[self.root].kind, Kind::Term(_) | Kind::And(_, _))
            || self
                .nodes
                .iter()
                .any(|node| !matches!(node.kind, Kind::Term(_) | Kind::And(_, _)))
        {
            return None;
        }
        let count = self
            .nodes
            .iter()
            .filter(|node| matches!(node.kind, Kind::Term(_)))
            .count();
        if count > output.len() {
            return None;
        }
        for (slot, term) in
            output
                .iter_mut()
                .zip(self.nodes.iter().filter_map(|node| match &node.kind {
                    Kind::Term(term) => Some(term.as_str()),
                    _ => None,
                }))
        {
            *slot = term;
        }
        Some(count)
    }

    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    // stores versioned source text, not native rust tree layout; decoding reparses it.
    pub fn encode(&self, output: &mut [u8], max_bytes: usize) -> Result<usize> {
        let len = self
            .source
            .len()
            .checked_add(12)
            .ok_or(Error::Limit("query bytes"))?;
        if len
            .checked_add(records::HEADER_BYTES)
            .is_none_or(|total| total > max_bytes)
        {
            return Err(Error::Limit("query bytes"));
        }
        let text_len = u32::try_from(self.source.len()).map_err(|_| Error::Limit("query bytes"))?;
        let mut writer = records::start(output, 5, len)?;
        writer.u32(PROFILE_ID)?;
        writer.u16(1)?;
        writer.u16(0)?;
        writer.u32(text_len)?;
        writer.put(self.source.as_bytes())?;
        Ok(writer.len())
    }

    pub fn decode(bytes: &[u8], limits: QueryLimits) -> Result<Self> {
        let max_bytes = limits
            .bytes
            .checked_add(28)
            .ok_or(Error::Limit("query bytes"))?;
        let mut reader = Reader::new(records::payload(bytes, 5, max_bytes)?);
        if reader.u32()? != PROFILE_ID {
            return Err(Error::InvalidProfile);
        }
        if reader.u16()? != 1 {
            return Err(codec::Error::new(4, codec::ErrorKind::UnsupportedVersion).into());
        }
        if reader.u16()? != 0 {
            return Err(codec::Error::new(6, codec::ErrorKind::NonCanonical).into());
        }
        let len = usize::try_from(reader.u32()?).map_err(|_| Error::Limit("query bytes"))?;
        let text = std::str::from_utf8(reader.take(len)?)
            .map_err(|_| codec::Error::new(12, codec::ErrorKind::InvalidUtf8))?;
        reader.finish()?;
        Self::parse(text, limits)
    }
}

#[cfg(test)]
mod tests {
    use super::{Query, QueryLimits};

    fn terms<'a>(query: &'a Query, output: &mut [&'a str]) -> Option<usize> {
        query.exact_conjunction_terms(output)
    }

    #[test]
    fn exact_conjunction_accepts_single_and_parenthesized_and_terms() {
        let mut output = [""; 4];
        let query = Query::parse("Alpha", QueryLimits::default()).unwrap();
        assert_eq!(terms(&query, &mut output), Some(1));
        assert_eq!(&output[..1], &["alpha"]);

        let query =
            Query::parse("(Alpha AND (BRAVO AND charlie))", QueryLimits::default()).unwrap();
        assert_eq!(terms(&query, &mut output), Some(3));
        assert_eq!(&output[..3], &["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn exact_conjunction_rejects_other_expression_kinds() {
        for source in [
            "alpha OR bravo",
            "\"alpha bravo\"",
            "alpha*",
            "NOT alpha",
            "(alpha AND bravo) OR charlie",
            "alpha AND NOT bravo",
        ] {
            let query = Query::parse(source, QueryLimits::default()).unwrap();
            let mut output = [""; 8];
            assert_eq!(terms(&query, &mut output), None, "{source}");
        }
    }

    #[test]
    fn exact_conjunction_rejects_insufficient_output_capacity() {
        {
            let query = Query::parse("alpha AND bravo", QueryLimits::default()).unwrap();
            let mut output = [""; 1];
            assert_eq!(terms(&query, &mut output), None);
        }
        let query = Query::parse("alpha", QueryLimits::default()).unwrap();
        assert_eq!(terms(&query, &mut []), None);
    }
}
