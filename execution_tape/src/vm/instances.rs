// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Lazy instance registration. Weak slots do not retain retired code definitions.

use super::continuation::CodeOwner;
use super::*;
use crate::instance::{Instance, InstanceData, InstanceError};
use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
};

#[derive(Default)]
pub(super) struct Instances {
    store: Option<Arc<()>>,
    slots: Vec<Weak<InstanceData>>,
    // Bound retained metadata to one default instance; other instances survive through their
    // actual users. Repeated ordinary calls need no new namespace allocation.
    last_default: Option<Instance>,
    imported: Vec<(Weak<InstanceData>, Weak<InstanceData>)>,
}

impl Instances {
    fn insert(
        &mut self,
        program: &VerifiedProgram,
        is_default: bool,
    ) -> Result<Instance, InstanceError> {
        let vacant = self.slots.iter().position(|slot| slot.strong_count() == 0);
        let index = vacant.unwrap_or(self.slots.len());
        let slot = u32::try_from(index).map_err(|_| InstanceError::CapacityExceeded)?;
        let store = self.store.get_or_insert_with(|| Arc::new(()));
        let instance = Instance::new(program.clone(), Arc::clone(store), slot, is_default);
        if let Some(index) = vacant {
            self.slots[index] = Arc::downgrade(&instance.0);
        } else {
            self.slots.push(Arc::downgrade(&instance.0));
        }
        Ok(instance)
    }

    pub(super) fn default_for(&mut self, program: &VerifiedProgram) -> Result<Instance, Trap> {
        if let Some(instance) = &self.last_default
            && instance.program().shares_definition_with(program)
        {
            return Ok(instance.clone());
        }
        if let Some(instance) = self
            .slots
            .iter()
            .filter_map(Weak::upgrade)
            .find(|instance| {
                instance.is_default && instance.program.shares_definition_with(program)
            })
        {
            let instance = Instance(instance);
            self.last_default = Some(instance.clone());
            return Ok(instance);
        }
        let instance = self
            .insert(program, true)
            .map_err(|_| Trap::InstanceLimitExceeded)?;
        self.last_default = Some(instance.clone());
        Ok(instance)
    }

    fn owns(&self, instance: &Instance) -> bool {
        self.store
            .as_ref()
            .is_some_and(|store| Arc::ptr_eq(store, &instance.0.store))
    }

    fn import(&mut self, source: &Instance) -> Result<Instance, Trap> {
        if self.owns(source) {
            return Ok(source.clone());
        }
        self.imported
            .retain(|(source, target)| source.strong_count() != 0 && target.strong_count() != 0);
        if let Some(target) = self.imported.iter().find_map(|(original, target)| {
            Weak::ptr_eq(original, &Arc::downgrade(&source.0))
                .then(|| target.upgrade())
                .flatten()
        }) {
            return Ok(Instance(target));
        }
        // Defaults are local to their original store. Two source stores using the same
        // definition still have distinct namespace identities and must not merge on import.
        let target = self
            .insert(source.program(), false)
            .map_err(|_| Trap::InstanceLimitExceeded)?;
        self.imported
            .push((Arc::downgrade(&source.0), Arc::downgrade(&target.0)));
        Ok(target)
    }
}

impl<H: Host> Vm<H> {
    /// Binds one retained code namespace to this VM's host store.
    ///
    /// A same-store instance preserves its exact identity. A foreign instance receives a
    /// destination namespace; repeated bindings preserve the association while both remain live.
    /// Resolve functions with `bound.function(source.id())` and preserve fresh control operation
    /// identity with `tag.in_instance(&bound)`. This transfers no host state or heap handles;
    /// closure environments still require an owned capture import.
    pub fn bind_instance(&mut self, source: &Instance) -> Result<Instance, InstanceError> {
        self.instances
            .import(source)
            .map_err(|_| InstanceError::CapacityExceeded)
    }

    /// Resolves a function in this VM's default namespace for the exact prepared definition.
    ///
    /// The result can be supplied as a runtime function value. It retains code and namespace
    /// identity without executing instructions or retaining the VM. Use [`crate::instance::Instance::function`]
    /// with an explicit [`Instance`] when separate host namespaces are required for one definition.
    pub fn function(
        &mut self,
        program: &VerifiedProgram,
        function: FuncId,
    ) -> Result<crate::value::FunctionRef, InstanceError> {
        if program.function(function).is_none() {
            return Err(InstanceError::MissingFunction { function });
        }
        let instance = self
            .instances
            .default_for(program)
            .map_err(|_| InstanceError::CapacityExceeded)?;
        Ok(crate::value::FunctionRef::from_verified(instance, function))
    }

    // Explicit owned-value import reconnects code namespaces to the destination host store.
    // Heap-only copying preserves function identity and cannot grant that store authority.
    pub(crate) fn import_capture(
        &mut self,
        snapshot: &crate::aggregates::ValueSnapshot,
    ) -> Result<Vec<Value>, Trap> {
        let first = self.memory.aggs().len_u32();
        let mut values = self
            .memory
            .aggs_mut()
            .import(snapshot)
            .map_err(Trap::AggError)?;
        let mut remap = |value: &mut Value| {
            let function = match value {
                Value::Tag(tag) => {
                    let instance = self.instances.import(tag.instance())?;
                    *tag = tag.rebind(instance);
                    return Ok(());
                }
                Value::Continuation(_) => {
                    return Err(Trap::ForeignStoreValue {
                        kind: ValueType::Continuation,
                    });
                }
                Value::Func(function) => function,
                Value::Closure(closure) => &mut closure.func,
                _ => return Ok(()),
            };
            let instance = self.instances.import(function.instance())?;
            *function = crate::value::FunctionRef::from_verified(instance, function.id());
            Ok::<(), Trap>(())
        };
        let result = self
            .memory
            .aggs_mut()
            .visit_values_mut_from(first, &mut remap)
            .and_then(|()| values.iter_mut().try_for_each(remap));
        if let Err(error) = result {
            self.memory.aggs_mut().truncate(first);
            return Err(error);
        }
        Ok(values)
    }

    /// Recreates an observed namespace in a fresh replay store, preserving default-entry aliasing.
    pub(crate) fn instantiate_recorded(&mut self, source: &Instance) -> Result<Instance, Trap> {
        if source.0.is_default {
            self.instances.default_for(source.program())
        } else {
            self.instantiate(source.program())
                .map_err(|_| Trap::InstanceLimitExceeded)
        }
    }

    /// Creates a distinct instance of shared verified code in this VM store.
    ///
    /// This retains code and namespace identity without executing guest instructions or host
    /// imports. Mutable state remains with the host; `HostContext::instance` identifies the
    /// active namespace. Keeping an instance does not keep this VM or its heap alive.
    pub fn instantiate(&mut self, program: &VerifiedProgram) -> Result<Instance, InstanceError> {
        self.instances.insert(program, false)
    }

    /// Whether this instance belongs to this VM store. The query never allocates or executes.
    #[must_use]
    pub fn owns_instance(&self, instance: &Instance) -> bool {
        self.instances.owns(instance)
    }
}

impl Instance {
    /// Starts an entry in this exact instance, using the originating VM's host and heap.
    ///
    /// Foreign-store denial is recoverable and occurs before entry observations or guest work.
    /// Arguments use the original heap and code ABI; this does not import foreign raw handles.
    /// The invocation retains the instance even if this handle is dropped while it is paused.
    pub fn start<'a, H: Host>(
        &self,
        vm: &'a mut Vm<H>,
        entry: FuncId,
        args: &'a [Value],
        options: InvocationOptions<'a>,
    ) -> Result<Invocation<'a, H>, InstanceError> {
        self.start_prepaid(vm, entry, args, options, DataWork::default())
    }

    pub(crate) fn start_prepaid<'a, H: Host>(
        &self,
        vm: &'a mut Vm<H>,
        entry: FuncId,
        args: &'a [Value],
        options: InvocationOptions<'a>,
        work: DataWork,
    ) -> Result<Invocation<'a, H>, InstanceError> {
        if !vm.owns_instance(self) {
            return Err(InstanceError::ForeignStore);
        }
        let checkpoint = vm.memory.checkpoint();
        Invocation::prepare(
            BorrowedOrOwned::Borrowed(vm),
            CodeOwner::Instance(self.clone()),
            entry,
            Cow::Borrowed(args),
            options,
            checkpoint,
            work,
        )
        .map_err(InstanceError::Trapped)
    }
}

impl<H: Host> Invocation<'_, H> {
    /// Instantiates newly verified code in this execution's existing store.
    ///
    /// This may be done while guest frames are suspended. It retains a new code namespace
    /// without changing live frames, values, pending operations or execution budgets. Future
    /// dispatch can select the returned instance while active calls retain their old code.
    pub fn instantiate(&mut self, program: &VerifiedProgram) -> Result<Instance, InstanceError> {
        self.vm.instantiate(program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{FunctionSig, ProgramBuilder};

    struct NoHost;
    impl Host for NoHost {
        fn call(
            &mut self,
            _: &str,
            _: crate::host::SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            panic!("scalar code has no host boundary")
        }
    }
    fn scalar() -> VerifiedProgram {
        let mut builder = ProgramBuilder::new();
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = builder.function_asm(entry).unwrap();
        body.ret(0, &[1]);
        builder.define_function(entry, body).unwrap();
        builder.build_verified().unwrap()
    }

    #[test]
    fn scalar_execution_does_not_allocate_instance_metadata() {
        let program = scalar();
        let mut vm = Vm::new(NoHost, Limits::default());
        assert_eq!(
            vm.run(
                &program,
                FuncId(0),
                &[Value::I64(42)],
                TraceMask::NONE,
                None
            )
            .unwrap(),
            [Value::I64(42)]
        );
        {
            let args = [Value::I64(42)];
            let mut call = vm
                .start(&program, FuncId(0), &args, InvocationOptions::default())
                .unwrap();
            assert!(call.instance().is_none());
            call.resume(1).unwrap();
            assert!(call.instance().is_none());
        }
        assert!(vm.instances.store.is_none());
        assert_eq!(vm.instances.slots.capacity(), 0);
        assert!(vm.instances.last_default.is_none());
    }

    #[test]
    fn retired_instances_reuse_slots_without_retaining_their_code() {
        let program = scalar();
        let mut vm = Vm::new(NoHost, Limits::default());
        for _ in 0..1000 {
            let instance = vm.instantiate(&program).unwrap();
            let retired = Arc::downgrade(&instance.0);
            drop(instance);
            assert!(retired.upgrade().is_none());
        }
        assert_eq!(vm.instances.slots.len(), 1);
        for _ in 0..1000 {
            let instance = vm.instances.default_for(&scalar()).unwrap();
            assert!(vm.owns_instance(&instance));
        }
        assert_eq!(
            vm.instances.slots.len(),
            2,
            "only the latest default is retained"
        );
        assert_eq!(
            vm.instances
                .slots
                .iter()
                .filter(|slot| slot.strong_count() != 0)
                .count(),
            1
        );
    }
}
