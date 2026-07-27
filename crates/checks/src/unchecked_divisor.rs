use crate::{Check, Finding, Severity};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{BinOp, Expr, ExprBinary, File, ImplItem, ItemImpl};

const CHECK_NAME: &str = "unchecked-divisor";

pub struct UncheckedDivisorCheck;

impl Check for UncheckedDivisorCheck {
    fn name(&self) -> &str {
        CHECK_NAME
    }

    fn run(&self, file: &File, _source: &str) -> Vec<Finding> {
        let mut visitor = DivisorVisitor::default();
        visit::visit_file(&mut visitor, file);
        visitor.findings
    }
}

#[derive(Default)]
struct DivisorVisitor {
    findings: Vec<Finding>,
    current_function_name: String,
}

impl<'ast> Visit<'ast> for DivisorVisitor {
    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        for item in &node.items {
            if let ImplItem::Fn(method) = item {
                let old_function_name = self.current_function_name.clone();
                self.current_function_name = method.sig.ident.to_string();

                let mut expr_visitor = DivisorExprVisitor {
                    findings: Vec::new(),
                    current_function_name: self.current_function_name.clone(),
                };
                visit::visit_block(&mut expr_visitor, &method.block);
                self.findings.extend(expr_visitor.findings);

                self.current_function_name = old_function_name;
            }
        }
        visit::visit_item_impl(self, node);
    }
}

#[derive(Default)]
struct DivisorExprVisitor {
    findings: Vec<Finding>,
    current_function_name: String,
}

impl<'ast> Visit<'ast> for DivisorExprVisitor {
    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        if matches!(node.op, BinOp::Div(_) | BinOp::DivAssign(_)) {
            if !is_literal(&node.right) {
                let description = "Divisor is not validated to be non-zero; division by zero will panic"
                    .to_string();
                self.findings.push(Finding {
                    check_name: CHECK_NAME.to_string(),
                    severity: Severity::High,
                    file_path: String::new(),
                    line: node.span().start().line,
                    function_name: self.current_function_name.clone(),
                    description,
                    rule_url: None,
                    suggestion: Some(
                        "Use checked_div or validate divisor > 0 before division".to_string(),
                    ),
                });
            }
        }
        visit::visit_expr_binary(self, node);
    }
}

fn is_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Lit(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_file;

    #[test]
    fn flags_non_literal_divisor() -> Result<(), syn::Error> {
        let src = r#"
#[contractimpl]
impl C {
    pub fn divide(a: u128, b: u128) -> u128 {
        a / b
    }
}
        "#;
        let file = parse_file(src)?;
        let check = UncheckedDivisorCheck;
        let findings = check.run(&file, src);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].check_name, "unchecked-divisor");
        assert!(findings[0]
            .description
            .contains("not validated to be non-zero"));
        Ok(())
    }

    #[test]
    fn flags_divide_assign() -> Result<(), syn::Error> {
        let src = r#"
#[contractimpl]
impl C {
    pub fn divide_assign(mut a: u128, b: u128) {
        a /= b;
    }
}
        "#;
        let file = parse_file(src)?;
        let check = UncheckedDivisorCheck;
        let findings = check.run(&file, src);
        assert_eq!(findings.len(), 1);
        Ok(())
    }

    #[test]
    fn ignores_literal_divisor() -> Result<(), syn::Error> {
        let src = r#"
#[contractimpl]
impl C {
    pub fn divide(a: u128) -> u128 {
        a / 2
    }
}
        "#;
        let file = parse_file(src)?;
        let check = UncheckedDivisorCheck;
        let findings = check.run(&file, src);
        assert_eq!(findings.len(), 0);
        Ok(())
    }
}
