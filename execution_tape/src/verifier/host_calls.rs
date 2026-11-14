// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host call sites of a verified program, with the arguments known before it runs.
//!
//! A forward must-analysis over the same CFG and dataflow solver as the other verifier facts:
//! a register holds a known constant at a call only when every path from the function entry
//! loads that same constant into it (directly, through `mov`, or through block arguments). Joins
//! that disagree, writes by any other instruction, and exceptional arms forget the fact. Function
//! arguments and call results are never known.

use alloc::vec::Vec;
use core::fmt;
use core::ops::Range;

use super::{VerifiedProgram, blocks};
use crate::analysis::cfg::{self, BasicBlock};
use crate::analysis::dataflow;
use crate::analysis::protection::Protection;
use crate::bytecode::{DecodedInstr, Instr, decode_instructions};
use crate::host::ValueRef;
use crate::program::{ConstEntry, ConstId, HostSigId, Program, SpanId};
use crate::value::{Decimal, FuncId};

/// A constant a register is known to hold. Strings and bytes stay in the program's pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Known {
    Unit,
    Bool(bool),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    F32(u32),
    F64(u64),
    Decimal(Decimal),
    Pool(ConstId),
}

impl Known {
    fn of(program: &Program, instr: &Instr) -> Option<Self> {
        Some(match *instr {
            Instr::ConstUnit { .. } => Self::Unit,
            Instr::ConstBool { imm, .. } => Self::Bool(imm),
            Instr::ConstI32 { imm, .. } => Self::I32(imm),
            Instr::ConstU32 { imm, .. } => Self::U32(imm),
            Instr::ConstI64 { imm, .. } => Self::I64(imm),
            Instr::ConstU64 { imm, .. } => Self::U64(imm),
            Instr::ConstF32 { bits, .. } => Self::F32(bits),
            Instr::ConstF64 { bits, .. } => Self::F64(bits),
            Instr::ConstDecimal { value, .. } => Self::Decimal(value),
            // Pool scalars use the immediate forms, so equal values meet as equal.
            Instr::ConstPool { idx, .. } => match program.const_pool.get(idx.0 as usize)? {
                ConstEntry::Unit => Self::Unit,
                ConstEntry::Bool(v) => Self::Bool(*v),
                ConstEntry::I64(v) => Self::I64(*v),
                ConstEntry::U64(v) => Self::U64(*v),
                ConstEntry::F64(bits) => Self::F64(*bits),
                ConstEntry::Decimal(_) => Self::Decimal(program.const_decimal(idx).ok()?),
                ConstEntry::Bytes(_) | ConstEntry::Str(_) => Self::Pool(idx),
            },
            _ => return None,
        })
    }

    fn value<'a>(&'a self, program: &'a Program) -> ValueRef<'a> {
        match self {
            Self::Unit => ValueRef::Unit,
            Self::Bool(v) => ValueRef::Bool(*v),
            Self::I32(v) => ValueRef::I32(*v),
            Self::U32(v) => ValueRef::U32(*v),
            Self::I64(v) => ValueRef::I64(*v),
            Self::U64(v) => ValueRef::U64(*v),
            Self::F32(bits) => ValueRef::F32(f32::from_bits(*bits)),
            Self::F64(bits) => ValueRef::F64(f64::from_bits(*bits)),
            Self::Decimal(v) => ValueRef::Decimal(v),
            Self::Pool(id) => match program.const_pool[id.0 as usize] {
                ConstEntry::Str(_) => ValueRef::Str(program.const_str(*id).expect("verified str")),
                _ => ValueRef::Bytes(program.const_bytes(*id).expect("verified bytes")),
            },
        }
    }
}

/// Known constants by register, sorted. `top` is the identity of the meet: not yet reached.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Constants {
    top: bool,
    known: Vec<(u32, Known)>,
}

impl Constants {
    fn get(&self, reg: u32) -> Option<Known> {
        self.known
            .binary_search_by_key(&reg, |&(r, _)| r)
            .ok()
            .map(|i| self.known[i].1)
    }

    fn set(&mut self, reg: u32, value: Option<Known>) {
        match (self.known.binary_search_by_key(&reg, |&(r, _)| r), value) {
            (Ok(i), Some(v)) => self.known[i].1 = v,
            (Ok(i), None) => {
                self.known.remove(i);
            }
            (Err(i), Some(v)) => self.known.insert(i, (reg, v)),
            (Err(_), None) => {}
        }
    }

    fn forget_writes(&mut self, decoded: &[DecodedInstr]) {
        for di in decoded {
            for reg in di.instr.writes() {
                self.set(reg, None);
            }
        }
    }

    fn transfer(&mut self, program: &Program, instr: &Instr) {
        match instr {
            // Parameters are bound on incoming edges.
            Instr::Block { .. } => {}
            Instr::Mov { dst, src } => self.set(*dst, self.get(*src)),
            _ => {
                let known = Known::of(program, instr);
                for reg in instr.writes() {
                    self.set(reg, known);
                }
            }
        }
    }
}

struct Site {
    function: FuncId,
    pc: u32,
    import: HostSigId,
    span: Option<SpanId>,
    args: Range<usize>,
}

/// Every reachable host call of a [`VerifiedProgram`], with the arguments known before it runs.
///
/// Returned by [`VerifiedProgram::host_call_sites`]. An argument is known only when every path
/// from the function entry to the call loads the same constant into its register, so a link
/// check can trust it: no fact is a guess. Sites are in function order, then bytecode order.
/// Calls in unreachable code are omitted, since they can never run.
///
/// # Example
///
/// A schema check that rejects an unknown setting name before anything runs:
///
/// ```
/// use execution_tape::asm::{FunctionSig, ProgramBuilder};
/// use execution_tape::host::{HostSig, ValueRef};
/// use execution_tape::program::ValueType;
///
/// let mut pb = ProgramBuilder::new();
/// let set = pb.host_sig_for(
///     "settings.set",
///     HostSig { args: vec![ValueType::Str, ValueType::F32], rets: vec![] },
/// );
/// let entry = pb.declare_function(FunctionSig { arg_types: vec![ValueType::F32], ret_types: vec![] })?;
/// let mut body = pb.function_asm(entry)?;
/// let value = pb.argument(entry, 0)?;
/// let [] = pb.emit_call_with(&mut body, set, ("rustiness", value))?;
/// body.ret(0, &[]);
/// pb.define_function(entry, body)?;
/// let program = pb.build_verified()?;
///
/// let schema = ["age", "wetness"];
/// let rejected: Vec<String> = program
///     .host_call_sites()
///     .iter()
///     .filter_map(|site| match site.arg(0) {
///         Some(ValueRef::Str(name)) if !schema.contains(&name) => Some(format!(
///             "{} at pc {}: nothing named {name:?}",
///             site.symbol(),
///             site.pc()
///         )),
///         _ => None,
///     })
///     .collect();
/// assert_eq!(rejected, ["settings.set at pc 3: nothing named \"rustiness\""]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// [`HostFunctions::bind_checked`](crate::host_binding::HostFunctions::bind_checked) runs the
/// same kind of check as part of binding and reports each rejection with its location.
pub struct HostCallSites<'a> {
    program: &'a Program,
    sites: Vec<Site>,
    args: Vec<Option<Known>>,
}

impl fmt::Debug for HostCallSites<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> HostCallSites<'a> {
    /// Returns the number of reachable host calls.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sites.len()
    }

    /// Returns `true` when the program makes no reachable host call.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    /// Iterates call sites in function order, then bytecode order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = HostCallSite<'_>> + '_ {
        self.sites.iter().map(|site| HostCallSite {
            program: self.program,
            site,
            args: &self.args[site.args.clone()],
        })
    }
}

/// One reachable `host_call` instruction and its statically known arguments.
#[derive(Clone, Copy)]
pub struct HostCallSite<'s> {
    program: &'s Program,
    site: &'s Site,
    args: &'s [Option<Known>],
}

impl fmt::Debug for HostCallSite<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostCallSite")
            .field("symbol", &self.symbol())
            .field("function", &self.function())
            .field("pc", &self.pc())
            .field("args", &self.args().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl<'s> HostCallSite<'s> {
    /// The called import.
    #[must_use]
    pub fn import(&self) -> HostSigId {
        self.site.import
    }

    /// The called import's symbol.
    #[must_use]
    pub fn symbol(&self) -> &'s str {
        let import = &self.program.host_sigs[self.site.import.0 as usize];
        self.program
            .symbol_str(import.symbol)
            .expect("verified host symbol")
    }

    /// The function containing the call.
    #[must_use]
    pub fn function(&self) -> FuncId {
        self.site.function
    }

    /// The call instruction's bytecode offset within its function.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.site.pc
    }

    /// The call instruction's source span, if the program records one.
    #[must_use]
    pub fn span(&self) -> Option<SpanId> {
        self.site.span
    }

    /// Returns argument `index` when it is a constant on every path to the call.
    ///
    /// `None` means not known (or no such argument), never a guess. Known values are scalars,
    /// decimals, strings or bytes, borrowed as the host would receive them.
    #[must_use]
    pub fn arg(&self, index: usize) -> Option<ValueRef<'s>> {
        self.args
            .get(index)?
            .as_ref()
            .map(|known| known.value(self.program))
    }

    /// Iterates every argument in order: `Some` where it is a known constant, as for
    /// [`Self::arg`].
    pub fn args(&self) -> impl ExactSizeIterator<Item = Option<ValueRef<'s>>> + 's {
        let program = self.program;
        self.args
            .iter()
            .map(move |known| known.as_ref().map(|known| known.value(program)))
    }
}

impl VerifiedProgram {
    /// Finds every reachable host call and the arguments known on all paths to it.
    ///
    /// This runs a small dataflow analysis per function that calls the host; nothing is cached
    /// and verification itself does not pay for it. See [`HostCallSites`].
    #[must_use]
    pub fn host_call_sites(&self) -> HostCallSites<'_> {
        let program = self.program();
        let mut sites = HostCallSites {
            program,
            sites: Vec::new(),
            args: Vec::new(),
        };
        for (index, definition) in program.functions.iter().enumerate() {
            let function = FuncId(u32::try_from(index).expect("verified function index"));
            let bytes = definition.bytecode(program).expect("verified bytecode");
            let decoded = decode_instructions(bytes).expect("verified instructions");
            if !decoded
                .iter()
                .any(|di| matches!(di.instr, Instr::HostCall { .. }))
            {
                continue;
            }
            let blocks = cfg::build_basic_blocks(
                u32::try_from(bytes.len()).expect("verified code size"),
                &decoded,
                &cfg::compute_boundaries(bytes.len(), &decoded),
                &Protection::new(program, definition, &decoded),
            )
            .expect("verified control flow");
            let reachable = cfg::compute_reachable(&blocks);
            let inputs = solve(program, &blocks, &reachable, &decoded);
            let executable = self.verified(function).expect("verified function");
            for (b, block) in blocks.iter().enumerate() {
                if !reachable[b] {
                    continue;
                }
                let mut state = inputs[b].clone();
                for di in &decoded[block.instr_start..block.instr_end] {
                    if let Instr::HostCall { host_sig, args, .. } = &di.instr {
                        let start = sites.args.len();
                        sites.args.extend(args.iter().map(|&reg| state.get(reg)));
                        let span = executable
                            .instrs
                            .binary_search_by_key(&di.offset, |e| e.offset)
                            .ok()
                            .and_then(|ix| executable.span_at_ix(ix));
                        sites.sites.push(Site {
                            function,
                            pc: di.offset,
                            import: *host_sig,
                            span,
                            args: start..sites.args.len(),
                        });
                    }
                    state.transfer(program, &di.instr);
                }
            }
        }
        sites
    }
}

fn solve(
    program: &Program,
    blocks: &[BasicBlock],
    reachable: &[bool],
    decoded: &[DecodedInstr],
) -> Vec<Constants> {
    let mut edge = Constants {
        top: false,
        known: Vec::new(),
    };
    let (inputs, _) = dataflow::solve_forward(
        blocks,
        reachable,
        Constants {
            top: false,
            known: Vec::new(),
        },
        Constants {
            top: true,
            known: Vec::new(),
        },
        |pred, arm, _, acc, incoming| {
            if incoming.top {
                return false;
            }
            let block = &blocks[pred];
            let normal = block.normal_arms();
            let incoming = if arm >= normal {
                // Raised before `first` completes: earlier instructions have run, and any later
                // one of the block may or may not have. The handler's parameters are payload.
                let raise = &block.raises[arm - normal];
                edge.clone_from(incoming);
                for di in &decoded[block.instr_start..raise.first] {
                    edge.transfer(program, &di.instr);
                }
                edge.forget_writes(&decoded[raise.first..block.instr_end]);
                let target = raise.handler.catch.target();
                for &param in blocks::declaration(decoded, target).unwrap_or(&[]) {
                    edge.set(param, None);
                }
                &edge
            } else if let Some((target, args)) = decoded[block.instr_end - 1]
                .instr
                .edge(arm)
                .filter(|(_, args)| !args.is_empty())
            {
                let params = blocks::declaration(decoded, target).expect("validated target");
                edge.clone_from(incoming);
                for (&param, &arg) in params.iter().zip(args) {
                    edge.set(param, incoming.get(arg));
                }
                &edge
            } else {
                incoming
            };
            if acc.top {
                acc.clone_from(incoming);
                return true;
            }
            let before = acc.known.len();
            acc.known.retain(|&(reg, v)| incoming.get(reg) == Some(v));
            acc.known.len() != before
        },
        |_, block, state| {
            for di in &decoded[block.instr_start..block.instr_end] {
                state.transfer(program, &di.instr);
            }
        },
    );
    inputs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{Asm, FunctionSig, ProgramBuilder};
    use crate::host::HostSig;
    use crate::program::{Const, ValueType};
    use alloc::vec;

    /// Builds `f(flag: bool, n: i64)` whose body `emit` writes, importing `h(str, i64)`.
    fn program(emit: impl FnOnce(&mut ProgramBuilder, &mut Asm, HostSigId)) -> VerifiedProgram {
        let mut pb = ProgramBuilder::new();
        let h = pb.host_sig_for(
            "h",
            HostSig {
                args: vec![ValueType::Str, ValueType::I64],
                rets: vec![],
            },
        );
        let f = pb
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Bool, ValueType::I64],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = pb.function_asm(f).unwrap();
        emit(&mut pb, &mut a, h);
        a.ret(0, &[]);
        pb.define_function(f, a).unwrap();
        pb.build_verified().unwrap()
    }

    fn args(program: &VerifiedProgram) -> Vec<Vec<Option<ValueRef<'_>>>> {
        // Leaks one small analysis per test so facts can borrow it for the assertion.
        let sites = alloc::boxed::Box::leak(alloc::boxed::Box::new(program.host_call_sites()));
        sites.iter().map(|site| site.args().collect()).collect()
    }

    fn text(pb: &mut ProgramBuilder, a: &mut Asm, s: &str) -> u32 {
        let r = a.alloc_register(ValueType::Str);
        a.const_pool(r, pb.const_str(s));
        r
    }

    #[test]
    fn straight_line_constants_are_known_and_arguments_are_not() {
        let p = program(|pb, a, h| {
            let name = text(pb, a, "age");
            let n = a.alloc_register(ValueType::I64);
            a.const_i64(n, 7);
            let copy = a.alloc_register(ValueType::Str);
            a.mov(copy, name);
            let [] = pb.emit_call(a, h, &[copy, n]).unwrap();
            let [] = pb.emit_call(a, h, &[name, 2]).unwrap();
        });
        assert_eq!(
            args(&p),
            [
                vec![Some(ValueRef::Str("age")), Some(ValueRef::I64(7))],
                vec![Some(ValueRef::Str("age")), None],
            ]
        );
        let sites = p.host_call_sites();
        let site = sites.iter().next().unwrap();
        assert_eq!(
            (site.symbol(), site.function(), site.arg(2)),
            ("h", FuncId(0), None)
        );
        assert_eq!(site.import(), HostSigId(0));
    }

    #[test]
    fn joins_keep_only_constants_every_path_agrees_on() {
        let p = program(|pb, a, h| {
            let (name, other, n) = (
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::I64),
            );
            a.const_pool(name, pb.const_str("same"));
            a.const_pool(other, pb.const_str("before"));
            let (yes, no, join) = (a.label(), a.label(), a.label());
            a.br(1, yes, no);
            a.place(yes).unwrap();
            a.const_pool(name, pb.const_str("same"));
            a.const_i64(n, 5);
            a.jmp(join);
            a.place(no).unwrap();
            a.const_pool(other, pb.const_str("changed"));
            // Pool scalars and immediates of equal value meet as equal.
            a.const_pool(n, pb.constant(Const::I64(5)));
            a.jmp(join);
            a.place(join).unwrap();
            let [] = pb.emit_call(a, h, &[name, n]).unwrap();
            let [] = pb.emit_call(a, h, &[other, n]).unwrap();
        });
        assert_eq!(
            args(&p),
            [
                vec![Some(ValueRef::Str("same")), Some(ValueRef::I64(5))],
                vec![None, Some(ValueRef::I64(5))],
            ]
        );
    }

    #[test]
    fn block_arguments_carry_constants_only_when_all_edges_agree() {
        let p = program(|pb, a, h| {
            let (x, y, name, n) = (
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::I64),
            );
            a.const_pool(x, pb.const_str("x"));
            a.const_pool(y, pb.const_str("y"));
            let (yes, no) = (a.label(), a.label());
            let join = a.block(&[name, n]);
            a.br(1, yes, no);
            a.place(yes).unwrap();
            a.const_i64(n, 1);
            a.jmp(join.with_args(&[x, n]));
            a.place(no).unwrap();
            a.jmp(join.with_args(&[y, 2]));
            a.place(join).unwrap();
            let [] = pb.emit_call(a, h, &[name, n]).unwrap();
        });
        assert_eq!(args(&p), [vec![None, None]]);

        let p = program(|pb, a, h| {
            let (x, name) = (
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::Str),
            );
            a.const_pool(x, pb.const_str("x"));
            let (yes, no) = (a.label(), a.label());
            let join = a.block(&[name]);
            a.br(1, yes, no);
            a.place(yes).unwrap();
            a.jmp(join.with_args(&[x]));
            a.place(no).unwrap();
            a.jmp(join.with_args(&[x]));
            a.place(join).unwrap();
            let [] = pb.emit_call(a, h, &[name, 2]).unwrap();
        });
        assert_eq!(args(&p), [vec![Some(ValueRef::Str("x")), None]]);
    }

    #[test]
    fn loops_and_other_writes_forget_constants() {
        let p = program(|pb, a, h| {
            let (name, n, one) = (
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::I64),
                a.alloc_register(ValueType::I64),
            );
            a.const_pool(name, pb.const_str("loop"));
            a.const_i64(n, 0).const_i64(one, 1);
            let (head, exit) = (a.label(), a.label());
            a.jmp(head);
            a.place(head).unwrap();
            // `n` is 0 on entry and `n + 1` around the back edge.
            let [] = pb.emit_call(a, h, &[name, n]).unwrap();
            a.i64_add(n, n, one);
            a.br(1, head, exit);
            a.place(exit).unwrap();
            let [] = pb.emit_call(a, h, &[name, one]).unwrap();
        });
        assert_eq!(
            args(&p),
            [
                vec![Some(ValueRef::Str("loop")), None],
                vec![Some(ValueRef::Str("loop")), Some(ValueRef::I64(1))],
            ]
        );
    }

    #[test]
    fn handlers_keep_only_constants_no_raising_point_can_have_changed() {
        let p = program(|pb, a, h| {
            let (name, kept, n) = (
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::Str),
                a.alloc_register(ValueType::I64),
            );
            a.const_pool(name, pb.const_str("before"));
            a.const_pool(kept, pb.const_str("kept"));
            a.const_i64(n, 1);
            let (handler, done) = (a.block(&[]), a.label());
            a.begin_region(&[crate::asm::Catch::All(handler)]);
            let [] = pb.emit_call(a, h, &[name, n]).unwrap();
            a.const_pool(name, pb.const_str("after"));
            let [] = pb.emit_call(a, h, &[name, n]).unwrap();
            a.end_region();
            a.jmp(done);
            a.place(handler).unwrap();
            let [] = pb.emit_call(a, h, &[name, n]).unwrap();
            let [] = pb.emit_call(a, h, &[kept, n]).unwrap();
            a.jmp(done);
            a.place(done).unwrap();
        });
        let s = |v| Some(ValueRef::Str(v));
        let one = Some(ValueRef::I64(1));
        assert_eq!(
            args(&p),
            [
                vec![s("before"), one],
                vec![s("after"), one],
                vec![None, one],
                vec![s("kept"), one],
            ]
        );
    }

    #[test]
    fn unreachable_calls_are_omitted() {
        let p = program(|pb, a, h| {
            let (live, dead) = (a.label(), a.label());
            a.jmp(live);
            a.place(dead).unwrap();
            let name = text(pb, a, "dead");
            let [] = pb.emit_call(a, h, &[name, 2]).unwrap();
            a.jmp(live);
            a.place(live).unwrap();
        });
        assert!(p.host_call_sites().is_empty());
        assert_eq!(p.host_call_sites().len(), 0);
    }
}
