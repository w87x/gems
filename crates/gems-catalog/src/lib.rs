//! Entity model: the common header every entity carries, entity kinds, and
//! flags, plus every schema/aux entity body (ARCHITECTURE.md §5):
//! `EntityAttribute`, `EntityType` (data-entity schema); `Subject`,
//! `Layer`/`LayerGroup`, `VariantList`, `Role`, `LinkType`/`Link` (aux
//! types). `Data` entity bodies themselves are `gems-codec`'s GBV maps,
//! validated against an `EntityType` by the layer that owns write paths
//! (not this crate, which only defines the shapes).

pub mod bootstrap;
pub mod entity_attribute;
pub mod entity_type;
pub mod flags;
pub mod header;
pub mod kind;
pub mod layer;
pub mod link;
pub mod policy;
pub mod subject;
mod util;

pub use entity_attribute::EntityAttribute;
pub use entity_type::{AttributeRef, EntityType, EntityTypeKind};
pub use flags::EntityFlags;
pub use header::EntityHeader;
pub use kind::EntityKind;
pub use layer::{Layer, LayerGroup, Role, VariantList};
pub use link::{Link, LinkTemporality, LinkType};
pub use policy::{Effect, Policy, SubjectPredicate, TargetPredicate};
pub use subject::{Credentials, Subject, SubjectKind};
