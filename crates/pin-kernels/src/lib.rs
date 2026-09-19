//! Allocation-free bitmap kernels over caller-owned, initialized slices.
//! CPU selection is process-local; no PostgreSQL pointers or shared state enter.
//! Contracts and independent review gate: docs/g6-api-evidence.md.

mod scalar;
#[cfg(all(target_arch = "x86_64", not(miri)))]
mod x86_avx2;

/// Explicit CPU policy; automatic selection is an opt-in experiment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuMode {
    Scalar,
    Auto,
    Avx2,
}

/// A bitwise operation; difference means left AND NOT right.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BitmapOp {
    Intersection,
    Union,
    Difference,
}

/// Rejected selection or slice shape; no output is modified on error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KernelError {
    UnsupportedCpu,
    LengthMismatch,
}

#[derive(Clone, Copy, Debug)]
enum Backend {
    Scalar,
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    Avx2,
}

/// A validated dispatch handle, selected once and reusable across batches.
/// Scalar is always available, including under Miri and on non-x86 targets.
#[derive(Clone, Copy, Debug)]
pub struct Kernels {
    backend: Backend,
}

impl Kernels {
    /// Selects the portable implementation without feature detection or allocation.
    pub const fn scalar() -> Self {
        Self {
            backend: Backend::Scalar,
        }
    }

    /// Selects a backend without retaining caller data or changing global policy.
    ///
    /// # Errors
    /// Forced AVX2 fails when this target/runtime cannot execute that backend.
    pub fn select(mode: CpuMode) -> Result<Self, KernelError> {
        if mode == CpuMode::Scalar {
            return Ok(Self::scalar());
        }
        #[cfg(all(target_arch = "x86_64", not(miri)))]
        {
            static AVX2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *AVX2.get_or_init(|| std::arch::is_x86_feature_detected!("avx2")) {
                return Ok(Self {
                    backend: Backend::Avx2,
                });
            }
        }
        if mode == CpuMode::Avx2 {
            Err(KernelError::UnsupportedCpu)
        } else {
            Ok(Self::scalar())
        }
    }

    /// Reports the actual backend, never the automatic-selection request.
    pub const fn mode(self) -> CpuMode {
        match self.backend {
            Backend::Scalar => CpuMode::Scalar,
            #[cfg(all(target_arch = "x86_64", not(miri)))]
            Backend::Avx2 => CpuMode::Avx2,
        }
    }

    /// Combines equally sized word slices without allocation or retained borrows.
    /// Inputs may alias each other; Rust's exclusive output borrow prevents overlap.
    /// No tail bits are invented, masked or interpreted as document membership.
    ///
    /// # Errors
    /// Unequal lengths return `LengthMismatch` before writing any output word.
    pub fn combine(
        self,
        operation: BitmapOp,
        left: &[u64],
        right: &[u64],
        output: &mut [u64],
    ) -> Result<(), KernelError> {
        if left.len() != right.len() || left.len() != output.len() {
            return Err(KernelError::LengthMismatch);
        }
        match operation {
            BitmapOp::Intersection => self.apply::<false, false>(left, right, output),
            BitmapOp::Union => self.apply::<true, false>(left, right, output),
            BitmapOp::Difference => self.apply::<false, true>(left, right, output),
        }
        Ok(())
    }

    fn apply<const UNION: bool, const DIFFERENCE: bool>(
        self,
        left: &[u64],
        right: &[u64],
        output: &mut [u64],
    ) {
        match self.backend {
            Backend::Scalar => scalar::combine::<UNION, DIFFERENCE>(left, right, output),
            #[cfg(all(target_arch = "x86_64", not(miri)))]
            Backend::Avx2 => {
                // safety: only runtime detection constructs this backend; combine
                // checked equal lengths, and output exclusively borrows live storage.
                unsafe { x86_avx2::combine::<UNION, DIFFERENCE>(left, right, output) }
            }
        }
    }
}
