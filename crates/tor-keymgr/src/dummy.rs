//! A dummy key manager implementation.
//!
//! This key manager implementation is only used when the `keymgr` feature is disabled.
//!
//! The implementations from this module ignore their arguments. The unused arguments can't be
//! removed, because the dummy implementations must have the same API as their fully-featured
//! counterparts.

use crate::{BoxedKeystore, Result};

use fs_mistrust::Mistrust;
use std::any::Any;
use std::path::Path;

/// A dummy key manager implementation.
///
/// This implementation has the same API as the key manager exposed when the `keymgr` feature is
/// enabled, except all its read operations return `None` and all its write operations will fail.
///
/// For operations that normally involve updating the state of the key manager and/or its
/// underlying storage, such as `insert` or `remove`, this `KeyMgr` always returns an error.
#[derive(derive_builder::Builder)]
#[builder(pattern = "owned")]
#[non_exhaustive]
pub struct KeyMgr {
    /// The default key store.
    #[allow(unused)] // Unused, but needed because we want its setter present in the builder
    primary_store: BoxedKeystore,
    /// The secondary key stores.
    #[allow(unused)] // Unused, but needed because we want its setter present in the builder
    #[builder(default, setter(custom))]
    secondary_stores: Vec<BoxedKeystore>,
}

// TODO: auto-generate using define_list_builder_accessors/define_list_builder_helper
// when that becomes possible.
//
// See https://gitlab.torproject.org/tpo/core/arti/-/merge_requests/1760#note_2969841
impl KeyMgrBuilder {
    /// Access the being-built list of secondary stores (resolving default)
    ///
    /// If the field has not yet been set or accessed, the default list will be
    /// constructed and a mutable reference to the now-defaulted list of builders
    /// will be returned.
    pub fn secondary_stores(&mut self) -> &mut Vec<BoxedKeystore> {
        self.secondary_stores.get_or_insert(Default::default())
    }

    /// Set the whole list (overriding the default)
    pub fn set_secondary_stores(mut self, list: Vec<BoxedKeystore>) -> Self {
        self.secondary_stores = Some(list);
        self
    }

    /// Inspect the being-built list (with default unresolved)
    ///
    /// If the list has not yet been set, or accessed, `&None` is returned.
    pub fn opt_secondary_stores(&self) -> &Option<Vec<BoxedKeystore>> {
        &self.secondary_stores
    }

    /// Mutably access the being-built list (with default unresolved)
    ///
    /// If the list has not yet been set, or accessed, `&mut None` is returned.
    pub fn opt_secondary_stores_mut(&mut self) -> &mut Option<Vec<BoxedKeystore>> {
        &mut self.secondary_stores
    }
}

/// A dummy key store trait.
pub trait Keystore: Send + Sync + 'static {
    // NOTE: resist the temptation to add additional functions here!
    //
    // If your code does not compile with the `tor-keymgr/keymgr` feature disabled
    // because this trait is missing some functions you are using/implementing,
    // the correct answer is very likely to feature-gate the offending code,
    // or arrange for the calling crate to unconditionally enable `tor-keymgr/keymgr`,
    // rather than to extend this trait to match the interface of the `Keystore` trait
    // exposed when the `tor-keymgr/keymgr` feature is enabled.
    //
    // See the note in the dummy `KeyMgr` impl block below for more details.
}

/// A dummy `ArtiNativeKeystore`.
#[non_exhaustive]
pub struct ArtiNativeKeystore;

impl ArtiNativeKeystore {
    /// Create a new [`ArtiNativeKeystore`].
    #[allow(clippy::unnecessary_wraps)]
    pub fn from_path_and_mistrust(_: impl AsRef<Path>, _: &Mistrust) -> Result<Self> {
        Ok(Self)
    }
}

impl Keystore for ArtiNativeKeystore {}

/// A dummy `ArtiEphemeralKeystore`.
#[non_exhaustive]
pub struct ArtiEphemeralKeystore;

impl Keystore for ArtiEphemeralKeystore {}

impl ArtiEphemeralKeystore {
    /// Create a new [`ArtiEphemeralKeystore`]
    #[allow(clippy::unnecessary_wraps)]
    pub fn new(_: String) -> Self {
        Self
    }
}

impl KeyMgr {
    /// A dummy `get` implementation that always behaves like the requested key is not found.
    ///
    /// This function always returns `Ok(None)`.
    pub fn get<K>(&self, _: &dyn Any) -> Result<Option<K>> {
        Ok(None)
    }

    // NOTE: resist the temptation to add additional functions here!
    //
    // If your code does not compile with the `tor-keymgr/keymgr` feature disabled
    // because this impl is missing some functions you are using,
    // the correct answer is very likely to feature-gate the offending code,
    // or arrange for the calling crate to unconditionally enable `tor-keymgr/keymgr`,
    // rather than to extend this impl to match the interface of the real `KeyMgr`
    // (exposed when the `tor-keymgr/keymgr` feature is enabled).
    //
    // The dummy `KeyMgr` (and the dummy keystores) and the fully fledged
    // `KeyMgr`/`Keystore` implementations gated behind the `keymgr` feature
    // are **not** supposed to have the same interface.
    // This is because implementations needing a real `KeyMgr`
    // to function shouldn't even compile if the real `KeyMgr` is disabled.
    // We could have provided an API here that's identical to the real one,
    // with the dummy implementation always returning an error,
    // but that would be strictly worse, because the user of this code
    // would only find out at *runtime* about what is essentially a *build* issue
    // (the build issue being that the application was built with an incoherent feature set).
}

inventory::collect!(&'static dyn crate::KeyPathInfoExtractor);
