use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
#[cfg(feature = "enable-serde")]
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::convert::TryInto;
use std::fmt;
use std::ops::{Add, Sub};
use thiserror::Error;

/// WebAssembly page sizes are fixed to be 64KiB.
/// Note: large page support may be added in an opt-in manner in the [future].
///
/// [future]: https://webassembly.org/docs/future-features/#large-page-support
pub const WASM_PAGE_SIZE: usize = 0x10000;

/// The number of pages we can have before we run out of byte index space.
pub const WASM_MAX_PAGES: u32 = 0x10000;

/// The minimum number of pages allowed.
pub const WASM_MIN_PAGES: u32 = 0x100;

/// Units of WebAssembly pages (as specified to be 65,536 bytes).
#[derive(
    Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, RkyvSerialize, RkyvDeserialize, Archive,
)]
#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "artifact-size", derive(loupe::MemoryUsage))]
#[rkyv(derive(Debug), compare(PartialEq, PartialOrd))]
pub struct Pages(u32);

impl Pages {
    /// Zero WebAssembly pages.
    pub const ZERO: Self = Self(0);

    /// One WebAssembly page.
    pub const ONE: Self = Self(1);

    /// Creates a `Pages` value from a page count, rejecting counts above [`WASM_MAX_PAGES`].
    pub const fn try_from_pages(pages: u32) -> Result<Self, PagesError> {
        if pages <= WASM_MAX_PAGES {
            Ok(Self(pages))
        } else {
            Err(PagesError::PageCountOutOfRange)
        }
    }

    /// Returns the largest value that can be represented by the Pages type.
    ///
    /// This is defined by the WebAssembly standard as 65,536 pages.
    #[inline(always)]
    pub const fn max_value() -> Self {
        Self(WASM_MAX_PAGES)
    }

    /// Checked addition. Computes `self + rhs`,
    /// returning `None` if overflow occurred.
    pub fn checked_add(self, rhs: Self) -> Option<Self> {
        let added = (self.0 as usize) + (rhs.0 as usize);
        if added <= (WASM_MAX_PAGES as usize) {
            Some(Self(added as u32))
        } else {
            None
        }
    }

    /// Get number of pages.
    pub fn pages(self) -> u32 {
        self.0
    }

    /// Calculate number of bytes from pages, saturating at `usize::MAX` on 32-bit targets.
    pub fn bytes(self) -> Bytes {
        self.into()
    }

    /// Calculates the number of pages needed to hold the given number of bytes,
    /// rounding up to include any partial page.
    /// Returns [`PagesError::PageCountOutOfRange`] if the result exceeds [`WASM_MAX_PAGES`].
    pub fn from_bytes_rounded_up(bytes: u64) -> Result<Self, PagesError> {
        let pages = bytes.div_ceil(WASM_PAGE_SIZE as u64);
        let pages = u32::try_from(pages).map_err(|_| PagesError::PageCountOutOfRange)?;
        Self::try_from_pages(pages)
    }
}

impl fmt::Debug for Pages {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} pages", self.0)
    }
}

/// Units of WebAssembly memory in terms of 8-bit bytes.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub struct Bytes(pub usize);

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

impl From<Pages> for Bytes {
    fn from(pages: Pages) -> Self {
        // At the maximum page count, 4 GiB is one byte more than a 32-bit usize can hold.
        Self(
            (pages.0 as usize)
                .checked_mul(WASM_PAGE_SIZE)
                .unwrap_or(usize::MAX),
        )
    }
}

impl From<usize> for Bytes {
    fn from(other: usize) -> Self {
        Self(other)
    }
}

impl From<u32> for Bytes {
    fn from(other: u32) -> Self {
        Self(other.try_into().unwrap())
    }
}

/// An error when constructing a [`Pages`] value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PagesError {
    /// The byte count is not an exact multiple of a WebAssembly page.
    #[error("Byte count is not a multiple of the WebAssembly page size")]
    Unaligned,
    /// The page count exceeds [`WASM_MAX_PAGES`].
    #[error("Number of pages exceeds the WebAssembly maximum")]
    PageCountOutOfRange,
}

impl TryFrom<Bytes> for Pages {
    type Error = PagesError;

    /// Converts an exact multiple of the WebAssembly page size to pages.
    fn try_from(bytes: Bytes) -> Result<Self, Self::Error> {
        if !bytes.0.is_multiple_of(WASM_PAGE_SIZE) {
            return Err(PagesError::Unaligned);
        }
        let pages = (bytes.0 / WASM_PAGE_SIZE)
            .try_into()
            .map_err(|_| PagesError::PageCountOutOfRange)?;
        Self::try_from_pages(pages)
    }
}

impl<T> Sub<T> for Bytes
where
    T: Into<Self>,
{
    type Output = Self;
    fn sub(self, rhs: T) -> Self {
        Self(self.0 - rhs.into().0)
    }
}

impl<T> Add<T> for Bytes
where
    T: Into<Self>,
{
    type Output = Self;
    fn add(self, rhs: T) -> Self {
        Self(self.0 + rhs.into().0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_bytes_rounded_up() {
        let max_bytes = u64::from(WASM_MAX_PAGES) * WASM_PAGE_SIZE as u64;
        assert_eq!(Pages::from_bytes_rounded_up(0), Ok(Pages::ZERO));
        assert_eq!(Pages::from_bytes_rounded_up(1), Ok(Pages::ONE));
        assert_eq!(
            Pages::from_bytes_rounded_up(max_bytes),
            Ok(Pages::max_value())
        );
        for bytes in [max_bytes + 1, u64::MAX] {
            assert_eq!(
                Pages::from_bytes_rounded_up(bytes),
                Err(PagesError::PageCountOutOfRange)
            );
        }
    }

    #[test]
    fn convert_max_pages_to_bytes() {
        #[cfg(target_pointer_width = "32")]
        assert_eq!(Pages::max_value().bytes(), Bytes(usize::MAX));
        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            Pages::max_value().bytes(),
            Bytes(WASM_MAX_PAGES as usize * WASM_PAGE_SIZE)
        );
    }

    #[test]
    fn convert_pages() {
        assert_eq!(Pages::try_from(Bytes(0)), Ok(Pages::ZERO));
        assert_eq!(Pages::try_from(Bytes(WASM_PAGE_SIZE)), Ok(Pages::ONE));
        assert_eq!(Pages::try_from(Bytes(28 * WASM_PAGE_SIZE)), Ok(Pages(28)));

        assert_eq!(Pages::try_from(Bytes(1)), Err(PagesError::Unaligned));
        assert_eq!(
            Pages::try_from(Bytes(WASM_PAGE_SIZE - 1)),
            Err(PagesError::Unaligned)
        );
        assert_eq!(
            Pages::try_from(Bytes(WASM_PAGE_SIZE + 1)),
            Err(PagesError::Unaligned)
        );

        assert_eq!(Pages::try_from_pages(0), Ok(Pages::ZERO));
        assert_eq!(
            Pages::try_from_pages(WASM_MAX_PAGES),
            Ok(Pages::max_value())
        );
        assert_eq!(
            Pages::try_from_pages(WASM_MAX_PAGES + 1),
            Err(PagesError::PageCountOutOfRange)
        );

        #[cfg(target_pointer_width = "64")]
        {
            let max_bytes = (WASM_MAX_PAGES as usize) * WASM_PAGE_SIZE;
            assert_eq!(Pages::try_from(Bytes(max_bytes)), Ok(Pages::max_value()));
            assert_eq!(
                Pages::try_from(Bytes(max_bytes + WASM_PAGE_SIZE)),
                Err(PagesError::PageCountOutOfRange)
            );
            assert_eq!(
                Pages::try_from(Bytes((u32::MAX as usize + 1) * WASM_PAGE_SIZE)),
                Err(PagesError::PageCountOutOfRange)
            );
        }
    }
}
