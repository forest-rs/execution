// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A small arithmetic frontend demonstrating exact source connections without span-ID conventions.
//! Grammar: signed integer literals, parentheses, +, -, *. All arithmetic wraps like the VM.

use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{Program, ValueType},
    trace::{TraceLocation, TraceMask, TraceSink},
    value::Value,
    verifier::{VerifyConfig, verify_program_owned},
    vm::{Limits, Vm},
};
use execution_tape_observatory::{
    ProgramBrowser, RecordingExplorer,
    sources::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
};
use std::{
    error::Error,
    io::{Read, Write},
    ops::Range,
    path::PathBuf,
};

const EXAMPLE: &str = "(7 + 5) * (9 - 7)\n";
const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
#[derive(Debug)]
struct Expr {
    range: Range<usize>,
    kind: ExprKind,
}
#[derive(Debug)]
enum ExprKind {
    Number(i64),
    Binary(u8, Box<Expr>, Box<Expr>),
}
struct Parser<'a> {
    source: &'a str,
    at: usize,
    depth: usize,
    nodes: usize,
}
impl Parser<'_> {
    fn node(&mut self) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > 512 {
            return Err("expression exceeds 512 nodes".into());
        }
        Ok(())
    }
    fn space(&mut self) {
        while self
            .source
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.at += 1;
        }
    }
    fn expression(&mut self, precedence: u8) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > 128 {
            return Err("expression nesting exceeds 128".into());
        }
        self.space();
        let start = self.at;
        let mut lhs = if self.source.as_bytes().get(self.at) == Some(&b'(') {
            self.at += 1;
            let mut value = self.expression(0)?;
            self.space();
            if self.source.as_bytes().get(self.at) != Some(&b')') {
                return Err(format!("expected ')' at byte {}", self.at));
            }
            self.at += 1;
            value.range = start..self.at;
            value
        } else {
            if self.source.as_bytes().get(self.at) == Some(&b'-') {
                self.at += 1;
            }
            let digits = self.at;
            while self
                .source
                .as_bytes()
                .get(self.at)
                .is_some_and(u8::is_ascii_digit)
            {
                self.at += 1;
            }
            if digits == self.at {
                return Err(format!("expected integer or '(' at byte {}", start));
            }
            let value = self.source[start..self.at]
                .parse()
                .map_err(|_| format!("i64 literal out of range at byte {start}"))?;
            self.node()?;
            Expr {
                range: start..self.at,
                kind: ExprKind::Number(value),
            }
        };
        loop {
            self.space();
            let op = self.source.as_bytes().get(self.at).copied();
            let power = match op {
                Some(b'+' | b'-') => 1,
                Some(b'*') => 2,
                _ => 0,
            };
            if power == 0 || power <= precedence {
                break;
            }
            self.at += 1;
            let rhs = self.expression(power)?;
            let range = lhs.range.start..rhs.range.end;
            self.node()?;
            lhs = Expr {
                range,
                kind: ExprKind::Binary(op.expect("operator"), Box::new(lhs), Box::new(rhs)),
            };
        }
        self.depth -= 1;
        Ok(lhs)
    }
}
fn compile(name: String, source: String) -> Result<(Program, SourceMap), Box<dyn Error>> {
    let mut parser = Parser {
        source: &source,
        at: 0,
        depth: 0,
        nodes: 0,
    };
    let expression = parser.expression(0)?;
    parser.space();
    if parser.at != source.len() {
        return Err(format!("unexpected input at byte {}", parser.at).into());
    }
    let mut body = Asm::new();
    let mut kinds = vec![ValueType::Unit];
    let mut ranges = Vec::new();
    fn emit(
        expression: &Expr,
        body: &mut Asm,
        kinds: &mut Vec<ValueType>,
        ranges: &mut Vec<(u32, Range<usize>)>,
    ) -> u32 {
        let (left, right) = match &expression.kind {
            ExprKind::Number(_) => (0, 0),
            ExprKind::Binary(_, left, right) => (
                emit(left, body, kinds, ranges),
                emit(right, body, kinds, ranges),
            ),
        };
        let reg = u32::try_from(kinds.len()).expect("bounded input registers");
        kinds.push(ValueType::I64);
        ranges.push((body.pc(), expression.range.clone()));
        match expression.kind {
            ExprKind::Number(value) => {
                body.const_i64(reg, value);
            }
            ExprKind::Binary(b'+', _, _) => {
                body.i64_add(reg, left, right);
            }
            ExprKind::Binary(b'-', _, _) => {
                body.i64_sub(reg, left, right);
            }
            ExprKind::Binary(b'*', _, _) => {
                body.i64_mul(reg, left, right);
            }
            _ => unreachable!(),
        }
        reg
    }
    let result = emit(&expression, &mut body, &mut kinds, &mut ranges);
    ranges.push((body.pc(), expression.range.clone()));
    body.ret(0, &[result]);
    body.set_register_types(kinds);
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("Arithmetic source frontend");
    let entry = builder.push_function_checked(
        body,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )?;
    builder.set_function_name(entry, "evaluate_expression")?;
    let program = builder.build_checked()?;
    let mappings = ranges
        .into_iter()
        .map(|(pc, range)| SourceMapping {
            location: TraceLocation { func: entry, pc },
            document: SourceId(0),
            range,
        })
        .collect();
    let map = SourceMap::new(
        &program,
        vec![SourceDocument { name, text: source }],
        mappings,
        SourceLimits::default(),
    )?;
    Ok((program, map))
}
struct NoHost;
impl Host for NoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        Err(HostError::UnknownSymbol)
    }
}
fn write_new(path: &std::path::Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}
fn main() -> Result<(), Box<dyn Error>> {
    let mut source_path = None;
    let mut write = None;
    let mut snapshot = false;
    let mut record = false;
    let mut explain = false;
    let mut colors = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!(
                "source_frontend [--source PATH] [--write-prefix PREFIX] [--record | --explain] [--snapshot] [--color=always]\nCompiles integer expressions with +, -, *, parentheses to ordinary tape.\n--write-prefix writes new PREFIX.tape and PREFIX.sources files, including supplied source text.\n--record opens actual execution with watched operands and source snapshots.\n--explain prints the recorded operands/source ranges and checks replay.\nOtherwise opens the source-linked browser, unless exporting. Source input limit: 1 MiB."
            );
            return Ok(());
        } else if arg == "--source" {
            if source_path.is_some() {
                return Err("--source may appear once".into());
            }
            source_path = Some(PathBuf::from(args.next().ok_or("--source needs a path")?));
        } else if arg == "--write-prefix" {
            if write.is_some() {
                return Err("--write-prefix may appear once".into());
            }
            write = Some(PathBuf::from(
                args.next().ok_or("--write-prefix needs a path")?,
            ));
        } else if arg == "--snapshot" {
            snapshot = true;
        } else if arg == "--record" {
            record = true;
        } else if arg == "--explain" {
            explain = true;
            record = true;
        } else if arg == "--color=always" {
            colors = true;
        } else {
            return Err(format!("unknown argument {arg:?}; use --help").into());
        }
    }
    let (name, source) = if let Some(path) = source_path {
        let mut bytes = Vec::new();
        std::fs::File::open(&path)?
            .take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err("source exceeds 1 MiB input limit".into());
        }
        (
            path.to_string_lossy().into_owned(),
            String::from_utf8(bytes)?,
        )
    } else {
        ("example.expr".into(), EXAMPLE.into())
    };
    let (program, sources) = compile(name, source)?;
    let prepared = std::sync::Arc::new(verify_program_owned(program, &VerifyConfig::default())?);
    let mut recorder = if record {
        let source = execution_tape::disasm::disassemble_function(
            prepared.program(),
            execution_tape::value::FuncId(0),
        )
        .map_err(|error| format!("cannot inspect emitted function: {error:?}"))?;
        let watches = source
            .instrs()
            .map(|i| execution_tape_recording::RegisterWatch {
                location: TraceLocation {
                    func: source.func(),
                    pc: i.pc(),
                },
                registers: i.reads().chain(i.writes()).collect(),
            })
            .collect();
        Some(
            execution_tape_recording::Recorder::new(
                std::sync::Arc::clone(&prepared),
                execution_tape_recording::RecordLimits::default(),
            )
            .with_instructions(execution_tape_recording::InstructionCapture {
                max_steps: 1024,
                watches,
            }),
        )
    } else {
        None
    };
    let mut vm = Vm::new(NoHost, Limits::default());
    let result = vm.run(
        &prepared,
        execution_tape::value::FuncId(0),
        &[],
        recorder.as_ref().map_or(TraceMask::NONE, TraceSink::mask),
        recorder
            .as_mut()
            .map(|recorder| recorder as &mut dyn TraceSink),
    )?;
    eprintln!("Result: {result:?}");
    if let Some(prefix) = write {
        let tape = prefix.with_extension("tape");
        let sidecar = prefix.with_extension("sources");
        write_new(&tape, &prepared.program().encode())?;
        write_new(&sidecar, &sources.encode())?;
        println!("Wrote {tape:?} and {sidecar:?}; source snapshots are included explicitly.");
        if !record {
            return Ok(());
        }
    }
    if colors && !snapshot {
        ratatui::crossterm::style::force_color_output(true);
    }
    if let Some(recorder) = recorder {
        let record = recorder.finish();
        record.status().map_err(Clone::clone)?;
        if explain {
            let bound = sources.bind(record.program().program())?;
            for (index, step) in record.instructions().enumerate() {
                let source = bound
                    .lookup(step.location)
                    .expect("frontend maps every instruction");
                println!(
                    "#{index} f{}:{:06} · {:?} · {:?}",
                    step.location.func.0,
                    step.location.pc,
                    execution_tape::opcode::Opcode::from_byte(step.opcode)
                        .expect("verified opcode"),
                    &source.document.text[source.range.clone()]
                );
                for (label, observations) in [("before", step.inputs), ("after", step.outputs)] {
                    for (reg, value) in observations.iter() {
                        println!("  {label} r{reg} = {value:?}");
                    }
                }
            }
            record.replay()?;
            println!(
                "History: {:?}; replay matched recorded path and values.",
                record.instruction_status().expect("enabled history")
            );
            return Ok(());
        }
        let explorer = RecordingExplorer::new(&record)
            .sources(&sources)?
            .instructions();
        if snapshot {
            print!("{}", explorer.render_text(160, 50)?);
        } else {
            explorer.run()?;
        }
    } else {
        let browser = ProgramBrowser::new(prepared.program()).sources(&sources)?;
        if snapshot {
            print!("{}", browser.render_text(120, 36)?);
        } else {
            browser.run()?;
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parsed_operations_own_exact_source_ranges_and_execute_normally() {
        let (program, sources) = compile("example.expr".into(), EXAMPLE.into()).unwrap();
        let prepared = verify_program_owned(program.clone(), &Default::default()).unwrap();
        let mut vm = Vm::new(NoHost, Limits::default());
        assert_eq!(
            vm.run(
                &prepared,
                execution_tape::value::FuncId(0),
                &[],
                TraceMask::NONE,
                None
            )
            .unwrap(),
            vec![Value::I64(24)]
        );
        let bound = sources.bind(&program).unwrap();
        let code = execution_tape::disasm::disassemble_function(
            &program,
            execution_tape::value::FuncId(0),
        )
        .unwrap();
        for instruction in code.instrs() {
            let range = bound
                .lookup(TraceLocation {
                    func: code.func(),
                    pc: instruction.pc(),
                })
                .unwrap();
            assert!(!range.range.is_empty());
            assert!(EXAMPLE.get(range.range).is_some());
        }
        let map = SourceMap::decode(&sources.encode(), SourceLimits::default()).unwrap();
        let text = ProgramBrowser::new(&program)
            .sources(&map)
            .unwrap()
            .render_text(80, 24)
            .unwrap();
        assert!(text.contains("FRONTEND SOURCE"));
        assert!(text.contains("example.expr"));
        assert!(text.contains("(7 + 5)"));
    }
    #[test]
    fn frontend_reports_syntax_and_nesting_errors_before_emitting_a_map() {
        for source in ["", "(1 +)", "1 2", "9223372036854775808", "1 / 2"] {
            assert!(compile("x".into(), source.into()).is_err(), "{source}");
        }
        assert!(compile("x".into(), vec!["1"; 600].join("+")).is_err());
        assert!(
            compile(
                "x".into(),
                format!("{}1{}", "(".repeat(130), ")".repeat(130))
            )
            .is_err()
        );
    }
}
