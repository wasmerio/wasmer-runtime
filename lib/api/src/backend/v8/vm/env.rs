use std::{
    any::Any,
    sync::{Arc, Weak},
};

/// Underlying Store-owned function environment. Lending moves the SAME stable
/// Box out, leaving a checked marker; no second mutable reference can be made.
#[derive(Debug)]
pub struct VMFunctionEnvironment {
    pub(crate) contents: Option<Box<dyn Any + Send + 'static>>,
    pub(crate) identity: Arc<()>,
}
impl VMFunctionEnvironment {
    /// Wraps the given host value.
    pub fn new(value: impl Any + Send + 'static) -> Self {
        Self {
            contents: Some(Box::new(value)),
            identity: Arc::new(()),
        }
    }
    /// Borrow data only while it is not separately lent from its Store.
    #[allow(clippy::should_implement_trait)]
    pub fn as_ref(&self) -> &(dyn Any + Send + 'static) {
        &**self
            .contents
            .as_ref()
            .expect("V8 function environment is separately lent")
    }
    /// Borrow mutable data only while it is not separately lent.
    #[allow(clippy::should_implement_trait)]
    pub fn as_mut(&mut self) -> &mut (dyn Any + Send + 'static) {
        &mut **self
            .contents
            .as_mut()
            .expect("V8 function environment is separately lent")
    }
    pub(crate) fn matches(&self, expected: &Weak<()>) -> bool {
        expected.ptr_eq(&Arc::downgrade(&self.identity))
    }
}
