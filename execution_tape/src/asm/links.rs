// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Link declarations: call targets the embedding can repoint after installation.

use super::*;

/// A link could not be declared. The builder is unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LinkBuildError {
    /// Another link in this builder already uses the name.
    DuplicateName {
        /// The repeated name.
        name: String,
        /// The link that already uses it.
        existing: LinkId,
    },
    /// The initial target or the fallback has no declaration in this builder.
    UnknownFunction(FuncId),
    /// A guarded link compares no keys, or more than [`MAX_GUARD_KEYS`].
    GuardKeys(usize),
}

impl fmt::Display for LinkBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateName { name, existing } => write!(
                f,
                "link name \"{name}\" is already declared by link#{}",
                existing.0
            ),
            Self::UnknownFunction(func) => write!(f, "link target f{} has no declaration", func.0),
            Self::GuardKeys(keys) => write!(
                f,
                "a guarded link compares 1 to {MAX_GUARD_KEYS} keys, not {keys}"
            ),
        }
    }
}

impl core::error::Error for LinkBuildError {}

impl ProgramBuilder {
    /// Declares a link: a call target each installed unit owns and the embedding can repoint.
    ///
    /// The link's signature is `target`'s declared signature, interned as a call signature, and
    /// every unit installed from the program starts with the link naming its own `target`.
    /// `call.link` sites (see [`Self::emit_call`] with [`CallTarget::Link`], or
    /// [`Asm::call_link`]) call whatever the calling unit's link names when they execute;
    /// [`Vm::relink`] repoints it at any installed function with exactly this signature.
    ///
    /// Names must be unique within the program; they identify the link in verifier diagnostics,
    /// disassembly and tooling, and an embedding can use them to find the link to repoint.
    ///
    /// ```
    /// use execution_tape::{asm::{FunctionSig, ProgramBuilder, Reg}, program::ValueType};
    /// let mut program = ProgramBuilder::new();
    /// let signature = FunctionSig { arg_types: vec![ValueType::I64], ret_types: vec![ValueType::I64] };
    /// let area = program.declare_function(&signature)?;
    /// let mut body = program.function_asm(area)?;
    /// let side = program.argument(area, 0)?;
    /// body.ret(Reg::EFFECT, &[side]);
    /// program.define_function(area, body)?;
    /// let link = program.declare_link("area(<square>)", area)?;
    /// let caller = program.declare_function(signature)?;
    /// let mut body = program.function_asm(caller)?;
    /// let side = program.argument(caller, 0)?;
    /// let [result] = program.emit_call(&mut body, link, &[side])?;
    /// body.ret(Reg::EFFECT, &[result]);
    /// program.define_function(caller, body)?;
    /// program.build_verified()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn declare_link(&mut self, name: &str, target: FuncId) -> Result<LinkId, LinkBuildError> {
        self.push_link(name, target, None)
    }

    /// Declares a guarded link: an inline cache for a dynamic send.
    ///
    /// Callers pass `key_count` `u64` keys with each call ([`Self::emit_guarded_call`] or
    /// [`Asm::call_link_guard`]), such as the classes of the arguments a generic function
    /// dispatches on. Each installed unit keeps a guard for the link, which starts unfilled: a
    /// call reaches the link's target only when its keys match the guard, and `fallback`
    /// otherwise. The embedding fills the guard with the keys a miss reported and the method
    /// they select, with [`Vm::relink_guarded`] or a waiting request's `relink_guarded`.
    ///
    /// The link's signature is `target`'s, as for [`Self::declare_link`]. `fallback` takes a
    /// `u64` link index, then `key_count` `u64` keys, then the link's arguments, and returns the
    /// link's results; the verifier checks it. Only `call.link.guard` calls a guarded link.
    pub fn declare_guarded_link(
        &mut self,
        name: &str,
        target: FuncId,
        key_count: usize,
        fallback: FuncId,
    ) -> Result<LinkId, LinkBuildError> {
        if !(1..=MAX_GUARD_KEYS).contains(&key_count) {
            return Err(LinkBuildError::GuardKeys(key_count));
        }
        if self.functions.get(fallback.0 as usize).is_none() {
            return Err(LinkBuildError::UnknownFunction(fallback));
        }
        let guard = LinkGuardEntry {
            key_count: u32::try_from(key_count).expect("at most MAX_GUARD_KEYS keys"),
            fallback,
        };
        self.push_link(name, target, Some(guard))
    }

    fn push_link(
        &mut self,
        name: &str,
        target: FuncId,
        guard: Option<LinkGuardEntry>,
    ) -> Result<LinkId, LinkBuildError> {
        let function = self
            .functions
            .get(target.0 as usize)
            .ok_or(LinkBuildError::UnknownFunction(target))?;
        if let Some(index) = self
            .links
            .iter()
            .position(|link| self.symbols[link.name.index() as usize - 1].symbol == name)
        {
            return Err(LinkBuildError::DuplicateName {
                name: name.into(),
                existing: LinkId(u32::try_from(index).unwrap_or(u32::MAX)),
            });
        }
        let abi = FunctionAbi {
            args: function.args.iter().map(|&id| id.into()).collect(),
            rets: function.rets.iter().map(|&id| id.into()).collect(),
        };
        let call_sig = self
            .call_sig(abi)
            .expect("a declared function's contracts belong to this builder");
        let name = self.symbol(name);
        let id = LinkId(u32::try_from(self.links.len()).unwrap_or(u32::MAX));
        self.links.push(LinkEntry {
            name,
            call_sig,
            target,
            guard,
        });
        Ok(id)
    }
}
