/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A drop-in for [`std::time::Instant`] that also works on the Worker WASM
//! target, where `std::time::Instant::now()` panics ("time not implemented on
//! this platform"). There it reads the Worker's monotonic clock through
//! [`crate::cross_process_instant::CrossProcessInstant`].

#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;
#[cfg(target_arch = "wasm32")]
pub use worker::Instant;

#[cfg(target_arch = "wasm32")]
mod worker {
    use std::time::Duration;

    use malloc_size_of_derive::MallocSizeOf;

    use crate::cross_process_instant::CrossProcessInstant;

    /// Monotonic nanoseconds since the Worker clock's epoch.
    #[derive(Clone, Copy, Debug, Eq, Hash, MallocSizeOf, Ord, PartialEq, PartialOrd)]
    pub struct Instant(u64);

    impl Instant {
        pub fn now() -> Self {
            Self(
                (CrossProcessInstant::now() - CrossProcessInstant::epoch())
                    .whole_nanoseconds()
                    .max(0) as u64,
            )
        }

        pub fn duration_since(&self, earlier: Self) -> Duration {
            *self - earlier
        }

        pub fn elapsed(&self) -> Duration {
            Self::now() - *self
        }
    }

    impl std::ops::Add<Duration> for Instant {
        type Output = Self;

        fn add(self, rhs: Duration) -> Self {
            Self(self.0.saturating_add(rhs.as_nanos() as u64))
        }
    }

    impl std::ops::Sub<Duration> for Instant {
        type Output = Self;

        fn sub(self, rhs: Duration) -> Self {
            Self(self.0.saturating_sub(rhs.as_nanos() as u64))
        }
    }

    impl std::ops::Sub for Instant {
        type Output = Duration;

        fn sub(self, rhs: Self) -> Duration {
            Duration::from_nanos(self.0.saturating_sub(rhs.0))
        }
    }
}
