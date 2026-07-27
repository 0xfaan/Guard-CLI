//! Shared helpers for walking `#[contractimpl]` impl blocks.

use syn::{ImplItem, Item, ItemImpl};

pub fn is_contractimpl(item_impl: &ItemImpl) -> bool {
    item_impl
        .attrs
        .iter()
        .any(|attr| path_is_contractimpl(attr.path()))
}

fn path_is_contractimpl(path: &syn::Path) -> bool {
    path.segments
        .last()
        .is_some_and(|s| s.ident == "contractimpl")
}

/// Every function item inside a `#[contractimpl]` impl in the file.
fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        attr.parse_args::<syn::Ident>()
            .map(|id| id == "test")
            .unwrap_or(false)
    })
}

/// Every function item inside a `#[contractimpl]` impl that is **not** inside a
/// `#[cfg(test)]` module or a module named `tests`.
pub fn contractimpl_functions_excluding_test(file: &syn::File) -> Vec<&syn::ImplItemFn> {
    let mut out = Vec::new();
    collect_contractimpl_fns(&file.items, false, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_file;

    #[test]
    fn excludes_contractimpl_functions_inside_test_modules() -> Result<(), syn::Error> {
        let file = parse_file(
            r#"
#[contractimpl]
impl C {
    pub fn live(env: Env) {
        let _ = env;
    }
}

#[cfg(test)]
mod tests {
    use soroban_sdk::{contractimpl, Env};

    #[contractimpl]
    impl C {
        pub fn test_only(env: Env) {
            let _ = env;
        }
    }
}
"#,
        )?;

        let methods = contractimpl_functions_excluding_test(&file);
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].sig.ident.to_string(), "live");
        Ok(())
    }
}

fn collect_contractimpl_fns<'a>(
    items: &'a [Item],
    in_test_mod: bool,
    out: &mut Vec<&'a syn::ImplItemFn>,
) {
    for item in items {
        match item {
            Item::Mod(m) => {
                let is_test = in_test_mod
                    || is_cfg_test(&m.attrs)
                    || m.ident == "tests"
                    || m.ident == "test";
                if let Some((_, nested)) = &m.content {
                    collect_contractimpl_fns(nested, is_test, out);
                }
            }
            Item::Impl(item_impl) if !in_test_mod && is_contractimpl(item_impl) => {
                for impl_item in &item_impl.items {
                    if let ImplItem::Fn(m) = impl_item {
                        out.push(m);
                    }
                }
            }
            _ => {}
        }
    }
}
