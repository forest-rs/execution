// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Share an explicitly installed observer only when execution gains another control segment.

use super::BorrowedOrOwned;
use crate::host::{AccessSink, ResourceKeyRef};
use alloc::rc::Rc;
use core::{
    cell::{RefCell, RefMut},
    ops::{Deref, DerefMut},
};

pub(super) enum Observer<'a, T: ?Sized> {
    Unique(BorrowedOrOwned<'a, T>),
    Shared(Rc<RefCell<BorrowedOrOwned<'a, T>>>),
}

pub(super) enum ObserverBorrow<'view, 'a, T: ?Sized> {
    Unique(&'view mut BorrowedOrOwned<'a, T>),
    Shared(RefMut<'view, BorrowedOrOwned<'a, T>>),
}

impl<'a, T: ?Sized> Observer<'a, T> {
    pub(super) fn borrow_mut(&mut self) -> ObserverBorrow<'_, 'a, T> {
        match self {
            Self::Unique(value) => ObserverBorrow::Unique(value),
            Self::Shared(value) => ObserverBorrow::Shared(value.borrow_mut()),
        }
    }

    pub(super) fn share(slot: &mut Option<Self>) -> Option<Self> {
        let shared = match slot.take()? {
            Self::Unique(value) => Rc::new(RefCell::new(value)),
            Self::Shared(value) => value,
        };
        *slot = Some(Self::Shared(Rc::clone(&shared)));
        Some(Self::Shared(shared))
    }
}

impl<T: ?Sized> Deref for ObserverBorrow<'_, '_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            Self::Unique(value) => value,
            Self::Shared(value) => value,
        }
    }
}
impl<T: ?Sized> DerefMut for ObserverBorrow<'_, '_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match self {
            Self::Unique(value) => value,
            Self::Shared(value) => value,
        }
    }
}

impl AccessSink for Observer<'_, dyn AccessSink + '_> {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        self.borrow_mut().read(key);
    }
    fn write(&mut self, key: ResourceKeyRef<'_>) {
        self.borrow_mut().write(key);
    }
}
