// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

/// Internal block-completion bits. Rust's Vec<bool> uses a byte per entry;
/// keeping eight entries per byte matters for large seeding catalogs.
#[derive(Default, Debug, Clone)]
pub(super) struct BlockBitfield {
    bytes: Vec<u8>,
    len: usize,
}

impl BlockBitfield {
    pub fn new(len: usize, value: bool) -> Self {
        Self {
            bytes: vec![if value { 255 } else { 0 }; len.div_ceil(8)],
            len,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn get(&self, index: usize) -> Option<&bool> {
        (index < self.len).then(|| {
            if self.bytes[index / 8] & (1 << (index % 8)) != 0 {
                &true
            } else {
                &false
            }
        })
    }

    pub fn set(&mut self, index: usize, value: bool) {
        assert!(index < self.len, "block index out of bounds");
        let mask = 1 << (index % 8);
        if value {
            self.bytes[index / 8] |= mask;
        } else {
            self.bytes[index / 8] &= !mask;
        }
    }
}

impl std::ops::Index<usize> for BlockBitfield {
    type Output = bool;
    fn index(&self, index: usize) -> &bool {
        self.get(index).expect("block index out of bounds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn packed_bits_match_byte_flags(len in 0usize..4096, initial in any::<bool>(),
            updates in prop::collection::vec((any::<usize>(), any::<bool>()), 0..512)) {
            let mut bits = BlockBitfield::new(len, initial);
            let mut reference = vec![initial; len];
            for (index, value) in updates {
                if len != 0 {
                    let index = index % len;
                    reference[index] = value;
                    bits.set(index, value);
                }
            }
            prop_assert_eq!(bits.len(), len);
            prop_assert_eq!(bits.get(len), None);
            let cloned = bits.clone();
            for (index, value) in reference.iter().enumerate() {
                prop_assert_eq!(bits.get(index), Some(value));
                prop_assert_eq!(cloned[index], *value);
            }
            prop_assert_eq!(bits.bytes.len(), len.div_ceil(8));
        }
    }

    proptest! {
        #[test]
        fn updates_and_resets_match_after_every_operation(
            operations in prop::collection::vec((any::<bool>(), any::<usize>(), any::<bool>()), 1..128)
        ) {
            let mut bits = BlockBitfield::default();
            let mut reference = Vec::new();
            for (reset, index, value) in operations {
                if reset {
                    let len = index % 513;
                    bits = BlockBitfield::new(len, value);
                    reference = vec![value; len];
                } else if !reference.is_empty() {
                    let index = index % reference.len();
                    bits.set(index, value);
                    reference[index] = value;
                }
                prop_assert_eq!(bits.len(), reference.len());
                prop_assert_eq!(bits.get(reference.len()), None);
                prop_assert_eq!(bits.get(usize::MAX), None);
                for (index, value) in reference.iter().enumerate() {
                    prop_assert_eq!(bits[index], *value);
                }
            }
        }
    }

    #[test]
    fn large_arrays_preserve_neighbors_and_clones() {
        for len in [65_537, 1_048_579] {
            for initial in [false, true] {
                let mut bits = BlockBitfield::new(len, initial);
                let original = bits.clone();
                let indices = [0, 7, 8, 63, 64, 65_535, 65_536, len - 1];
                for index in indices {
                    bits.set(index, !initial);
                }
                for index in 0..len {
                    assert_eq!(
                        bits[index],
                        if indices.contains(&index) {
                            !initial
                        } else {
                            initial
                        }
                    );
                    assert_eq!(original[index], initial);
                }
                assert_eq!(bits.bytes.len(), len.div_ceil(8));
            }
        }
    }

    #[test]
    fn byte_boundaries_and_padding_are_not_addressable() {
        for len in [0, 1, 7, 8, 9, 63, 64, 65] {
            for initial in [false, true] {
                let mut bits = BlockBitfield::new(len, initial);
                for index in 0..len {
                    bits.set(index, !initial);
                }
                assert!((0..len).all(|index| bits[index] != initial));
                assert_eq!(bits.get(len), None);
                assert_eq!(bits.get(usize::MAX), None);
            }
        }
    }
}
