//! Thread-safe reference-counted null-terminated strings.
//!
//! This crate provides a space efficient mechanism for storing immutable strings.
//! The best illustration of this is to go over the alternatives:
//!
//! ```rust
//! // &str:
//! //  - content must be known at compile time
//! //  + can be shared between threads
//! //  + space overhead is 2*usize (fat pointer to the string)
//! let s = "foobar";
//! // String
//! //  + can be created at runtime
//! //  - cannot be shared between threads (except with Clone)
//! //  - space overhead of 3*usize (Vec capacity and len + pointer to bytes)
//! //  - accessing string requires two pointer derefs
//! let s = format!("foobar");
//! // CString:
//! //  * mostly same as String
//! //  * space overhead is 2*usize (uses Box<[u8]> internally)
//! use std::ffi::CString;
//! let s = CString::new("foobar").unwrap();
//! // CStr:
//! //  + space overhead is just the pointer (1*usize)
//! //  - hard to construct
//! //  - generally cannot be shared between threds (lifetime usually not 'static)
//! use std::ffi::CStr;
//! let s: &CStr = &*s;
//! // Arc<String>:
//! //  + can be created at runtime
//! //  + can be shared between threads
//! //  - space overhead is 7*usize:
//! //     - pointer to Arc
//! //     - weak count
//! //     - strong count
//! //     - pointer to String
//! //     - String overhead (3*usize)
//! use std::sync::Arc;
//! let s = ArcCStr::from(format!("foobar"));
//! // ArcCStr:
//! //  + can be created at runtime
//! //  + can be shared between threads
//! //  - space overhead is 2*usize (pointer + strong count)
//! use rcstring::ArcCStr;
//! let s = ArcCStr::from("foobar");
//! ```
//!
//! See the [`ArcCStr`][arc] documentation for more details.
//!
//! [arc]: struct.ArcCStr.html

#![feature(shared, core_intrinsics, alloc, heap_api, unique)]
extern crate alloc;

use std::sync::atomic;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release, SeqCst};
use std::borrow;
use std::fmt;
use std::cmp::Ordering;
use std::mem::{size_of, align_of};
use std::intrinsics::abort;
use std::mem;
use std::ops::Deref;
use std::ptr::{self, Shared};
use std::hash::{Hash, Hasher};
use std::{isize, usize};
use std::convert::From;
use alloc::heap;

// Note that much of this code is taken directly from

/// A soft limit on the amount of references that may be made to an `ArcCStr`.
///
/// Going above this limit will abort your program (although not
/// necessarily) at _exactly_ `MAX_REFCOUNT + 1` references.
const MAX_REFCOUNT: usize = (isize::MAX) as usize;

/// A thread-safe reference-counted null-terminated string.
///
/// The type `ArcCStr` provides shared ownership of a C-style null-terminated string allocated in
/// the heap. Invoking [`clone`] on `ArcCStr` produces a new pointer to the same value in the heap.
/// When the last `ArcCStr` pointer to a given string is destroyed, the pointed-to string is also
/// destroyed. Behind the scenes, `ArcCStr` works much like [`Arc`].
///
/// Strings pointed to using `ArcCStr` are meant to be immutable, and there therefore *no*
/// mechanism is provided to get a mutable reference to the underlying string, even if there are no
/// other pointers to the string in question.
///
/// `ArcCStr` uses atomic operations for reference counting, so `ArcCStr`s can be sent freely
/// between threads. In other words, `ArcCStr` implements cheap [`Send`] for strings using the fact
/// that [`CStr`] is [`Sync`]. `ArcCStr` tries to minimize the space overhead of this feature by
/// sharing the string data. The disadvantage of this approach is that it requires atomic
/// operations that are more expensive than ordinary memory accesses. Thus, if you have many
/// threads accessing the same data, you may see contention. However, in the common case, using
/// `ArcCStr` should still be faster than cloning the full string.
///
/// `ArcCStr` automatically dereferences to `CStr` (via the [`Deref`] trait), so you can call
/// `CStr`'s methods on a value of type `ArcCStr`. To avoid name clashes with `CStr`'s methods, the
/// methods of `ArcCStr` itself are [associated functions][assoc], called using function-like
/// syntax:
///
/// ```
/// use rcstring::ArcCStr;
/// let mut my_arc = ArcCStr::from("foobar");
/// ArcCStr::strong_count(&my_arc);
/// ```
///
/// [`clone`]: https://doc.rust-lang.org/std/clone/trait.Clone.html#tymethod.clone
/// [`Arc`]: https://doc.rust-lang.org/std/sync/struct.Arc.html
/// [`Send`]: https://doc.rust-lang.org/std/marker/trait.Send.html
/// [`Sync`]: https://doc.rust-lang.org/std/marker/trait.Sync.html
/// [`Deref`]: https://doc.rust-lang.org/std/ops/trait.Deref.html
/// [`CStr`]: https://doc.rust-lang.org/std/ffi/struct.CStr.html
/// [assoc]: https://doc.rust-lang.org/book/method-syntax.html#associated-functions
///
/// # Examples
///
/// Sharing some immutable strings between threads:
///
// Note that we **do not** run these tests here. The windows builders get super
// unhappy if a thread outlives the main thread and then exits at the same time
// (something deadlocks) so we just avoid this entirely by not running these
// tests.
/// ```no_run
/// use rcstring::ArcCStr;
/// use std::thread;
///
/// let five = ArcCStr::from("5");
///
/// for _ in 0..10 {
///     let five = five.clone();
///
///     thread::spawn(move || {
///         println!("{:?}", five);
///     });
/// }
/// ```
pub struct ArcCStr {
    ptr: Shared<u8>,
}

unsafe impl Send for ArcCStr {}
unsafe impl Sync for ArcCStr {}

impl<'a> From<&'a [u8]> for ArcCStr {
    fn from(b: &'a [u8]) -> Self {
        let blen = b.len() as isize;
        let aus = size_of::<atomic::AtomicUsize>() as isize;
        let mut s = unsafe {
            ptr::Unique::new(heap::allocate((aus + blen + 1) as usize,
                                            align_of::<atomic::AtomicUsize>()))
        };
        // Initialize the AtomicUsize to 1
        {
            let s: &mut atomic::AtomicUsize = unsafe { mem::transmute(s.get_mut()) };
            s.store(1, SeqCst);
        }
        // Fill in the string
        let mut s = unsafe {
            let buf = s.offset(aus);
            ptr::copy_nonoverlapping(&b[0] as *const _, buf, b.len());
            buf.offset(blen)
        };
        // Add \0
        unsafe {
            *s = 0u8;
            let s = s.offset(-blen).offset(-aus);
            ArcCStr { ptr: Shared::new(s) }
        }
    }
}

impl<'a> From<&'a str> for ArcCStr {
    fn from(s: &'a str) -> Self {
        Self::from(s.as_bytes())
    }
}

impl From<String> for ArcCStr {
    fn from(s: String) -> Self {
        Self::from(&*s)
    }
}

use std::ffi::CString;
impl From<CString> for ArcCStr {
    fn from(s: CString) -> Self {
        Self::from(&*s)
    }
}

use std::ffi::CStr;
impl<'a> From<&'a CStr> for ArcCStr {
    fn from(s: &'a CStr) -> Self {
        Self::from(s.to_bytes())
    }
}

impl ArcCStr {
    /// Gets the number of pointers to this string.
    ///
    /// # Safety
    ///
    /// This method by itself is safe, but using it correctly requires extra care.
    /// Another thread can change the strong count at any time,
    /// including potentially between calling this method and acting on the result.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    /// let _also_five = five.clone();
    ///
    /// // This assertion is deterministic because we haven't shared
    /// // the `ArcCStr` between threads.
    /// assert_eq!(2, ArcCStr::strong_count(&five));
    /// ```
    #[inline]
    pub fn strong_count(this: &Self) -> usize {
        this.atomic().load(SeqCst)
    }

    #[inline]
    fn atomic(&self) -> &atomic::AtomicUsize {
        // We're doing *so* many dodgy things here, so let's go through it step-by-step:
        //
        //  - As long as this arc is alive, we know that the pointer is still valid
        //  - AtomicUsize is (obviously) Sync, and we're just giving out a &
        //  - We know that the first bit of memory pointer to by self.ptr contains an AtomicUsize
        //
        unsafe { mem::transmute(self.ptr.as_ref().unwrap()) }
    }

    // Non-inlined part of `drop`.
    #[inline(never)]
    unsafe fn drop_slow(&mut self) {
        atomic::fence(Acquire);
        let blen = self.to_bytes_with_nul().len();
        heap::deallocate(self.ptr.offset(0),
                         size_of::<atomic::AtomicUsize>() + blen,
                         align_of::<atomic::AtomicUsize>())
    }

    #[inline]
    /// Returns true if the two `ArcCStr`s point to the same value (not
    /// just values that compare as equal).
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    /// let same_five = five.clone();
    /// let other_five = ArcCStr::from("5");
    ///
    /// assert!(ArcCStr::ptr_eq(&five, &same_five));
    /// assert!(!ArcCStr::ptr_eq(&five, &other_five));
    /// ```
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        unsafe { this.ptr.offset(0) == other.ptr.offset(0) }
    }
}

impl Clone for ArcCStr {
    /// Makes a clone of the `ArcCStr` pointer.
    ///
    /// This creates another pointer to the same underlying string, increasing the reference count.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// five.clone();
    /// ```
    #[inline]
    fn clone(&self) -> ArcCStr {
        // Using a relaxed ordering is alright here, as knowledge of the
        // original reference prevents other threads from erroneously deleting
        // the object.
        //
        // As explained in the [Boost documentation][1], Increasing the
        // reference counter can always be done with memory_order_relaxed: New
        // references to an object can only be formed from an existing
        // reference, and passing an existing reference from one thread to
        // another must already provide any required synchronization.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        let old_size = self.atomic().fetch_add(1, Relaxed);

        // However we need to guard against massive refcounts in case someone
        // is `mem::forget`ing Arcs. If we don't do this the count can overflow
        // and users will use-after free. We racily saturate to `isize::MAX` on
        // the assumption that there aren't ~2 billion threads incrementing
        // the reference count at once. This branch will never be taken in
        // any realistic program.
        //
        // We abort because such a program is incredibly degenerate, and we
        // don't care to support it.
        if old_size > MAX_REFCOUNT {
            unsafe {
                abort();
            }
        }

        ArcCStr { ptr: self.ptr }
    }
}

impl Deref for ArcCStr {
    type Target = CStr;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // Even more dodgy pointer stuff:
        //
        //  - As long as this arc is alive, we know that the pointer is still valid
        //  - CStr is Sync (and besides, we're only giving out an immutable pointer)
        //  - We know that the first bit of memory pointer to by self.ptr contains an AtomicUsize,
        //    and *after* that comes the CStr we initially copied in.
        //  - We know that the following bytes are a well-formed CStr (e.g., valid unicode and has
        //    a null terminator , because we used a valid CStr to construct this arc in the first
        //    place.
        //
        let aus = size_of::<atomic::AtomicUsize>() as isize;
        unsafe { CStr::from_ptr(mem::transmute(self.ptr.offset(aus))) }
    }
}

impl Drop for ArcCStr {
    /// Drops the `ArcCStr`.
    ///
    /// This will decrement the reference count. If the reference count reaches zero then we also
    /// deallocate the underlying string.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let foo  = ArcCStr::from("foo");
    /// let foo2 = foo.clone();
    ///
    /// drop(foo);    // "foo" is still in memory
    /// drop(foo2);   // "foo" is deallocated
    /// ```
    #[inline]
    fn drop(&mut self) {
        // Because `fetch_sub` is already atomic, we do not need to synchronize
        // with other threads unless we are going to delete the object.
        if self.atomic().fetch_sub(1, Release) != 1 {
            return;
        }

        // This fence is needed to prevent reordering of use of the data and
        // deletion of the data.  Because it is marked `Release`, the decreasing
        // of the reference count synchronizes with this `Acquire` fence. This
        // means that use of the data happens before decreasing the reference
        // count, which happens before this fence, which happens before the
        // deletion of the data.
        //
        // As explained in the [Boost documentation][1],
        //
        // > It is important to enforce any possible access to the object in one
        // > thread (through an existing reference) to *happen before* deleting
        // > the object in a different thread. This is achieved by a "release"
        // > operation after dropping a reference (any access to the object
        // > through this reference must obviously happened before), and an
        // > "acquire" operation before deleting the object.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        atomic::fence(Acquire);

        unsafe {
            self.drop_slow();
        }
    }
}

impl PartialEq for ArcCStr {
    /// Equality for two `ArcCStr`s.
    ///
    /// Two `ArcCStr`s are equal if their underlying strings are equal.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// assert!(five == ArcCStr::from("5"));
    /// ```
    fn eq(&self, other: &ArcCStr) -> bool {
        *(*self) == *(*other)
    }

    /// Inequality for two `ArcCStr`s.
    ///
    /// Two `ArcCStr`s are unequal if their inner values are unequal.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// assert!(five != ArcCStr::from("6"));
    /// ```
    fn ne(&self, other: &ArcCStr) -> bool {
        *(*self) != *(*other)
    }
}
impl PartialOrd for ArcCStr {
    /// Partial comparison for two `ArcCStr`s.
    ///
    /// The two are compared by calling `partial_cmp()` on their underlying strings.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    /// use std::cmp::Ordering;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// assert_eq!(Some(Ordering::Less), five.partial_cmp(&ArcCStr::from("6")));
    /// ```
    fn partial_cmp(&self, other: &ArcCStr) -> Option<Ordering> {
        (**self).partial_cmp(&**other)
    }

    /// Less-than comparison for two `ArcCStr`s.
    ///
    /// The two are compared by calling `<` on their inner values.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// assert!(five < ArcCStr::from("6"));
    /// ```
    fn lt(&self, other: &ArcCStr) -> bool {
        *(*self) < *(*other)
    }

    /// 'Less than or equal to' comparison for two `ArcCStr`s.
    ///
    /// The two are compared by calling `<=` on their underlying strings.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// assert!(five <= ArcCStr::from("5"));
    /// ```
    fn le(&self, other: &ArcCStr) -> bool {
        *(*self) <= *(*other)
    }

    /// Greater-than comparison for two `ArcCStr`s.
    ///
    /// The two are compared by calling `>` on their underlying strings.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// assert!(five > ArcCStr::from("4"));
    /// ```
    fn gt(&self, other: &ArcCStr) -> bool {
        *(*self) > *(*other)
    }

    /// 'Greater than or equal to' comparison for two `ArcCStr`s.
    ///
    /// The two are compared by calling `>=` on their underlying strings.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// assert!(five >= ArcCStr::from("5"));
    /// ```
    fn ge(&self, other: &ArcCStr) -> bool {
        *(*self) >= *(*other)
    }
}
impl Ord for ArcCStr {
    /// Comparison for two `ArcCStr`s.
    ///
    /// The two are compared by calling `cmp()` on their underlying strings.
    ///
    /// # Examples
    ///
    /// ```
    /// use rcstring::ArcCStr;
    /// use std::cmp::Ordering;
    ///
    /// let five = ArcCStr::from("5");
    ///
    /// assert_eq!(Ordering::Less, five.cmp(&ArcCStr::from("6")));
    /// ```
    fn cmp(&self, other: &ArcCStr) -> Ordering {
        (**self).cmp(&**other)
    }
}
impl Eq for ArcCStr {}

impl fmt::Debug for ArcCStr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl fmt::Pointer for ArcCStr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Pointer::fmt(&*self.ptr, f)
    }
}

impl Hash for ArcCStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state)
    }
}

impl borrow::Borrow<CStr> for ArcCStr {
    fn borrow(&self) -> &CStr {
        &*self
    }
}

impl AsRef<CStr> for ArcCStr {
    fn as_ref(&self) -> &CStr {
        &**self
    }
}

#[cfg(test)]
mod tests {
    use std::clone::Clone;
    use std::sync::mpsc::channel;
    use std::thread;
    use super::ArcCStr;
    use std::convert::From;

    #[test]
    #[cfg_attr(target_os = "emscripten", ignore)]
    fn manually_share_arc() {
        let v = "0123456789";
        let arc_v = ArcCStr::from(v);

        let (tx, rx) = channel();

        let _t = thread::spawn(move || {
            let arc_v: ArcCStr = rx.recv().unwrap();
            assert_eq!((*arc_v).to_bytes()[3], b'3');
        });

        tx.send(arc_v.clone()).unwrap();

        assert_eq!((*arc_v).to_bytes()[2], b'2');
        assert_eq!((*arc_v).to_bytes()[4], b'4');
    }

    #[test]
    fn show_arc() {
        let a = ArcCStr::from("foo");
        assert_eq!(format!("{:?}", a), "\"foo\"");
    }

    #[test]
    fn test_from_string() {
        let foo_arc = ArcCStr::from(format!("foo"));
        assert!("foo" == foo_arc.to_string_lossy());
    }

    #[test]
    fn test_ptr_eq() {
        let five = ArcCStr::from("5");
        let same_five = five.clone();
        let other_five = ArcCStr::from("5");

        assert!(ArcCStr::ptr_eq(&five, &same_five));
        assert!(!ArcCStr::ptr_eq(&five, &other_five));
    }
}
