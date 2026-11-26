use crate::macros::backend::{gen_rt_ty, match_rt};

gen_rt_ty! {
    #[derive(Debug, Clone)]
    pub(crate) BackendEngine(entities::engine::Engine);
}

impl BackendEngine {
    /// Returns the deterministic id of this engine.
    #[inline]
    pub fn deterministic_id(&self) -> String {
        match_rt!(on self  => s {
            s.deterministic_id()
        })
    }
}

impl Default for BackendEngine {
    #[allow(unreachable_code)]
    #[inline]
    fn default() -> Self {
        #[cfg(feature = "sys-default")]
        {
            return Self::Sys(crate::backend::sys::entities::engine::default_engine());
        }

        #[cfg(feature = "v8-default")]
        {
            return Self::V8(crate::backend::v8::entities::engine::default_engine());
        }

        #[cfg(feature = "js-default")]
        {
            return Self::Js(crate::backend::js::entities::engine::default_engine());
        }

        #[cfg(feature = "sys")]
        {
            return Self::Sys(crate::backend::sys::entities::engine::default_engine());
        }

        #[cfg(feature = "v8")]
        {
            return Self::V8(crate::backend::v8::entities::engine::default_engine());
        }

        #[cfg(feature = "js")]
        {
            return Self::Js(crate::backend::js::entities::engine::default_engine());
        }

        panic!("No runtime enabled!")
    }
}
