//! Python source read with Ruff's parser, standing in for the `ast` module where atif-scan
//! reads untrusted Python (never run). Ranges are byte offsets into the source.

use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{Alias, Expr, ExprContext, ModModule, Parameter, Stmt};
use ruff_text_size::Ranged;

/// `ast.parse(source)`, or `None` when it isn't valid Python.
pub fn parse_module(source: &str) -> Option<ModModule> {
    ruff_python_parser::parse_module(source).ok().map(|parsed| parsed.into_syntax())
}

pub fn span(node: &impl Ranged) -> (usize, usize) {
    let range = node.range();
    (range.start().to_usize(), range.end().to_usize())
}

/// Every expression in `stmt` (itself excluded), outer first.
pub fn expressions(stmt: &Stmt) -> Vec<&Expr> {
    struct Collect<'a>(Vec<&'a Expr>);
    impl<'a> Visitor<'a> for Collect<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            self.0.push(expr);
            visitor::walk_expr(self, expr);
        }
    }
    let mut collect = Collect(Vec::new());
    collect.visit_stmt(stmt);
    collect.0
}

/// Roughly `len(list(ast.walk(tree)))`: statements, expressions, and the context and
/// operator nodes Python's AST has of its own.
pub fn node_count(module: &ModModule) -> usize {
    struct Count(usize);
    impl<'a> Visitor<'a> for Count {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            self.0 += 1;
            visitor::walk_stmt(self, stmt);
        }
        fn visit_expr(&mut self, expr: &'a Expr) {
            self.0 += 1 + usize::from(matches!(
                expr,
                Expr::Name(_) | Expr::Attribute(_) | Expr::Subscript(_) | Expr::Starred(_) | Expr::List(_) | Expr::Tuple(_) | Expr::BinOp(_) | Expr::BoolOp(_) | Expr::UnaryOp(_)
            ));
            visitor::walk_expr(self, expr);
        }
        fn visit_alias(&mut self, alias: &'a Alias) {
            self.0 += 1;
            visitor::walk_alias(self, alias);
        }
        fn visit_parameter(&mut self, parameter: &'a Parameter) {
            self.0 += 1;
            visitor::walk_parameter(self, parameter);
        }
    }
    let mut count = Count(1);
    count.visit_body(&module.body);
    count.0
}

/// Every name bound anywhere in the module (assignment targets, function, class and
/// parameter names, imports), with repeats.
pub fn bound_names(module: &ModModule) -> Vec<String> {
    struct Bound(Vec<String>);
    impl<'a> Visitor<'a> for Bound {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                Stmt::FunctionDef(f) => self.0.push(f.name.to_string()),
                Stmt::ClassDef(c) => self.0.push(c.name.to_string()),
                _ => {}
            }
            visitor::walk_stmt(self, stmt);
        }
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && name.ctx == ExprContext::Store
            {
                self.0.push(name.id.to_string());
            }
            visitor::walk_expr(self, expr);
        }
        fn visit_parameter(&mut self, parameter: &'a Parameter) {
            self.0.push(parameter.name.to_string());
            visitor::walk_parameter(self, parameter);
        }
        fn visit_alias(&mut self, alias: &'a Alias) {
            let name = match &alias.asname {
                Some(asname) => asname.to_string(),
                None => alias.name.split('.').next().unwrap_or("").to_string(),
            };
            self.0.push(name);
        }
    }
    let mut bound = Bound(Vec::new());
    bound.visit_body(&module.body);
    bound.0
}
