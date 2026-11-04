// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Logical scope descriptions outlive the physical guards of a parked stack.

use alloc::vec::Vec;
use execution_tape::vm::ActivationId;
use footfall::{Args, ContextHandle, ScopeDesc, ScopeGuard};

pub(super) struct Scope {
    desc: &'static ScopeDesc,
    context: ContextHandle,
    args: Args,
    guard: Option<ScopeGuard>,
}
impl Scope {
    pub(super) fn new(desc: &'static ScopeDesc, context: ContextHandle, args: Args) -> Self {
        Self {
            desc,
            context,
            args,
            guard: None,
        }
    }
    pub(super) fn token(&self) -> Option<u64> {
        self.guard
            .as_ref()
            .map(ScopeGuard::token)
            .filter(|token| token.is_valid())
            .map(|token| token.get())
    }
    fn park(&mut self) {
        self.guard.take();
    }
    fn attach(&mut self) {
        self.guard = Some(footfall::scope_with(self.desc, self.context, &self.args));
    }
}

struct Stack {
    id: Option<ActivationId>,
    attached: bool,
    run: Option<Scope>,
    scopes: Vec<Scope>,
}
impl Stack {
    fn park(&mut self) {
        for scope in self.scopes.iter_mut().rev() {
            scope.park();
        }
        if let Some(run) = &mut self.run {
            run.park();
        }
        self.attached = false;
    }
    fn attach(&mut self) {
        if let Some(run) = &mut self.run {
            run.attach();
        }
        for scope in &mut self.scopes {
            scope.attach();
        }
        self.attached = true;
    }
}

pub(super) struct Scopes {
    stacks: Vec<Option<Stack>>,
    active: Vec<usize>,
    consistent: bool,
}
impl Scopes {
    pub(super) const fn new() -> Self {
        Self {
            stacks: Vec::new(),
            active: Vec::new(),
            consistent: true,
        }
    }
    pub(super) fn consistent(&self) -> bool {
        self.consistent
    }
    fn find(&self, id: Option<&ActivationId>) -> Option<usize> {
        // Ordinary callbacks address the active leaf. Search parked stacks only on a transfer.
        self.active
            .last()
            .copied()
            .filter(|&index| {
                self.stacks[index]
                    .as_ref()
                    .is_some_and(|stack| stack.id.as_ref() == id)
            })
            .or_else(|| {
                self.stacks
                    .iter()
                    .rposition(|entry| entry.as_ref().is_some_and(|stack| stack.id.as_ref() == id))
            })
    }
    fn ensure(&mut self, id: Option<&ActivationId>, new_run: bool) -> Option<usize> {
        if !self.consistent {
            return None;
        }
        if !(new_run && id.is_none())
            && let Some(index) = self.find(id)
        {
            if self.active.last() != Some(&index) {
                self.fail();
                return None;
            }
            return Some(index);
        }
        let stack = Stack {
            id: id.cloned(),
            attached: true,
            run: None,
            scopes: Vec::new(),
        };
        let index = self
            .stacks
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.stacks.len());
        if index == self.stacks.len() {
            self.stacks.push(Some(stack));
        } else {
            self.stacks[index] = Some(stack);
        }
        self.active.push(index);
        Some(index)
    }
    pub(super) fn start_run(&mut self, id: Option<&ActivationId>, mut scope: Scope) {
        if let Some(index) = self.ensure(id, true) {
            if self.stacks[index]
                .as_ref()
                .expect("registered")
                .run
                .is_some()
            {
                self.fail();
                return;
            }
            scope.attach();
            self.stacks[index].as_mut().expect("registered").run = Some(scope);
        }
    }
    pub(super) fn enter(&mut self, id: Option<&ActivationId>, mut scope: Scope) {
        if let Some(index) = self.ensure(id, false) {
            scope.attach();
            self.stacks[index]
                .as_mut()
                .expect("registered")
                .scopes
                .push(scope);
        }
    }
    pub(super) fn run_token(&self, id: Option<&ActivationId>) -> Option<u64> {
        self.stacks[self.find(id)?].as_ref()?.run.as_ref()?.token()
    }
    pub(super) fn scope_token(&self, id: Option<&ActivationId>) -> Option<u64> {
        self.stacks[self.find(id)?].as_ref()?.scopes.last()?.token()
    }
    pub(super) fn exit(&mut self, id: Option<&ActivationId>) {
        let Some(index) = self.find(id) else {
            self.fail();
            return;
        };
        if self.stacks[index].as_ref().expect("registered").attached
            && self.active.last() != Some(&index)
        {
            self.fail();
            return;
        }
        let stack = self.stacks[index].as_mut().expect("registered");
        if stack.scopes.pop().is_none() {
            self.fail();
            return;
        }
        if stack.scopes.is_empty() && stack.run.is_none() {
            self.end_index(index);
        }
    }
    pub(super) fn end_run(&mut self, id: Option<&ActivationId>) {
        let Some(index) = self.find(id) else {
            self.fail();
            return;
        };
        if self.stacks[index]
            .as_ref()
            .expect("registered")
            .run
            .is_none()
        {
            self.fail();
            return;
        }
        self.end_index(index);
    }
    fn end_index(&mut self, index: usize) {
        if self.stacks[index].as_ref().expect("registered").attached {
            if self.active.last() != Some(&index) {
                self.fail();
                return;
            }
            self.active.pop();
        }
        if let Some(mut stack) = self.stacks[index].take() {
            stack.park();
        }
    }
    pub(super) fn park(&mut self, id: Option<&ActivationId>) {
        let Some(index) = self.find(id) else {
            return;
        }; // This mask may have no scopes here.
        if self.active.last() != Some(&index) {
            self.fail();
            return;
        }
        self.active.pop();
        self.stacks[index].as_mut().expect("registered").park();
    }
    pub(super) fn attach(&mut self, id: Option<&ActivationId>) {
        let Some(index) = self.find(id) else {
            return;
        };
        if self.stacks[index].as_ref().expect("registered").attached {
            self.fail();
            return;
        }
        self.stacks[index].as_mut().expect("registered").attach();
        self.active.push(index);
    }
    fn fail(&mut self) {
        self.clear();
        self.consistent = false;
    }
    pub(super) fn clear(&mut self) {
        while let Some(index) = self.active.pop() {
            if let Some(mut stack) = self.stacks[index].take() {
                stack.park();
            }
        }
        self.stacks.clear();
    }
    pub(super) fn depth(&self) -> usize {
        self.stacks
            .iter()
            .flatten()
            .map(|stack| stack.scopes.len())
            .sum()
    }
    #[cfg(test)]
    pub(super) fn runs(&self) -> usize {
        self.stacks
            .iter()
            .flatten()
            .filter(|stack| stack.run.is_some())
            .count()
    }
}
impl Drop for Scopes {
    fn drop(&mut self) {
        self.clear();
    }
}
