#![deny(
    missing_docs,
    missing_debug_implementations,
    clippy::missing_safety_doc,
    unsafe_op_in_unsafe_fn,
    deprecated_in_future,
    rustdoc::broken_intra_doc_links,
    rustdoc::bare_urls,
    rustdoc::invalid_codeblock_attributes
)]
#![doc(
    html_playground_url = "https://play.rust-lang.org/",
    test(attr(deny(warnings)))
)]

//! Bounded queue implementations
//!
//! # References
//!
//!  - Wang, Jiawei, et al. "{BBQ}: A Block-based Bounded Queue for Exchanging
//!    Data and Profiling." 2022 USENIX Annual Technical Conference (USENIX ATC
//!    22). 2022. [Link to PDF][bbq-pdf]
//!
//! [bbq-pdf]: https://www.usenix.org/system/files/atc22-wang-jiawei.pdf

#[cfg(not(target_has_atomic = "64"))]
compile_error!(
    "The library only works when it has access to atomic instructions for values of 64 bits"
);

#[cfg(all(feature = "shuttle", loom))]
compile_error!("This library only works when one of the features `shuttle` or `loom` is enabled");

mod sync;
pub use sync::{Capacity, Queue, TryPopError, TryPushError};

mod debug_with {
    //! This module contains types that are helpful for debugging values that
    //! need some additional context.
    //!
    //! An example is the [`Queue`][crate::sync::Queue] type, which needs
    //! access to some additional parameters in order to display internal
    //! types.
    //!
    //! This module is inspired by [something similar in the Lark compiler project](https://github.com/lark-exploration/lark/blob/master/components/lark-debug-with/src/lib.rs).

    /// A `Debug` trait that carries a context.
    ///
    /// To use it, do something like
    ///
    /// ```compile_fail
    /// format!("{:?}", value.debug_with(cx))
    /// ```
    pub(crate) trait DebugWith<Cx: ?Sized> {
        /// Store a reference to self and the context together to enable call
        /// [`Debug`] on the [`DebugCxPair`] type.
        fn debug_with<'me>(&'me self, cx: &'me Cx) -> DebugCxPair<'me, &'me Self, Cx> {
            DebugCxPair { value: self, cx }
        }

        // Formats the value using the given formatter and context.
        fn fmt_with(&self, cx: &Cx, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result;
    }

    /// A helper struct that carries a value that implement [`DebugWith`]
    /// alongside a compatible context.
    pub(crate) struct DebugCxPair<'me, Value, Cx: ?Sized>
    where
        Value: DebugWith<Cx>,
    {
        value: Value,
        cx: &'me Cx,
    }

    impl<'me, Value, Cx: ?Sized> std::fmt::Debug for DebugCxPair<'me, Value, Cx>
    where
        Value: DebugWith<Cx>,
    {
        fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.value.fmt_with(self.cx, fmt)
        }
    }

    impl<T, Cx> DebugWith<Cx> for &T
    where
        T: ?Sized + DebugWith<Cx>,
        Cx: ?Sized,
    {
        fn fmt_with(&self, cx: &Cx, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            T::fmt_with(self, cx, fmt)
        }
    }

    impl<T, Cx> DebugWith<Cx> for [T]
    where
        T: DebugWith<Cx>,
    {
        fn fmt_with(&self, cx: &Cx, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_list()
                .entries(self.iter().map(|elem| elem.debug_with(cx)))
                .finish()
        }
    }
}

mod stubs {
    //! Module containing copies of Rust standard library types that have been
    //! modified or extracted in some way.

    #[cfg(loom)]
    pub use loom::{
        cell::UnsafeCell,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[cfg(all(loom, test))]
    pub use loom::{model, thread};

    #[cfg(feature = "shuttle")]
    pub use shuttle::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[cfg(feature = "shuttle")]
    pub use std::cell::UnsafeCell;

    #[cfg(all(feature = "shuttle", test))]
    pub use shuttle::thread;

    #[cfg(all(not(loom), not(feature = "shuttle")))]
    pub use std::{
        cell::UnsafeCell,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[cfg(all(not(loom), not(feature = "shuttle"), test))]
    pub use std::thread;

    /// Stub of `loom::model` that just runs the given closure, with no
    /// additional concurrent permutations
    #[cfg(all(not(loom), not(feature = "shuttle"), test))]
    pub fn model<F>(f: F)
    where
        F: Fn() + Sync + Send + 'static,
    {
        f()
    }

    /// Stub of `loom::model` that just runs the given closure, with no
    /// additional concurrent permutations
    #[cfg(all(feature = "shuttle", test))]
    pub fn model<F>(f: F)
    where
        F: Fn() + Sync + Send + 'static,
    {
        shuttle::check_pct(f, 10_000, 100)
    }
}
