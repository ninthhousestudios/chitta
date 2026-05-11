//! Tool handlers and shared argument validators.
//!
//! Each tool lives in its own submodule with `Args` + `Output` types
//! (`serde::Deserialize` / `Serialize`) and one async `handle` fn.

pub mod delete;
pub mod get;
pub mod get_profile;
pub mod health;
pub mod list;
pub mod record_feedback;
pub mod reflect_status;
pub mod search;
pub mod store;
pub mod supersede;
pub mod update;
pub mod validate;

pub use delete::{DeleteArgs, DeleteOutput};
pub use get::{GetArgs, GetOutput};
pub use get_profile::{GetProfileArgs, GetProfileOutput};
pub use health::{HealthArgs, HealthOutput};
pub use list::{ListArgs, ListItem, ListOutput};
pub use record_feedback::{FeedbackKind, RecordFeedbackArgs, RecordFeedbackOutput};
pub use reflect_status::{ReflectStatusArgs, ReflectStatusOutput};
pub use search::{AppliesTo, SearchArgs, SearchHit, SearchOutput};
pub use store::{DerivationInput, StoreArgs, StoreOutput};
pub use supersede::{SupersedeArgs, SupersedeOutput};
pub use update::{UpdateArgs, UpdateOutput};
