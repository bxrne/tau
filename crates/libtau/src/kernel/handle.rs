//! Capabilities. A [`Handle`] is the only way to address a kernel resource — holding
//! one is the grant, and there is no other path to a resource. Handles are minted
//! solely by the kernel, so they can't be forged.

use std::marker::PhantomData;

/// Type-erased resource identity: an index into the kernel's slab plus the
/// generation it was live at, so a handle to a removed resource is detectably
/// stale rather than silently aliasing its replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RawHandle {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl RawHandle {
    /// Create a new raw handle (for testing only).
    #[cfg(test)]
    pub fn new(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }
}

/// A typed capability to a resource of type `T`. `Copy`, cheap, and inert on its
/// own — it does nothing until used with a syscall. The phantom is `fn() -> T` so
/// the handle is `Send`/`Sync`/`Copy` regardless of `T`.
pub struct Handle<T: ?Sized> {
    pub(crate) raw: RawHandle,
    _pd: PhantomData<fn() -> T>,
}

impl<T: ?Sized> Handle<T> {
    pub(crate) fn new(raw: RawHandle) -> Self {
        Handle {
            raw,
            _pd: PhantomData,
        }
    }

    /// The underlying type-erased identity, e.g. to key host-side I/O handlers.
    pub fn raw(self) -> RawHandle {
        self.raw
    }
}

// Manual impls: deriving would wrongly demand `T: Clone`/`Copy`/`Debug`.
impl<T: ?Sized> Clone for Handle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: ?Sized> Copy for Handle<T> {}
impl<T: ?Sized> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Handle<{}>({:?})", std::any::type_name::<T>(), self.raw)
    }
}
impl<T: ?Sized> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<T: ?Sized> Eq for Handle<T> {}

/// Type alias for dynamic handles (when you don't know the concrete type at compile time).
pub type Capability = Handle<dyn std::any::Any>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_equality() {
        let raw = RawHandle::new(1, 0);
        let h1 = Handle::<()>::new(raw);
        let h2 = Handle::<()>::new(raw);
        assert_eq!(h1, h2);
    }

    #[test]
    fn handle_inequality() {
        let h1 = Handle::<()>::new(RawHandle::new(1, 0));
        let h2 = Handle::<()>::new(RawHandle::new(2, 0));
        assert_ne!(h1, h2);
    }

    #[test]
    fn stale_handle_detection() {
        let h1 = Handle::<()>::new(RawHandle::new(1, 0));
        let h2 = Handle::<()>::new(RawHandle::new(1, 1)); // same index, different generation
        assert_ne!(h1, h2);
    }
}
