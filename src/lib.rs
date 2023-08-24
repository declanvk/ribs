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

pub mod unsync;

mod polyfill {
    //! Module containing copies of Rust standard library unstable functions for
    //! use outside of the nightly distribution.

    /// Assuming all the elements are initialized, get a slice to them.
    ///
    /// # Safety
    ///
    /// It is up to the caller to guarantee that the `MaybeUninit<T>` elements
    /// really are in an initialized state.
    /// Calling this when the content is not yet fully initialized causes
    /// undefined behavior.
    ///
    /// See [`assume_init_ref`][std::mem::MaybeUninit::assume_init_ref] for more
    /// details and examples.
    ///
    /// **This is a unstable API copied from the Rust standard library, tracking
    /// issue is [#63569][issue-63569]**
    ///
    /// [issue-63569]: https://github.com/rust-lang/rust/issues/63569
    pub const unsafe fn maybe_uninit_slice_assume_init_ref<T>(
        slice: &[std::mem::MaybeUninit<T>],
    ) -> &[T] {
        #[cfg(feature = "nightly")]
        {
            // SAFETY: Covered by condition of containing function
            unsafe { std::mem::MaybeUninit::slice_assume_init_ref(slice) }
        }

        #[cfg(not(feature = "nightly"))]
        {
            // SAFETY: casting `slice` to a `*const [T]` is safe since the caller guarantees
            // that `slice` is initialized, and `MaybeUninit` is guaranteed to have
            // the same layout as `T`. The pointer obtained is valid since it refers
            // to memory owned by `slice` which is a reference and thus guaranteed
            // to be valid for reads.
            unsafe { &*(slice as *const [std::mem::MaybeUninit<T>] as *const [T]) }
        }
    }

    /// Assuming all the elements are initialized, get a mutable slice to them.
    ///
    /// # Safety
    ///
    /// It is up to the caller to guarantee that the `MaybeUninit<T>` elements
    /// really are in an initialized state.
    /// Calling this when the content is not yet fully initialized causes
    /// undefined behavior.
    ///
    /// See [`assume_init_mut`][std::mem::MaybeUninit::assume_init_mut] for more
    /// details and examples.
    ///
    /// **This is a unstable API copied from the Rust standard library, tracking
    /// issue is [#63569][issue-63569]**
    ///
    /// [issue-63569]: https://github.com/rust-lang/rust/issues/63569
    pub unsafe fn maybe_uninit_slice_assume_init_mut<T>(
        slice: &mut [std::mem::MaybeUninit<T>],
    ) -> &mut [T] {
        #[cfg(feature = "nightly")]
        {
            // SAFETY: Covered by condition of containing function
            unsafe { std::mem::MaybeUninit::slice_assume_init_mut(slice) }
        }

        #[cfg(not(feature = "nightly"))]
        {
            // SAFETY: similar to safety notes for `slice_get_ref`, but we have a
            // mutable reference which is also guaranteed to be valid for writes.
            unsafe { &mut *(slice as *mut [std::mem::MaybeUninit<T>] as *mut [T]) }
        }
    }
}
