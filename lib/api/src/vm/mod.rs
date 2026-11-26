//! This module defines traits to handle abstractions created by the runtimes.

mod impls;

use crate::{VMExternToExtern, macros::backend::gen_rt_ty};
use wasmer_types::RawValue;

macro_rules! define_vm_like {
    ($name:ident $(, $derives:ident)*) => {
        paste::paste! {
            gen_rt_ty! {
                #[derive($($derives,)*)]
                pub [<VM $name>](vm::[<VM $name>]);
            }
        }
    };
}

define_vm_like!(Extern);
define_vm_like!(ExternFunction, Debug);
define_vm_like!(ExternGlobal);
define_vm_like!(ExternTag);
define_vm_like!(ExternMemory);
define_vm_like!(ExternTable);
define_vm_like!(FunctionCallback, Debug);
define_vm_like!(Trampoline, Debug);

define_vm_like!(Memory, Debug);
define_vm_like!(SharedMemory);

define_vm_like!(ExceptionRef);
// `VMExternRef` is an implementation detail of `ExternRef`; keep it out of the
// public API.
gen_rt_ty! {
    pub(crate) VMExternRef(vm::VMExternRef);
}
define_vm_like!(FuncRef);
