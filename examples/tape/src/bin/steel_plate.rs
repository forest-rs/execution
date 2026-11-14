// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A procedural material's build logic as a tape program, against a mock material kernel.
//!
//! The body configures and instantiates child modules through opaque host handles: steel, then
//! paint on one branch (with parameters derived from `age` when `weather` is set), then rust. It
//! shows typed host imports shared by the program and the host, constants passed directly as
//! call arguments, and a load-time check of constant parameter names against the child modules'
//! schemas, so a misspelt name is rejected with its location before anything runs.
//!
//! `cargo run -p execution_tape_examples --bin steel_plate`

use std::sync::Arc;

use execution_tape::asm::{CallArg, FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{HostContext, HostError, ValueRef};
use execution_tape::host_binding::{HostFunctions, HostImport};
use execution_tape::program::{HostTypeId, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncRef, Obj, ObjHandle, Value};
use execution_tape::verifier::{HostCallSite, VerifiedProgram};
use execution_tape::vm::{Limits, Vm};

use ValueType::{Bool, F32, Obj as O, Str, U32, U64};

/// A binding under construction.
const BIND: HostTypeId = HostTypeId(1);
/// An instance's outputs.
const OUTPUTS: HostTypeId = HostTypeId(2);
/// A material.
const MATERIAL: HostTypeId = HostTypeId(3);

// The kernel surface, declared once for the program and the host.
const BIND_NEW: HostImport = HostImport::new("kernel.bind.new", &[], &[O(BIND)]);
const BIND_SCALAR: HostImport = HostImport::new("kernel.bind.scalar", &[O(BIND), Str, F32], &[]);
const BIND_COLOR: HostImport =
    HostImport::new("kernel.bind.color", &[O(BIND), Str, F32, F32, F32], &[]);
const BIND_MATERIAL: HostImport =
    HostImport::new("kernel.bind.material", &[O(BIND), Str, O(MATERIAL)], &[]);
const INSTANTIATE: HostImport = HostImport::new(
    "kernel.instantiate",
    &[Str, U32, Str, O(BIND)],
    &[O(OUTPUTS)],
);
const OUTPUTS_MATERIAL: HostImport = HostImport::new(
    "kernel.outputs.material",
    &[O(OUTPUTS), Str],
    &[O(MATERIAL)],
);
const OUTPUT_MATERIAL: HostImport =
    HostImport::new("kernel.output.material", &[Str, O(MATERIAL)], &[]);

/// The child modules the body may instantiate, at version 1, with their parameter names.
const MODULES: [(&str, &[&str]); 3] = [
    ("library.steel", &["brushing", "mill_scale"]),
    (
        "library.paint",
        &["base", "color", "gloss", "chipping", "fade"],
    ),
    ("library.rust", &["base", "age", "wetness"]),
];

/// `finish`: bare steel under mill scale.
const BARE: u32 = 0;
/// `finish`: primer and topcoat.
const PAINTED: u32 = 1;

/// What `weather` derives from `age`, in native Rust.
fn weathered(chipping: f32, fade: f32, age: f32) -> (f32, f32) {
    (chipping.max(0.6 * age * age), (fade + 0.9 * age).min(1.0))
}

/// The steel plate's body as a tape program.
fn program() -> Result<VerifiedProgram, Box<dyn std::error::Error>> {
    // --- tape source: begin
    let mut pb = ProgramBuilder::new();
    pb.set_program_name("example.steel_plate@1");
    let [bind_new, bind_scalar, bind_color, bind_material] =
        [BIND_NEW, BIND_SCALAR, BIND_COLOR, BIND_MATERIAL].map(|i| pb.host_import(i));
    let [instantiate, outputs_material, output_material] =
        [INSTANTIATE, OUTPUTS_MATERIAL, OUTPUT_MATERIAL].map(|i| pb.host_import(i));

    // Parameters arrive as typed arguments, in interface order.
    let names = [
        "finish",
        "brushing",
        "mill_scale",
        "paint_r",
        "paint_g",
        "paint_b",
        "gloss",
        "chipping",
        "fade",
        "age",
        "wetness",
        "weather",
        "seed",
    ];
    let entry = pb.declare_function(FunctionSig {
        arg_types: vec![
            U32, F32, F32, F32, F32, F32, F32, F32, F32, F32, F32, Bool, U64,
        ],
        ret_types: vec![],
    })?;
    for (i, n) in (0..).zip(names) {
        pb.set_function_input_name(entry, i, n)?;
    }
    let arg = |i: u32| Reg(pb.argument(entry, i).expect("declared argument"));
    let [finish, brushing, mill_scale, paint_r, paint_g, paint_b] = [0, 1, 2, 3, 4, 5].map(arg);
    let [gloss, chipping, fade, age, wetness, weather] = [6, 7, 8, 9, 10, 11].map(arg);
    let mut a = pb.function_asm(entry)?;
    // Constants several calls share, loaded once at entry so that they reach every branch.
    let [material, base_input] = ["material", "base"].map(|name| pb.emit_const(&mut a, name));
    let version = pb.emit_const(&mut a, CallArg::U32(1));

    // let steel = bind(); if finish == BARE { steel.brushing = ..; steel.mill_scale = .. }
    let (bare, is_bare) = (a.alloc_register(U32), a.alloc_register(Bool));
    a.const_u32(bare, BARE).u32_eq(is_bare, finish.0, bare);
    let [steel] = pb.emit_call_with(&mut a, bind_new, ())?;
    let (l_bare, l_steel) = (a.label(), a.label());
    a.br(is_bare, l_bare, l_steel);
    a.place(l_bare)?;
    let [] = pb.emit_call_with(&mut a, bind_scalar, (steel, "brushing", brushing))?;
    let [] = pb.emit_call_with(&mut a, bind_scalar, (steel, "mill_scale", mill_scale))?;
    a.jmp(l_steel);
    a.place(l_steel)?;

    // let base = instantiate(steel@1 as "steel", steel).material
    let module = ("library.steel", version, "steel", steel);
    let [out] = pb.emit_call_with(&mut a, instantiate, module)?;
    let [steel_m] = pb.emit_call_with(&mut a, outputs_material, (out, material))?;
    let base = Reg(a.alloc_register(O(MATERIAL)));
    a.mov(base.0, steel_m.0);

    // if finish != BARE { derive; paint }
    let (l_paint, l_rust) = (a.label(), a.label());
    a.br(is_bare, l_rust, l_paint);
    a.place(l_paint)?;
    let (chip, fd) = (a.alloc_register(F32), a.alloc_register(F32));
    a.mov(chip, chipping.0).mov(fd, fade.0);
    let (l_weather, l_painting) = (a.label(), a.label());
    a.br(weather.0, l_weather, l_painting);
    a.place(l_weather)?;
    // chip = max(chipping, 0.6 * age * age); fd = min(fade + 0.9 * age, 1)
    // `max_num`/`min_num`: Rust's `f32::max`/`min` favor numbers.
    let (k, t) = (a.alloc_register(F32), a.alloc_register(F32));
    a.const_f32(k, 0.6).f32_mul(t, k, age.0);
    a.f32_mul(t, t, age.0).f32_max_num(chip, chipping.0, t);
    a.const_f32(k, 0.9).f32_mul(t, k, age.0);
    a.f32_add(t, fade.0, t);
    a.const_f32(k, 1.0).f32_min_num(fd, t, k);
    a.jmp(l_painting);
    a.place(l_painting)?;
    let [paint] = pb.emit_call_with(&mut a, bind_new, ())?;
    let [] = pb.emit_call_with(&mut a, bind_material, (paint, base_input, base))?;
    let color = (paint, "color", paint_r, paint_g, paint_b);
    let [] = pb.emit_call_with(&mut a, bind_color, color)?;
    let [] = pb.emit_call_with(&mut a, bind_scalar, (paint, "gloss", gloss))?;
    let [] = pb.emit_call_with(&mut a, bind_scalar, (paint, "chipping", Reg(chip)))?;
    let [] = pb.emit_call_with(&mut a, bind_scalar, (paint, "fade", Reg(fd)))?;
    let module = ("library.paint", version, "paint", paint);
    let [out] = pb.emit_call_with(&mut a, instantiate, module)?;
    let [painted] = pb.emit_call_with(&mut a, outputs_material, (out, material))?;
    a.mov(base.0, painted.0);
    a.jmp(l_rust);
    a.place(l_rust)?;

    // output.material = instantiate(rust@1 as "rust", { base, age, wetness }).material
    let [rust] = pb.emit_call_with(&mut a, bind_new, ())?;
    let [] = pb.emit_call_with(&mut a, bind_material, (rust, base_input, base))?;
    let [] = pb.emit_call_with(&mut a, bind_scalar, (rust, "age", age))?;
    let [] = pb.emit_call_with(&mut a, bind_scalar, (rust, "wetness", wetness))?;
    let module = ("library.rust", version, "rust", rust);
    let [out] = pb.emit_call_with(&mut a, instantiate, module)?;
    let [m] = pb.emit_call_with(&mut a, outputs_material, (out, material))?;
    let [] = pb.emit_call_with(&mut a, output_material, (material, m))?;
    a.ret(0, &[]);
    pb.define_function(entry, a)?;
    Ok(pb.build_verified()?)
    // --- tape source: end
}

/// The link check: every parameter name and module the body names must exist, before it runs.
fn check(site: &HostCallSite<'_>) -> Result<(), String> {
    let binds = [BIND_SCALAR, BIND_COLOR, BIND_MATERIAL];
    if binds.into_iter().any(|op| site.calls(op)) {
        return match site.arg(1) {
            Some(ValueRef::Str(name)) if MODULES.iter().any(|(_, p)| p.contains(&name)) => Ok(()),
            Some(ValueRef::Str(name)) => Err(format!("no child module has a parameter {name:?}")),
            _ => Err("a parameter name must be a constant".into()),
        };
    }
    if site.calls(INSTANTIATE) {
        return match (site.arg(0), site.arg(1)) {
            (Some(ValueRef::Str(name)), Some(ValueRef::U32(1)))
                if MODULES.iter().any(|(m, _)| *m == name) =>
            {
                Ok(())
            }
            _ => Err("instantiates a module that is not required".into()),
        };
    }
    Ok(())
}

/// The mock kernel: bindings and instances as plain records.
#[derive(Default)]
struct Kernel {
    binds: Vec<Vec<(String, Value)>>,
    /// Each instance's module and bindings, in instantiation order.
    instances: Vec<(String, Vec<(String, Value)>)>,
    output: Option<Value>,
}

type HostResult = Result<u64, HostError>;

fn handle(index: usize, host_type: HostTypeId) -> Value {
    Value::Obj(Obj {
        host_type,
        handle: ObjHandle(index as u64),
    })
}

fn bind_new(
    k: &mut Kernel,
    _: &[ValueRef<'_>],
    rets: &mut [Value],
    _: HostContext<'_, '_>,
) -> HostResult {
    k.binds.push(Vec::new());
    rets[0] = handle(k.binds.len() - 1, BIND);
    Ok(0)
}

fn bind(
    k: &mut Kernel,
    args: &[ValueRef<'_>],
    _: &mut [Value],
    _: HostContext<'_, '_>,
) -> HostResult {
    let [ValueRef::Obj(b), ValueRef::Str(name), values @ ..] = args else {
        return Err(HostError::Failed);
    };
    let value = match values {
        [v] => v.to_value(),
        // A color is recorded as its red channel; the example only inspects scalars.
        [r, _, _] => r.to_value(),
        _ => return Err(HostError::Failed),
    };
    let bind = k
        .binds
        .get_mut(b.handle.0 as usize)
        .ok_or(HostError::Failed)?;
    bind.push(((*name).into(), value));
    Ok(0)
}

fn instantiate(
    k: &mut Kernel,
    args: &[ValueRef<'_>],
    rets: &mut [Value],
    _: HostContext<'_, '_>,
) -> HostResult {
    let [ValueRef::Str(module), _, _, ValueRef::Obj(b)] = args else {
        return Err(HostError::Failed);
    };
    let bind = std::mem::take(&mut k.binds[b.handle.0 as usize]);
    k.instances.push(((*module).into(), bind));
    rets[0] = handle(k.instances.len() - 1, OUTPUTS);
    Ok(0)
}

fn outputs_material(
    _: &mut Kernel,
    args: &[ValueRef<'_>],
    rets: &mut [Value],
    _: HostContext<'_, '_>,
) -> HostResult {
    let [ValueRef::Obj(out), ValueRef::Str("material")] = args else {
        return Err(HostError::Failed);
    };
    rets[0] = handle(out.handle.0 as usize, MATERIAL);
    Ok(0)
}

fn output_material(
    k: &mut Kernel,
    args: &[ValueRef<'_>],
    _: &mut [Value],
    _: HostContext<'_, '_>,
) -> HostResult {
    k.output = Some(args[1].to_value());
    Ok(0)
}

fn kernel() -> Result<HostFunctions<Kernel>, Box<dyn std::error::Error>> {
    let mut functions = HostFunctions::new();
    functions.implement(BIND_NEW, bind_new)?;
    for op in [BIND_SCALAR, BIND_COLOR, BIND_MATERIAL] {
        functions.implement(op, bind)?;
    }
    functions.implement(INSTANTIATE, instantiate)?;
    functions.implement(OUTPUTS_MATERIAL, outputs_material)?;
    functions.implement(OUTPUT_MATERIAL, output_material)?;
    Ok(functions)
}

/// A body with one misspelt parameter name.
fn misspelt() -> Result<VerifiedProgram, Box<dyn std::error::Error>> {
    let mut pb = ProgramBuilder::new();
    let [bind_new, bind_scalar] = [BIND_NEW, BIND_SCALAR].map(|i| pb.host_import(i));
    let entry = pb.declare_function(FunctionSig {
        arg_types: vec![F32],
        ret_types: vec![],
    })?;
    pb.set_function_name(entry, "rusted_plate")?;
    let age = Reg(pb.argument(entry, 0)?);
    let mut a = pb.function_asm(entry)?;
    let [rust] = pb.emit_call_with(&mut a, bind_new, ())?;
    let [] = pb.emit_call_with(&mut a, bind_scalar, (rust, "rustiness", age))?;
    a.ret(0, &[]);
    pb.define_function(entry, a)?;
    Ok(pb.build_verified()?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let functions = kernel()?;
    let program = Arc::new(program()?);
    let bindings = functions.bind_checked(&program, check)?;
    println!(
        "link check passed: {} host calls",
        program.host_call_sites().len()
    );

    let (chipping, fade, age) = (0.03_f32, 0.3_f32, 0.4_f32);
    let args = [
        Value::U32(PAINTED),
        Value::F32(0.0),
        Value::F32(0.6),
        Value::F32(0.035),
        Value::F32(0.085),
        Value::F32(0.11),
        Value::F32(0.5),
        Value::F32(chipping),
        Value::F32(fade),
        Value::F32(age),
        Value::F32(0.6),
        Value::Bool(true),
        Value::U64(0),
    ];
    let mut vm = Vm::new(bindings.with_state(Kernel::default()), Limits::default());
    let unit = vm.install(Arc::clone(&program))?;
    let entry = FuncRef::new(unit, execution_tape::value::FuncId(0));
    vm.run(entry, &args, TraceMask::NONE, None)?;
    let kernel = vm.host().state();
    for (module, binds) in &kernel.instances {
        println!("{module}: {binds:?}");
    }
    let modules: Vec<&str> = kernel.instances.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(modules, ["library.steel", "library.paint", "library.rust"]);
    // The derived parameters match native f32 arithmetic bit for bit.
    let (chip, fd) = weathered(chipping, fade, age);
    let paint = &kernel.instances[1].1;
    assert!(paint.contains(&("chipping".into(), Value::F32(chip))));
    assert!(paint.contains(&("fade".into(), Value::F32(fd))));
    assert_eq!(kernel.output, Some(handle(2, MATERIAL)));

    let misspelt = misspelt()?;
    let error = functions.bind_checked(&misspelt, check).unwrap_err();
    println!("rejected at load: {error}");
    let [call] = &error.calls[..] else {
        panic!("expected one rejected call: {error}");
    };
    assert_eq!(
        call.to_string(),
        "function #0, pc 10, host call kernel.bind.scalar: \
         no child module has a parameter \"rustiness\""
    );
    // With the program at hand, the location names the function and shows the instruction.
    let named = call.display_in(misspelt.program()).to_string();
    println!("  {named}");
    assert!(named.starts_with("function #0 \"rusted_plate\", pc 10 (host_call "));
    assert!(
        named.ends_with(
            "host call kernel.bind.scalar: no child module has a parameter \"rustiness\""
        )
    );
    Ok(())
}
