//! Entity header flags. See ARCHITECTURE.md §5.2 for the rationale behind
//! each one, particularly why `PENDING_DELETE`/`PENDING_RENAME` exist as
//! asynchronous fan-out markers rather than being resolved synchronously.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityFlags(u32);

impl EntityFlags {
    pub const NONE: EntityFlags = EntityFlags(0);

    pub const PENDING_DELETE: EntityFlags = EntityFlags(1 << 0);
    pub const PENDING_RENAME: EntityFlags = EntityFlags(1 << 1);
    pub const DISABLED: EntityFlags = EntityFlags(1 << 2);
    pub const SYSTEM: EntityFlags = EntityFlags(1 << 3);
    pub const IMMUTABLE: EntityFlags = EntityFlags(1 << 4);
    pub const HIDDEN: EntityFlags = EntityFlags(1 << 5);
    pub const TOMBSTONE: EntityFlags = EntityFlags(1 << 6);
    pub const CONFLICT: EntityFlags = EntityFlags(1 << 7);

    pub fn bits(&self) -> u32 {
        self.0
    }

    pub fn from_bits(bits: u32) -> Self {
        EntityFlags(bits)
    }

    pub fn contains(&self, flag: EntityFlags) -> bool {
        self.0 & flag.0 == flag.0
    }

    pub fn with(&self, flag: EntityFlags) -> Self {
        EntityFlags(self.0 | flag.0)
    }

    pub fn without(&self, flag: EntityFlags) -> Self {
        EntityFlags(self.0 & !flag.0)
    }

    /// Entities excluded from default (non-admin) query results.
    pub fn is_default_hidden(&self) -> bool {
        self.contains(Self::PENDING_DELETE)
            || self.contains(Self::TOMBSTONE)
            || self.contains(Self::HIDDEN)
    }
}

impl std::ops::BitOr for EntityFlags {
    type Output = EntityFlags;
    fn bitor(self, rhs: Self) -> Self::Output {
        self.with(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combine_and_check() {
        let f = EntityFlags::DISABLED | EntityFlags::SYSTEM;
        assert!(f.contains(EntityFlags::DISABLED));
        assert!(f.contains(EntityFlags::SYSTEM));
        assert!(!f.contains(EntityFlags::TOMBSTONE));
    }

    #[test]
    fn default_hidden_rules() {
        assert!(EntityFlags::TOMBSTONE.is_default_hidden());
        assert!(EntityFlags::PENDING_DELETE.is_default_hidden());
        assert!(!EntityFlags::DISABLED.is_default_hidden());
    }
}
