use crate::{array::Offset, bitmap::utils::ZipValidity, trusted_len::TrustedLen};

use super::BinaryArray;

/// Iterator over slices of `&[u8]`.
#[derive(Debug, Clone)]
pub struct BinaryValueIter<'a, O: Offset> {
    array: &'a BinaryArray<O>,
    index: usize,
}

impl<'a, O: Offset> BinaryValueIter<'a, O> {
    /// Creates a new [`BinaryValueIter`]
    pub fn new(array: &'a BinaryArray<O>) -> Self {
        Self { array, index: 0 }
    }
}

impl<'a, O: Offset> Iterator for BinaryValueIter<'a, O> {
    type Item = &'a [u8];

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.array.len() {
            return None;
        } else {
            self.index += 1;
        }
        Some(unsafe { self.array.value_unchecked(self.index - 1) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (
            self.array.len() - self.index,
            Some(self.array.len() - self.index),
        )
    }
}

impl<'a, O: Offset> IntoIterator for &'a BinaryArray<O> {
    type Item = Option<&'a [u8]>;
    type IntoIter = ZipValidity<'a, &'a [u8], BinaryValueIter<'a, O>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

unsafe impl<O: Offset> TrustedLen for BinaryValueIter<'_, O> {}
