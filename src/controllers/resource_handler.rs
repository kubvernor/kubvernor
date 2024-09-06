// use std::sync::Arc;

// use kube::{runtime::controller::Action, ResourceExt};

// use super::{utils::ResourceState, ControllerError};

// type Result<T, E = ControllerError> = std::result::Result<T, E>;

// trait ResourceHandler<T>
// where
//     T: ResourceExt,
// {
//     fn process(&self) -> Result<Action> {
//         match resource_state {
//             ResourceState::NotSeenBefore => todo!(),
//             ResourceState::Existing => todo!(),
//             ResourceState::Deleted => todo!(),
//         }
//     }

//     fn not_seen_before(&self) {}

//     fn existing(&self) {}

//     fn deleted(&self) {}
// }
