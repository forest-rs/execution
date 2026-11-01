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
}

impl<H: Host> Vm<H> {
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
