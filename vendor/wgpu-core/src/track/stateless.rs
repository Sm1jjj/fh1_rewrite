use alloc::{sync::Arc, vec::Vec};
use core::slice::Iter;

/// A tracker that holds strong references to resources.
///
/// This is only used to keep resources alive.
#[derive(Debug)]
pub(crate) struct StatelessTracker<T> {
    resources: Vec<Arc<T>>,
    /// FH1 patch 3: position in `resources` per key (tracker index) for `insert_single_once`.
    positions: crate::FastHashMap<usize, usize>,
}

impl<T> StatelessTracker<T> {
    pub fn new() -> Self {
        Self {
            resources: Vec::new(),
            positions: crate::FastHashMap::default(),
        }
    }

    /// FH1 patch 3 (vendor/wgpu-core/FH1_PATCHES.md): `insert_single`, but a resource already inserted under `key` (its
    /// tracker index, unique while this tracker holds it alive) is not pushed again. The tracker only keeps resources
    /// alive and lets submit validate each one (`validate_command_buffer` walks every bind group's buffers and
    /// textures), so one entry per resource is equivalent; the duplicates made submit re-walk a bindless material slab's
    /// hundreds of textures for every `set_bind_group` of it.
    pub fn insert_single_once(&mut self, resource: Arc<T>, key: usize) -> &Arc<T> {
        let at = match self.positions.get(&key) {
            Some(&at) => at,
            None => {
                self.positions.insert(key, self.resources.len());
                self.resources.push(resource);
                self.resources.len() - 1
            }
        };
        &self.resources[at]
    }

    /// Inserts a single resource into the resource tracker.
    ///
    /// Returns a reference to the newly inserted resource.
    /// (This allows avoiding a clone/reference count increase in many cases.)
    pub fn insert_single(&mut self, resource: Arc<T>) -> &Arc<T> {
        self.resources.push(resource);
        unsafe { self.resources.last().unwrap_unchecked() }
    }
}

impl<'a, T> IntoIterator for &'a StatelessTracker<T> {
    type Item = &'a Arc<T>;
    type IntoIter = Iter<'a, Arc<T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.resources.as_slice().iter()
    }
}
