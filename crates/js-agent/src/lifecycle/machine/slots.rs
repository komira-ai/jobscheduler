//! The slots of the jobs (stub).

/// The slots.
pub struct Slots;

impl Slots {
    /// `count` slots, all free.
    pub fn new(_count: u32) -> Self {
        Self
    }

    /// Whether a slot is free.
    pub fn has_room(&self) -> bool {
        true
    }

    /// Whether `slot` is free.
    pub fn is_free(&self, _slot: u32) -> bool {
        true
    }

    /// The lowest free slot, now held.
    pub fn take(&mut self) -> u32 {
        0
    }

    /// Holds `slot`.
    pub fn hold(&mut self, _slot: u32) -> bool {
        true
    }

    /// Frees `slot`.
    pub fn release(&mut self, _slot: u32) {}

    /// Quarantines `slot`.
    pub fn quarantine(&mut self, _slot: u32) {}

    /// The held slots.
    pub fn held(&self) -> Vec<u32> {
        Vec::new()
    }

    /// The quarantined slots.
    pub fn quarantined(&self) -> Vec<u32> {
        Vec::new()
    }
}
