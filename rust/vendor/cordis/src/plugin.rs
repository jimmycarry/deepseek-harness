use crate::{Context, Result};

/// A plugin that contributes services, events, or effects to a context.
pub trait Plugin: Send + Sync + 'static {
    /// Stable plugin name used in dump-config and diagnostics.
    fn name(&self) -> &'static str;

    /// Required service keys. The fiber stays Pending until each is Active.
    fn inject(&self) -> &'static [&'static str] {
        &[]
    }

    /// Mount this plugin on `ctx`. Registrations must go through effects.
    fn apply(&self, ctx: &Context) -> Result<()>;
}

/// Function-plugin wrapper: `{ name, inject, apply }`.
pub struct FnPlugin<F>
where
    F: Fn(&Context) -> Result<()> + Send + Sync + 'static,
{
    name: &'static str,
    inject: &'static [&'static str],
    apply: F,
}

impl<F> FnPlugin<F>
where
    F: Fn(&Context) -> Result<()> + Send + Sync + 'static,
{
    /// Wrap a function plugin.
    pub fn new(name: &'static str, apply: F) -> Self {
        Self {
            name,
            inject: &[],
            apply,
        }
    }

    /// Declare required services.
    pub fn with_inject(mut self, inject: &'static [&'static str]) -> Self {
        self.inject = inject;
        self
    }
}

impl<F> Plugin for FnPlugin<F>
where
    F: Fn(&Context) -> Result<()> + Send + Sync + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn inject(&self) -> &'static [&'static str] {
        self.inject
    }

    fn apply(&self, ctx: &Context) -> Result<()> {
        (self.apply)(ctx)
    }
}
