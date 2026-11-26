use wasmer_types::{TagType, Type};

use crate::{
    AsStoreMut, AsStoreRef, BackendTag, RuntimeError, TagKind,
    js::vm::VMTag,
    vm::{VMExtern, VMExternTag},
};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A WebAssembly `tag` in the `js` runtime.
pub(crate) struct Tag {
    pub(crate) handle: VMTag,
}

unsafe impl Send for Tag {}
unsafe impl Sync for Tag {}

// Tag can't be Send in js because it doesn't support `structuredClone`
// https://developer.mozilla.org/en-US/docs/Web/API/structuredClone
// unsafe impl Send for Tag {}

impl Tag {
    pub fn new<P: Into<Box<[Type]>>>(
        _store: &mut impl AsStoreMut,
        params: P,
    ) -> Result<Self, RuntimeError> {
        let descriptor = js_sys::Object::new();
        let params: Box<[Type]> = params.into();
        let parameters = params
            .iter()
            .map(|param| match param {
                Type::I32 => Ok("i32"),
                Type::I64 => Ok("i64"),
                Type::F32 => Ok("f32"),
                Type::F64 => Ok("f64"),
                other => Err(RuntimeError::new(format!(
                    "a tag parameter of type {other} is not supported by the js backend"
                ))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        js_sys::Reflect::set(&descriptor, &"parameters".into(), &parameters.into())
            .map_err(|_| RuntimeError::new("could not describe the tag to the JavaScript host"))?;

        let tag = js_sys::WebAssembly::Tag::new(&descriptor).map_err(|error| {
            RuntimeError::new(format!(
                "the JavaScript host could not make the tag: {}",
                error.as_string().unwrap_or_else(|| format!("{error:?}"))
            ))
        })?;
        let ty = TagType::new(TagKind::Exception, params);
        Ok(Self {
            handle: VMTag::new(tag, ty),
        })
    }

    pub fn ty(&self, _store: &impl AsStoreRef) -> TagType {
        self.handle.ty.clone()
    }

    pub(crate) fn from_vm_extern(_store: &mut impl AsStoreMut, vm_extern: VMExternTag) -> Self {
        Self {
            handle: vm_extern.unwrap_js(),
        }
    }

    pub fn is_from_store(&self, _store: &impl AsStoreRef) -> bool {
        true
    }

    pub(crate) fn to_vm_extern(&self) -> VMExtern {
        VMExtern::Js(crate::js::vm::VMExtern::Tag(self.handle.clone()))
    }
}

impl crate::Tag {
    /// Convert a reference to [`self`] into a reference [`crate::backend::js::tag::Tag`].
    pub(crate) fn as_js(&self) -> &crate::backend::js::tag::Tag {
        match self.0 {
            BackendTag::Js(ref s) => s,
            _ => panic!("Not a `js` tag!"),
        }
    }
}
