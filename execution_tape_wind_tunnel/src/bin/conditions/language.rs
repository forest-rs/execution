// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The private ABI conventions shared by the example frontend and its language runtime.

use execution_tape::program::HostTypeId;

pub(super) const RESTART_TOKEN: HostTypeId = HostTypeId(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    Condition,
    Warning,
    Error,
    ArithmeticError,
    DivisionByZero,
    InvalidRestart,
}
impl Class {
    pub(super) fn code(self) -> i64 {
        self as i64
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Policy {
    CommonLisp,
    Dylan,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RestartMode {
    Function,
    Exit,
}
