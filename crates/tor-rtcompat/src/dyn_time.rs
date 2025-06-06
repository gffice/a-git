//! type-erased time provider

use std::future::Future;
use std::mem::{self, MaybeUninit};
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime};

use dyn_clone::DynClone;
use educe::Educe;
use paste::paste;

use crate::{CoarseInstant, CoarseTimeProvider, SleepProvider};

//-------------------- handle PreferredRuntime maybe not existing ----------

// TODO use this more widely, eg in tor-rtcompat/lib.rs

/// See the other implementation
#[allow(unused_macros)] // Will be redefined if there *is* a preferred runtime
macro_rules! if_preferred_runtime {{ [$($y:tt)*] [$($n:tt)*] } => { $($n)* }}
#[cfg(all(
    any(feature = "native-tls", feature = "rustls"),
    any(feature = "async-std", feature = "tokio")
))]
/// `if_preferred_runtime!{[ Y ] [ N ]}` expands to `Y` (if there's `PreferredRuntime`) or `N`
macro_rules! if_preferred_runtime {{ [$($y:tt)*] [$($n:tt)*] } => { $($y)* }}

if_preferred_runtime! {[
    use crate::PreferredRuntime;
] [
    /// Dummy value that makes the variant uninhabited
    #[derive(Clone, Debug)]
    enum PreferredRuntime {}
]}
/// `with_preferred_runtime!( R; EXPR )` expands to `EXPR`, or to `match *R {}`.
macro_rules! with_preferred_runtime {{ $p:ident; $($then:tt)* } => {
    if_preferred_runtime!([ $($then)* ] [ match *$p {} ])
}}

//---------- principal types ----------

/// Convenience alias for a boxed sleep future
type DynSleepFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// Object-safe version of `SleepProvider` and `CoarseTimeProvider`
///
/// The methods mirror those in `SleepProvider` and `CoarseTimeProvider`
#[allow(clippy::missing_docs_in_private_items)]
trait DynProvider: DynClone + Send + Sync + 'static {
    // SleepProvider principal methods
    fn dyn_now(&self) -> Instant;
    fn dyn_wallclock(&self) -> SystemTime;
    fn dyn_sleep(&self, duration: Duration) -> DynSleepFuture;

    // SleepProvider testing stuff
    fn dyn_block_advance(&self, reason: String);
    fn dyn_release_advance(&self, _reason: String);
    fn dyn_allow_one_advance(&self, duration: Duration);

    // CoarseTimeProvider
    fn dyn_now_coarse(&self) -> CoarseInstant;
}

dyn_clone::clone_trait_object!(DynProvider);

/// Type-erased `SleepProvider` and `CoarseTimeProvider`
///
/// Useful where time is needed, but we don't want a runtime type parameter.
#[derive(Clone, Debug)]
pub struct DynTimeProvider(Impl);

/// Actual contents of a `DynTimeProvider`
///
/// We optimise the `PreferredRuntime` case.
/// We *could*, instead, just use `Box<dyn DynProvider>` here.
///
/// The reason for doing it this way is that we expect this to be on many hot paths.
/// Putting a message in a queue is extremely common, and we'd like to save a dyn dispatch,
/// and reference to a further heap entry (which might be distant in the cache).
///
/// (Also, it's nice to avoid boxing when we crate new types that use this,
/// including our memory-quota-tracked mpsc streams, see `tor-memquota::mq_queue`.
///
/// The downside is that this means:
///  * This enum instead of a simple type
///  * The `unsafe` inside `downcast_value`.
///  * `match` statements in method shims
#[derive(Clone, Educe)]
#[educe(Debug)]
enum Impl {
    /// Just (a handle to) the preferred runtime
    Preferred(PreferredRuntime),
    /// Some other runtime
    Dyn(#[educe(Debug(ignore))] Box<dyn DynProvider>),
}

impl DynTimeProvider {
    /// Create a new `DynTimeProvider` from a concrete runtime type
    pub fn new<R: SleepProvider + CoarseTimeProvider>(runtime: R) -> Self {
        // Try casting to a `DynTimeProvider` directly first, to avoid possibly creating a
        // `DynTimeProvider` containing another `DynTimeProvider`.
        let runtime = match downcast_value(runtime) {
            Ok(x) => return x,
            Err(x) => x,
        };
        // Try casting to a `PreferredRuntime`.
        let imp = match downcast_value(runtime) {
            Ok(preferred) => Impl::Preferred(preferred),
            Err(other) => Impl::Dyn(Box::new(other) as _),
        };
        DynTimeProvider(imp)
    }
}

//---------- impl DynProvider for any SleepProvider + CoarseTimeProvider ----------

/// Define ordinary methods in `impl DynProvider`
///
/// This macro exists mostly to avoid copypaste mistakes where we (for example)
/// implement `block_advance` by calling `release_advance`.
macro_rules! dyn_impl_methods { { $(
    fn $name:ident(
        ,
        $( $param:ident: $ptype:ty ),*
    ) -> $ret:ty;
)* } => { paste! { $(
    fn [<dyn_ $name>](
        &self,
        $( $param: $ptype, )*
    )-> $ret {
        self.$name( $($param,)* )
    }
)* } } }

impl<R: SleepProvider + CoarseTimeProvider> DynProvider for R {
    dyn_impl_methods! {
        fn now(,) -> Instant;
        fn wallclock(,) -> SystemTime;

        fn block_advance(, reason: String) -> ();
        fn release_advance(, reason: String) -> ();
        fn allow_one_advance(, duration: Duration) -> ();

        fn now_coarse(,) -> CoarseInstant;
    }

    fn dyn_sleep(&self, duration: Duration) -> DynSleepFuture {
        Box::pin(self.sleep(duration))
    }
}

//---------- impl SleepProvider and CoarseTimeProvider for DynTimeProvider ----------

/// Define ordinary methods in `impl .. for DynTimeProvider`
///
/// This macro exists mostly to avoid copypaste mistakes where we (for example)
/// implement `block_advance` by calling `release_advance`.
macro_rules! pub_impl_methods { { $(
    fn $name:ident $( [ $($generics:tt)* ] )? (
        ,
        $( $param:ident: $ptype:ty ),*
    ) -> $ret:ty;
)* } => { paste! { $(
    fn $name $( < $($generics)* > )?(
        &self,
        $( $param: $ptype, )*
    )-> $ret {
        match &self.0 {
            Impl::Preferred(p) => with_preferred_runtime!(p; p.$name( $($param,)* )),
            Impl::Dyn(p) => p.[<dyn_ $name>]( $($param .into() ,)? ),
        }
    }
)* } } }

impl SleepProvider for DynTimeProvider {
    pub_impl_methods! {
        fn now(,) -> Instant;
        fn wallclock(,) -> SystemTime;

        fn block_advance[R: Into<String>](, reason: R) -> ();
        fn release_advance[R: Into<String>](, reason: R) -> ();
        fn allow_one_advance(, duration: Duration) -> ();
    }

    type SleepFuture = DynSleepFuture;

    fn sleep(&self, duration: Duration) -> DynSleepFuture {
        match &self.0 {
            Impl::Preferred(p) => with_preferred_runtime!(p; Box::pin(p.sleep(duration))),
            Impl::Dyn(p) => p.dyn_sleep(duration),
        }
    }
}

impl CoarseTimeProvider for DynTimeProvider {
    pub_impl_methods! {
        fn now_coarse(,) -> CoarseInstant;
    }
}

//---------- downcast_value ----------

// TODO expose this, maybe in tor-basic-utils ?

/// Try to cast `I` (which is presumably a TAIT) to `O` (presumably a concrete type)
///
/// We use runtime casting, but typically the answer is known at compile time.
///
/// Astonishingly, this isn't in any of the following:
///  * `std`
///  * `match-downcast`
///  * `better_any` (`downcast:move` comes close but doesn't give you your `self` back)
///  * `castaway`
///  * `mopa`
///  * `as_any`
fn downcast_value<I: std::any::Any, O: Sized + 'static>(input: I) -> Result<O, I> {
    // `MaybeUninit` makes it possible to to use `downcast_mut`
    // and, if it's successful, *move* out of the reference.
    //
    // It might be possible to write this function using `mme::transmute` instead.
    // That might be simpler on the surface, but `mem:transmute` is a very big hammer,
    // and doing it that way would make it quite easy to accidentally
    // use the wrong type for the dynamic type check, or mess up lifetimes in I or O.
    // (Also if we try to transmute the *value*, it might not be possible to
    // persuade the compiler that the two layouts were necessarily the same.)
    //
    // The technique we use is:
    //    * Put the input into `MaybeUninit`, giving us manual control of `I`'s ownership.
    //    * Try to downcast `&mut I` (from the `MaybeUninit`) to `&mut O`.
    //    * If the downcast is successful, move out of the `&mut O`;
    //      this invalidates the `MaybeUninit` (making it uninitialised).
    //    * If the downcast is unsuccessful, reocver the original `I`,
    //      which hasn't in fact have invalidated.

    let mut input = MaybeUninit::new(input);
    // SAFETY: the MaybeUninit is initialised just above
    let mut_ref: &mut I = unsafe { input.assume_init_mut() };
    match <dyn std::any::Any>::downcast_mut(mut_ref) {
        Some::<&mut O>(output) => {
            let output = output as *mut O;
            // SAFETY:
            //  output is properly aligned and points to a properly initialised
            //    O, because it came from a mut reference
            //  Reading this *invalidates* the MaybeUninit, since the value isn't Copy.
            //  It also invalidates mut_ref, which we therefore mustn't use again.
            let output: O = unsafe { output.read() };
            // Prove that the MaybeUninit is live up to here, and then isn't used any more
            #[allow(clippy::drop_non_drop)] // Yes, we know
            mem::drop::<MaybeUninit<I>>(input);
            Ok(output)
        }
        None => Err(
            // SAFETY: Indeed, it was just initialised, and downcast_mut didn't change that
            unsafe { input.assume_init() },
        ),
    }
}

#[cfg(test)]
mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::mixed_attributes_style)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::single_char_pattern)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::unchecked_duration_subtraction)]
    #![allow(clippy::useless_vec)]
    #![allow(clippy::needless_pass_by_value)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->
    #![allow(clippy::useless_format)]
    use super::*;

    use std::fmt::{Debug, Display};
    use std::hint::black_box;

    fn try_downcast_string<S: Display + Debug + 'static>(x: S) -> Result<String, S> {
        black_box(downcast_value(black_box(x)))
    }

    #[test]
    fn check_downcast_value() {
        // This and the one in check_downcast_dropcount are not combined, with generics,
        // so that the types of everything are as clear as they can be.
        assert_eq!(try_downcast_string(format!("hi")).unwrap(), format!("hi"));
        assert_eq!(try_downcast_string("hi").unwrap_err().to_string(), "hi");
    }

    #[test]
    fn check_downcast_dropcount() {
        #[derive(Debug, derive_more::Display)]
        #[display("{self:?}")]
        struct DropCounter(u32);

        fn try_downcast_dc(x: impl Debug + 'static) -> Result<DropCounter, impl Debug + 'static> {
            black_box(downcast_value(black_box(x)))
        }

        impl Drop for DropCounter {
            fn drop(&mut self) {
                let _: u32 = self.0.checked_sub(1).unwrap();
            }
        }

        let dc = DropCounter(0);
        let mut dc: DropCounter = try_downcast_dc(dc).unwrap();
        assert_eq!(dc.0, 0);
        dc.0 = 1;

        let dc = DropCounter(0);
        let mut dc: DropCounter = try_downcast_string(dc).unwrap_err();
        assert_eq!(dc.0, 0);
        dc.0 = 1;
    }

    if_preferred_runtime! {[
        #[test]
        fn dyn_time_provider_from_dyn_time_provider() {
            // A new `DynTimeProvider(Impl::PreferredRuntime(_))`.
            let x = DynTimeProvider::new(PreferredRuntime::create().unwrap());

            // Cast `x` as a generic `SleepProvider + CoarseTimeProvider` and wrap in a new
            // `DynTimeProvider`.
            fn new_provider<R: SleepProvider + CoarseTimeProvider>(runtime: R) -> DynTimeProvider {
                DynTimeProvider::new(runtime)
            }
            let x = new_provider(x);

            // Ensure that `x` didn't end up as a `DynTimeProvider(Impl::Dyn(_))`.
            assert!(matches!(x, DynTimeProvider(Impl::Preferred(_))));
        }
    ] [
        // no test if there is no preferred runtime
    ]}
}
