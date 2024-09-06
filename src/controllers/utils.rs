use std::sync::Arc;

use kube::ResourceExt;

#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub enum ResourceState {
    NotSeenBefore,
    Existing,
    Deleted,
}

impl ResourceState {
    pub fn check_state(
        resource: &impl ResourceExt,
        maybe_added: Option<&Arc<impl ResourceExt>>,
    ) -> ResourceState {
        if !resource.finalizers().is_empty() && resource.meta().deletion_timestamp.is_some() {
            return ResourceState::Deleted;
        }

        if let Some(stored_object) = maybe_added {
            if stored_object.meta().resource_version == resource.meta().resource_version {
                return ResourceState::Existing;
            }
        }

        ResourceState::NotSeenBefore
    }
}

pub struct VerifiyItems;

impl VerifiyItems {
    #[allow(clippy::unwrap_used)]
    pub fn verify<I, E>(iter: impl Iterator<Item = std::result::Result<I, E>>) -> (Vec<I>, Vec<E>)
    where
        I: std::fmt::Debug,
        E: std::fmt::Debug,
    {
        let (good, bad): (Vec<_>, Vec<_>) = iter.partition(std::result::Result::is_ok);
        let good: Vec<_> = good.into_iter().map(|i| i.unwrap()).collect();
        let bad: Vec<_> = bad.into_iter().map(|i| i.unwrap_err()).collect();
        (good, bad)
    }
}
