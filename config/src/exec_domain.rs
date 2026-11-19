use crate::config::validate_domain_name;
use luahelper::impl_lua_conversion_dynamic;
use wezterm_dynamic::{FromDynamic, ToDynamic, Value};

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub enum ValueOrFunc {
    Value(Value),
    Func(String),
}
impl_lua_conversion_dynamic!(ValueOrFunc);

#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct ExecDomain {
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,
    pub fixup_command: String,
    pub label: Option<ValueOrFunc>,
}
impl_lua_conversion_dynamic!(ExecDomain);

/// Native Rex server domain. `session` is an ID or a label to find/create.
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct RexDomain {
    #[dynamic(validate = "validate_domain_name")]
    pub name: String,
    pub endpoint: String,
    pub session: String,
    #[dynamic(default)]
    pub initial_cwd: Option<String>,
    #[dynamic(default)]
    pub remote_shell: bool,
    #[dynamic(default)]
    pub independent: bool,
}
impl_lua_conversion_dynamic!(RexDomain);

/// Native floating-pane decorations, selected by the server block label.
#[derive(Debug, Clone, FromDynamic, ToDynamic)]
pub struct RexPopupStyle {
    pub border_style:String,
    pub title:String,
}
