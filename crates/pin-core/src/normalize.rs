// budgeted streaming nfc; tables come from unicode-normalization 0.1.24.
// canonical order preserves equal classes; composition honors blocking and hangul.
// contracts: docs/g1-api-evidence.md, uax #15, and unicode_normalization::char.

use crate::budget::MemoryBudget;
use crate::error::{Error, Result};
use crate::memory::reserve;
use unicode_normalization::char::{canonical_combining_class as class, compose, decompose_canonical};

#[derive(Clone, Copy)]
struct Scalar {
    value: char,
    order: u32,
}

#[derive(Default)]
struct Nfc {
    pending: Vec<Scalar>,
}

impl Nfc {
    fn push<F>(&mut self, value: char, emit: &mut F, budget: &mut MemoryBudget) -> Result<()>
    where
        F: FnMut(char, &mut MemoryBudget) -> Result<()>,
    {
        let mut status = Ok(());
        decompose_canonical(value, |value| {
            if status.is_ok() {
                status = self.decomposed(value, emit, budget);
            }
        });
        status
    }

    fn decomposed<F>(&mut self, value: char, emit: &mut F, budget: &mut MemoryBudget) -> Result<()>
    where
        F: FnMut(char, &mut MemoryBudget) -> Result<()>,
    {
        if class(value) == 0 && !self.pending.is_empty() {
            self.compose();
            if self.pending.len() == 1 && class(self.pending[0].value) == 0 {
                if let Some(joined) = compose(self.pending[0].value, value) {
                    self.pending[0].value = joined;
                    return Ok(());
                }
            }
            self.emit(emit, budget)?;
        }
        let order = u32::try_from(self.pending.len()).map_err(|_| Error::Limit("combining sequence"))?;
        reserve(&mut self.pending, 1, u32::MAX as usize, budget)?;
        self.pending.push(Scalar { value, order });
        Ok(())
    }

    fn compose(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let begin = usize::from(class(self.pending[0].value) == 0);
        // the original order makes unstable sorting stable for equal classes.
        self.pending[begin..].sort_unstable_by_key(|scalar| (class(scalar.value), scalar.order));
        if begin == 0 {
            return;
        }
        let mut write = 1;
        let mut previous = 0;
        for read in 1..self.pending.len() {
            let scalar = self.pending[read];
            let current = class(scalar.value);
            if previous == 0 || previous < current {
                if let Some(joined) = compose(self.pending[0].value, scalar.value) {
                    self.pending[0].value = joined;
                    continue;
                }
            }
            self.pending[write] = scalar;
            write += 1;
            previous = current;
        }
        self.pending.truncate(write);
    }

    fn emit<F>(&mut self, emit: &mut F, budget: &mut MemoryBudget) -> Result<()>
    where
        F: FnMut(char, &mut MemoryBudget) -> Result<()>,
    {
        for scalar in &self.pending {
            emit(scalar.value, budget)?;
        }
        self.pending.clear();
        Ok(())
    }

    fn finish<F>(&mut self, emit: &mut F, budget: &mut MemoryBudget) -> Result<()>
    where
        F: FnMut(char, &mut MemoryBudget) -> Result<()>,
    {
        self.compose();
        self.emit(emit, budget)
    }

    fn release(self, budget: &mut MemoryBudget) -> Result<()> {
        let bytes = self.pending.capacity() * std::mem::size_of::<Scalar>();
        drop(self);
        budget.release(bytes)?;
        Ok(())
    }
}

fn fold(value: char) -> Result<char> {
    unicode_case_mapping::case_folded(value).map_or(Ok(value), |mapped| {
        char::from_u32(mapped.get()).ok_or(Error::InvalidProfile)
    })
}

pub(crate) fn profile_text(text: &str, limit: usize, budget: &mut MemoryBudget) -> Result<(String, usize)> {
    let mut bytes = Vec::new();
    reserve(&mut bytes, text.len().min(limit), limit, budget)?;
    if text.is_ascii() {
        if text.len() > limit {
            return Err(Error::Limit("normalized bytes"));
        }
        bytes.extend(text.bytes().map(|byte| byte.to_ascii_lowercase()));
        let peak = budget.used();
        return Ok((String::from_utf8(bytes).map_err(|_| Error::InvalidDocument)?, peak));
    }
    let mut first = Nfc::default();
    let mut second = Nfc::default();
    let mut output = |value: char, budget: &mut MemoryBudget| {
        let mut encoded = [0; 4];
        let encoded = value.encode_utf8(&mut encoded);
        reserve(&mut bytes, encoded.len(), limit, budget)?;
        bytes.extend_from_slice(encoded.as_bytes());
        Ok(())
    };
    {
        let mut folded = |value: char, budget: &mut MemoryBudget| second.push(fold(value)?, &mut output, budget);
        for value in text.chars() {
            first.push(value, &mut folded, budget)?;
        }
        first.finish(&mut folded, budget)?;
    }
    second.finish(&mut output, budget)?;
    let peak = budget.used();
    first.release(budget)?;
    second.release(budget)?;
    Ok((String::from_utf8(bytes).map_err(|_| Error::InvalidDocument)?, peak))
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_normalization::UnicodeNormalization;

    #[test]
    fn nfc_matches_independent_iterator_for_every_scalar() {
        for code in 0..=0x10ffff {
            let Some(value) = char::from_u32(code) else { continue };
            let mut budget = MemoryBudget::new(4096);
            let mut state = Nfc::default();
            let mut actual = String::new();
            let mut emit = |value, _: &mut MemoryBudget| { actual.push(value); Ok(()) };
            state.push(value, &mut emit, &mut budget).unwrap();
            state.finish(&mut emit, &mut budget).unwrap();
            assert_eq!(actual, value.to_string().nfc().collect::<String>(), "{code:x}");
            state.release(&mut budget).unwrap();
            assert_eq!(budget.used(), 0);
        }
    }
}
