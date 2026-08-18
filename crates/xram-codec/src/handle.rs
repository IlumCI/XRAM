//! Packed reference to a stored page.
//!
//! One handle exists per resident compressed page, and the tier map holds one
//! per page frame, so its width is a real cost: at 4 KiB pages, every byte here
//! is 256 MiB of metadata per TiB of logical capacity. Hence 32 bits rather
//! than a pointer.

/// Bits reserved for the size class.
const CLASS_BITS: u32 = 7;
/// Bits reserved for the slot index within that class.
const SLOT_BITS: u32 = 32 - CLASS_BITS;
const SLOT_MASK: u32 = (1 << SLOT_BITS) - 1;

/// Largest addressable slot index within one size class.
pub const MAX_SLOT: u32 = SLOT_MASK;
/// Number of real (storage-backed) size classes.
pub const MAX_CLASSES: usize = (1 << CLASS_BITS) - 1;

/// Reserved class meaning "every byte of this page is the same value".
///
/// Same-fill pages - all-zero above all, but also all-`0xFF` and the like - are
/// common in anonymous memory, and they cost *no* storage: the byte itself
/// rides in the handle. zram carries the same special case for the same reason.
const CLASS_SAME_FILL: u32 = MAX_CLASSES as u32;

/// A stored page: either a slot in a size class, or a same-fill page held
/// entirely within the handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Handle(u32);

impl Handle {
    pub(crate) fn slot(class: usize, slot: u32) -> Self {
        debug_assert!(class < MAX_CLASSES);
        debug_assert!(slot <= MAX_SLOT);
        Self(((class as u32) << SLOT_BITS) | (slot & SLOT_MASK))
    }

    pub(crate) fn same_fill(byte: u8) -> Self {
        Self((CLASS_SAME_FILL << SLOT_BITS) | byte as u32)
    }

    pub(crate) fn class(self) -> u32 {
        self.0 >> SLOT_BITS
    }

    pub(crate) fn slot_index(self) -> u32 {
        self.0 & SLOT_MASK
    }

    /// The fill byte, if this page was stored as same-fill.
    pub fn fill_byte(self) -> Option<u8> {
        (self.class() == CLASS_SAME_FILL).then_some((self.0 & 0xFF) as u8)
    }

    /// True when this page occupies no pool storage at all.
    pub fn is_same_fill(self) -> bool {
        self.class() == CLASS_SAME_FILL
    }

    pub fn to_bits(self) -> u32 {
        self.0
    }

    /// Rebuilds a handle from its packed form.
    ///
    /// Only meaningful for bits produced by [`Handle::to_bits`] on the same
    /// pool; passing anything else will resolve to an unrelated slot or fail.
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_handles_round_trip() {
        for class in [0usize, 1, 63, MAX_CLASSES - 1] {
            for slot in [0u32, 1, 4095, MAX_SLOT] {
                let h = Handle::slot(class, slot);
                assert_eq!(h.class(), class as u32, "class {class} slot {slot}");
                assert_eq!(h.slot_index(), slot, "class {class} slot {slot}");
                assert!(!h.is_same_fill());
                assert_eq!(h.fill_byte(), None);
                assert_eq!(Handle::from_bits(h.to_bits()), h);
            }
        }
    }

    #[test]
    fn same_fill_handles_carry_the_byte_and_no_storage() {
        for b in [0u8, 1, 0x7F, 0xFF] {
            let h = Handle::same_fill(b);
            assert!(h.is_same_fill());
            assert_eq!(h.fill_byte(), Some(b));
            assert_eq!(Handle::from_bits(h.to_bits()), h);
        }
    }

    #[test]
    fn same_fill_class_cannot_collide_with_a_real_class() {
        // The reserved class sits immediately above the last real one, so a
        // storage-backed handle can never be mistaken for same-fill.
        assert_eq!(CLASS_SAME_FILL, MAX_CLASSES as u32);
        assert!(!Handle::slot(MAX_CLASSES - 1, MAX_SLOT).is_same_fill());
    }
}
