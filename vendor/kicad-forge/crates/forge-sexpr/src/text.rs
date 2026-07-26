//! `Text`: a piece of node text (a token's raw bytes, or the trivia before a
//! node) that is either a zero-copy view into the shared source buffer or an
//! owned heap string for freshly built / mutated nodes.
//!
//! # Why
//!
//! A large board parses into tens of millions of tokens. Storing each token's
//! text as an owned `String` costs one heap allocation per token (~80 ms of
//! `malloc`/`memcpy` and the same again freeing them on a big board). Storing
//! it as `Arc<str>` trades the allocation for an atomic refcount bump per node,
//! which is its own ~80 ms of churn at this scale.
//!
//! `Text` instead borrows: the source is owned once by the
//! [`Document`](crate::Document) as an `Arc<str>`, and a parsed `Text` is a
//! plain pointer+length view into it, no per-node allocation and no refcount.
//! It is laid out as **16 bytes** (one word smaller than `String`), so the CST
//! nodes are smaller than the original owned-`String` design too.
//!
//! # Representation
//!
//! A single struct `{ ptr, len: u32, cap: u32 }`:
//! * **Span** (borrowed): `cap == SPAN`. `ptr/len` view bytes the Document owns.
//! * **Owned**: `cap != SPAN` is a real heap capacity; `ptr/len/cap` are a
//!   `String`'s raw parts and are freed on drop. `cap == 0` (and `ptr` dangling)
//!   is the canonical empty owned string, no allocation.
//!
//! `u32` len/cap caps a single piece of text at 4 GiB, far beyond any token or
//! whole KiCad file.
//!
//! # Soundness
//!
//! A Span borrows bytes it does not own, so the contract is: **a Span must not
//! outlive the buffer it points into.** Upheld structurally:
//! * Only the parser mints Spans, pointing them at the Document's own
//!   `Arc<str>`; Spans thus live *inside* the Document that owns their bytes.
//! * The source buffer is immutable and never moves (heap-stable behind the
//!   `Arc`), so the pointers stay valid for its life.
//! * [`Clone`] of a Span **materialises an owned copy**, so a clone never
//!   carries a borrow that could dangle if it outlives the original tree.
//!
//! Bytes are immutable, owned-elsewhere UTF-8, so `Text` is `Send + Sync`.

use std::ops::Deref;

/// `cap` sentinel marking a borrowed span (no owned allocation to free).
const SPAN: u32 = u32::MAX;

/// Source-backed text: a borrowed span into the Document's source, or an owned
/// heap string. See the module docs for the layout and safety contract.
pub struct Text {
    ptr: *const u8,
    len: u32,
    cap: u32,
}

// SAFETY: an owned `Text` uniquely owns its heap buffer (like `String`); a span
// `Text` borrows an immutable, never-moved, owned-elsewhere UTF-8 buffer. Both
// are safe to send and share across threads.
unsafe impl Send for Text {}
unsafe impl Sync for Text {}

impl Text {
    /// Empty owned text. No allocation.
    #[inline]
    pub const fn empty() -> Text {
        // Dangling but well-aligned; len 0 so the pointer is never read.
        Text { ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(), len: 0, cap: 0 }
    }

    /// A borrowed view of `s`. The caller guarantees `s` lives in a buffer that
    /// outlives every `Text` produced from it (the parser uses the Document's
    /// own `Arc<str>`).
    #[inline]
    pub fn view(s: &str) -> Text {
        debug_assert!(s.len() as u64 <= u32::MAX as u64 - 1);
        Text { ptr: s.as_ptr(), len: s.len() as u32, cap: SPAN }
    }

    #[inline]
    fn from_string(s: String) -> Text {
        if s.capacity() == 0 {
            return Text::empty();
        }
        let mut s = std::mem::ManuallyDrop::new(s);
        debug_assert!(s.capacity() as u64 <= u32::MAX as u64 - 1 && s.len() as u64 <= u32::MAX as u64);
        let t = Text { ptr: s.as_mut_ptr(), len: s.len() as u32, cap: s.capacity() as u32 };
        t
    }

    #[inline]
    fn is_span(&self) -> bool {
        self.cap == SPAN
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: for spans, the buffer outlives `self` and holds valid UTF-8
        // (module docs). For owned, `ptr/len` are a live `String`'s parts.
        unsafe {
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.ptr, self.len as usize))
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len as usize
    }
}

impl Drop for Text {
    #[inline]
    fn drop(&mut self) {
        if !self.is_span() && self.cap != 0 {
            // Reconstitute and drop the owned String.
            // SAFETY: these are the raw parts of a String we took ownership of
            // in `from_string`, never freed elsewhere.
            unsafe {
                drop(String::from_raw_parts(self.ptr as *mut u8, self.len as usize, self.cap as usize));
            }
        }
    }
}

impl Clone for Text {
    /// Cloning always produces an owned copy, so a clone never carries a borrow
    /// that could dangle if it outlives the source tree.
    #[inline]
    fn clone(&self) -> Text {
        Text::from_string(self.as_str().to_string())
    }
}

impl Deref for Text {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Default for Text {
    fn default() -> Text {
        Text::empty()
    }
}

impl From<String> for Text {
    #[inline]
    fn from(s: String) -> Text {
        Text::from_string(s)
    }
}

impl From<&str> for Text {
    #[inline]
    fn from(s: &str) -> Text {
        Text::from_string(s.to_string())
    }
}

impl AsRef<str> for Text {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for Text {
    #[inline]
    fn eq(&self, other: &Text) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for Text {}

impl PartialEq<str> for Text {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for Text {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
