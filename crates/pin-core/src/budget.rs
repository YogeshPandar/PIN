//! Allocation-free accounting for a caller-owned memory budget.
//! Charge actual retained capacity before allocation; release only after freeing it.
//! This counter does not intercept allocations or promise recoverable process OOM.
//! Rust contract: <https://doc.rust-lang.org/std/primitive.usize.html#method.checked_add>.

/// A rejected accounting operation; failure leaves the counter unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetError {
    ArithmeticOverflow,
    LimitExceeded,
    InvalidRelease,
}

/// A private accounting counter; not an allocator or cross-process primitive.
#[derive(Debug)]
pub struct MemoryBudget {
    limit: usize,
    used: usize,
    peak: usize,
}

impl MemoryBudget {
    pub const fn new(limit: usize) -> Self {
        Self { limit, used: 0, peak: 0 }
    }

    /// Accounts for retained bytes before their allocation.
    ///
    /// # Errors
    /// Rejects overflow or a total above the limit without changing state.
    pub fn charge(&mut self, bytes: usize) -> Result<(), BudgetError> {
        let next = self.used.checked_add(bytes).ok_or(BudgetError::ArithmeticOverflow)?;
        if next > self.limit {
            return Err(BudgetError::LimitExceeded);
        }
        self.used = next;
        self.peak = self.peak.max(next);
        Ok(())
    }

    /// Accounts for `count` elements, including capacity that is not yet filled.
    ///
    /// # Errors
    /// Rejects multiplication overflow or any error from `charge`.
    pub fn charge_array<T>(&mut self, count: usize) -> Result<(), BudgetError> {
        let bytes = count.checked_mul(size_of::<T>()).ok_or(BudgetError::ArithmeticOverflow)?;
        self.charge(bytes)
    }

    /// Releases bytes after their storage has been freed.
    ///
    /// # Errors
    /// Rejects a release larger than the current total without changing state.
    pub fn release(&mut self, bytes: usize) -> Result<(), BudgetError> {
        let next = self.used.checked_sub(bytes).ok_or(BudgetError::InvalidRelease)?;
        self.used = next;
        Ok(())
    }

    pub const fn used(&self) -> usize {
        self.used
    }

    pub const fn peak(&self) -> usize {
        self.peak
    }

    pub const fn remaining(&self) -> usize {
        self.limit - self.used
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_operations_are_atomic() {
        let mut budget = MemoryBudget::new(64);
        budget.charge_array::<u64>(8).unwrap();
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.charge(1), Err(BudgetError::LimitExceeded));
        assert_eq!(budget.release(65), Err(BudgetError::InvalidRelease));
        assert_eq!(budget.used(), 64);
        budget.release(64).unwrap();
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.peak(), 64);
    }

    #[test]
    fn addition_and_multiplication_do_not_wrap() {
        let mut budget = MemoryBudget::new(usize::MAX);
        assert_eq!(budget.charge_array::<u64>(usize::MAX), Err(BudgetError::ArithmeticOverflow));
        budget.charge(usize::MAX).unwrap();
        assert_eq!(budget.charge(1), Err(BudgetError::ArithmeticOverflow));
        assert_eq!(budget.used(), usize::MAX);
    }
}
