//! Entity model: the common header every entity carries, entity kinds, and
//! flags. See ARCHITECTURE.md §5. Schema/aux type bodies (EntityAttribute,
//! EntityType, Subject, Layer, LayerGroup, VariantList, Role, LinkType,
//! Link) build on top of this header and are added incrementally as the
//! query/index layers land.

pub mod entity_attribute;
pub mod entity_type;
pub mod flags;
pub mod header;
pub mod kind;

pub use entity_attribute::EntityAttribute;
pub use entity_type::EntityType;
pub use flags::EntityFlags;
pub use header::EntityHeader;
pub use kind::EntityKind;
