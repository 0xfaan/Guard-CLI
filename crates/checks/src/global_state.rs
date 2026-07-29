//! Detects `static mut` items in Soroban contracts (mutable global state).

use crate::{Check, Finding, Severity};
use syn::{File, Item, ItemStatic};

const CHECK_NAME: &str = "mutable-global-state";

pub struct MutableGlobalStateCheck;

impl Check for MutableGlobalStateCheck {
    fn name(&self) -> &str {
        CHECK_NAME
    }

    fn run(&self, file: &File, _source: &str) -> Vec<Finding> {
        let mut statics = Vec::new();
        collect_static_items(&file.items, &mut statics);

        statics
            .into_iter()
            .filter_map(|ItemStatic { mutability, ident, .. }| {
                if matches!(mutability, syn::StaticMutability::Mut(_)) {
                    return Some(Finding {
                        check_name: CHECK_NAME.to_string(),
                        severity: Severity::High,
                        file_path: String::new(),
                        line: ident.span().start().line,
                        function_name: String::new(),
                        description: format!(
                            "`static mut {ident}` introduces mutable global state. \
                             In Soroban, contract instances are stateless between \
                             invocations — `static mut` is unsafe and its value is \
                             not persisted on-chain."
                        ),
                        rule_url: Some(
                            "https://github.com/SorobanGuard/Guard-CLI/blob/main/docs/checks.md#mutable-global-state-high"
                                .to_string(),
                        ),
                        suggestion: Some(
                            "Replace `static mut` with `env.storage().persistent()` or `env.storage().instance()` for on-chain state."
                                .to_string(),
                        ),
                    });
                }
                None
            })
            .collect()
    }
}

/// Every `static` item in the file, recursing into nested `mod` blocks.
fn collect_static_items<'a>(items: &'a [Item], out: &mut Vec<&'a ItemStatic>) {
    for item in items {
        match item {
            Item::Mod(m) => {
                if let Some((_, nested)) = &m.content {
                    collect_static_items(nested, out);
                }
            }
            Item::Static(item_static) => out.push(item_static),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_file;

    #[test]
    fn flags_static_mut() {
        let file = parse_file("static mut COUNT: u32 = 0;").unwrap();
        let hits = MutableGlobalStateCheck.run(&file, "");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].severity, Severity::High);
        assert!(hits[0].description.contains("COUNT"));
    }

    #[test]
    fn ignores_immutable_static() {
        let file = parse_file("static COUNT: u32 = 0;").unwrap();
        let hits = MutableGlobalStateCheck.run(&file, "");
        assert!(hits.is_empty());
    }
}
