// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One owner shape for borrowed entry points and independently stored invocations.

use alloc::boxed::Box;
use core::ops::{Deref, DerefMut};

pub(super) enum BorrowedOrOwned<'a, T: ?Sized> {
    Borrowed(&'a mut T),
    Owned(Box<T>),
}
impl<T: ?Sized> Deref for BorrowedOrOwned<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            Self::Borrowed(value) => value,
            Self::Owned(value) => value,
        }
    }
}
impl<T: ?Sized> DerefMut for BorrowedOrOwned<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match self {
            Self::Borrowed(value) => value,
            Self::Owned(value) => value,
        }
    }
}
