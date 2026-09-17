// fallible owned buffers with checked capacity accounting.
// callers abort the whole operation on error; allocator overhead is not observable here.
// contract: https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve_exact

use std::mem::size_of;
use crate::budget::{BudgetError, MemoryBudget};
use crate::error::{Error, Result};

pub(crate) fn reserve<T>(values: &mut Vec<T>, additional: usize, limit: usize, budget: &mut MemoryBudget) -> Result<()> {
    let required = values.len().checked_add(additional).ok_or(Error::Limit("element count"))?;
    if required > limit { return Err(Error::Limit("element count")); }
    let old = values.capacity();
    if required <= old { return Ok(()); }
    let mut target = old.saturating_mul(2).max(required).min(limit);
    let bytes = |capacity: usize| (capacity - old).checked_mul(size_of::<T>());
    if bytes(target).is_none_or(|n| n > budget.remaining()) { target = required; }
    if bytes(target).is_none_or(|n| n > budget.remaining()) {
        return Err(BudgetError::LimitExceeded.into());
    }
    values.try_reserve_exact(target - values.len()).map_err(|_| Error::Allocation)?;
    budget.charge_array::<T>(values.capacity() - old)?;
    Ok(())
}

pub(crate) fn vector<T>(capacity: usize, budget: &mut MemoryBudget) -> Result<Vec<T>> {
    let mut values = Vec::new();
    reserve(&mut values, capacity, capacity, budget)?;
    Ok(values)
}

pub(crate) fn copy_text(text: &str, budget: &mut MemoryBudget) -> Result<String> {
    let mut bytes = vector(text.len(), budget)?;
    bytes.extend_from_slice(text.as_bytes());
    String::from_utf8(bytes).map_err(|_| Error::InvalidDocument)
}

pub(crate) struct Work {
    remaining: usize,
}

impl Work {
    pub(crate) const fn new(limit: usize) -> Self { Self { remaining: limit } }

    pub(crate) fn charge(&mut self, units: usize) -> Result<()> {
        self.remaining = self.remaining.checked_sub(units).ok_or(Error::Limit("search work"))?;
        Ok(())
    }
}
