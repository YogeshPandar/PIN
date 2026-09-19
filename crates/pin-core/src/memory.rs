// fallible owned buffers with checked capacity accounting.
// callers abort the whole operation on error; allocator overhead is not observable here.
// contract: https://doc.rust-lang.org/std/vec/struct.Vec.html#method.try_reserve_exact

use crate::budget::{BudgetError, MemoryBudget};
use crate::error::{Error, Result};
use std::mem::size_of;

pub(crate) fn reserve<T>(
    values: &mut Vec<T>,
    additional: usize,
    limit: usize,
    budget: &mut MemoryBudget,
) -> Result<()> {
    let required = values
        .len()
        .checked_add(additional)
        .ok_or(Error::Limit("element count"))?;
    if required > limit {
        return Err(Error::Limit("element count"));
    }
    let old = values.capacity();
    if required <= old {
        return Ok(());
    }
    let mut target = old.saturating_mul(2).max(required).min(limit);
    let bytes = |capacity: usize| (capacity - old).checked_mul(size_of::<T>());
    if bytes(target).is_none_or(|n| n > budget.remaining()) {
        target = required;
    }
    if bytes(target).is_none_or(|n| n > budget.remaining()) {
        return Err(BudgetError::LimitExceeded.into());
    }
    values
        .try_reserve_exact(target - values.len())
        .map_err(|_| Error::Allocation)?;
    budget.charge_array::<T>(values.capacity() - old)?;
    Ok(())
}

pub(crate) fn vector<T>(capacity: usize, budget: &mut MemoryBudget) -> Result<Vec<T>> {
    let mut values = Vec::new();
    reserve(&mut values, capacity, capacity, budget)?;
    Ok(values)
}

// release actual capacity only after the owning allocation has been dropped.
pub(crate) fn release<T>(values: Vec<T>, budget: &mut MemoryBudget) -> Result<()> {
    let bytes = values.capacity().checked_mul(size_of::<T>())
        .ok_or(Error::Limit("allocation bytes"))?;
    drop(values);
    budget.release(bytes)?;
    Ok(())
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
    pub(crate) const fn new(limit: usize) -> Self {
        Self { remaining: limit }
    }

    pub(crate) fn charge(&mut self, units: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(units)
            .ok_or(Error::Limit("search work"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_release_returns_capacity_without_resetting_peak() {
        let mut budget = MemoryBudget::new(4096);
        let first: Vec<u64> = vector(16, &mut budget).unwrap();
        let second: Vec<u8> = vector(7, &mut budget).unwrap();
        let retained = second.capacity();
        let peak = budget.peak();
        release(first, &mut budget).unwrap();
        assert_eq!(budget.used(), retained);
        assert_eq!(budget.peak(), peak);
        release(second, &mut budget).unwrap();
        release(Vec::<u8>::new(), &mut budget).unwrap();
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.peak(), peak);
    }
}
