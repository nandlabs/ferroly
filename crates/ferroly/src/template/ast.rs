//! The parsed form of a template: statements and expressions, each carrying
//! the byte offset it came from.

use std::collections::HashMap;
use std::sync::Arc;

use super::builtins::{Filter, Test};

#[derive(Debug, Clone)]
pub(crate) enum Const {
    None,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(Arc<str>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    NotIn,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Args {
    pub(crate) pos: Vec<Expr>,
    pub(crate) kw: Vec<(Arc<str>, Expr)>,
}

#[derive(Debug, Clone)]
pub(crate) struct Expr {
    pub(crate) kind: ExprKind,
    pub(crate) pos: usize,
    /// Height of this node in the tree (a leaf is 1). Checked against
    /// `Limits::max_depth` at parse time, so evaluation recursion is bounded.
    pub(crate) depth: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum ExprKind {
    Const(Const),
    Name(Arc<str>),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Attr(Box<Expr>, Arc<str>),
    Item(Box<Expr>, Box<Expr>),
    Slice {
        obj: Box<Expr>,
        start: Option<Box<Expr>>,
        stop: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },
    Call {
        callee: Box<Expr>,
        args: Args,
    },
    Filter {
        value: Box<Expr>,
        filter: Filter,
        args: Args,
    },
    Test {
        value: Box<Expr>,
        test: Test,
        negated: bool,
        args: Args,
    },
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Pos(Box<Expr>),
    /// `a and b and c`, short-circuiting left to right.
    And(Vec<Expr>),
    /// `a or b or c`, short-circuiting left to right.
    Or(Vec<Expr>),
    /// A left-associative run of same-precedence operators
    /// (`a + b - c`), kept flat so evaluating it is a loop, not recursion.
    /// Each step keeps its operator's offset for error messages.
    Chain(Box<Expr>, Vec<(BinOp, usize, Expr)>),
    Concat(Vec<Expr>),
    Compare(Box<Expr>, Vec<(CmpOp, Expr)>),
    Cond {
        cond: Box<Expr>,
        then: Box<Expr>,
        otherwise: Option<Box<Expr>>,
    },
}

impl ExprKind {
    /// The largest depth among the direct children (0 for a leaf).
    pub(crate) fn child_depth(&self) -> usize {
        fn args(a: &Args) -> usize {
            a.pos
                .iter()
                .map(|e| e.depth)
                .chain(a.kw.iter().map(|(_, e)| e.depth))
                .max()
                .unwrap_or(0)
        }
        let opt = |e: &Option<Box<Expr>>| e.as_ref().map_or(0, |e| e.depth);
        match self {
            ExprKind::Const(_) | ExprKind::Name(_) => 0,
            ExprKind::List(items)
            | ExprKind::Concat(items)
            | ExprKind::And(items)
            | ExprKind::Or(items) => items.iter().map(|e| e.depth).max().unwrap_or(0),
            ExprKind::Map(pairs) => pairs
                .iter()
                .map(|(k, v)| k.depth.max(v.depth))
                .max()
                .unwrap_or(0),
            ExprKind::Attr(e, _) | ExprKind::Not(e) | ExprKind::Neg(e) | ExprKind::Pos(e) => {
                e.depth
            }
            ExprKind::Item(a, b) => a.depth.max(b.depth),
            ExprKind::Chain(first, rest) => rest
                .iter()
                .map(|(_, _, e)| e.depth)
                .max()
                .unwrap_or(0)
                .max(first.depth),
            ExprKind::Slice {
                obj,
                start,
                stop,
                step,
            } => obj.depth.max(opt(start)).max(opt(stop)).max(opt(step)),
            ExprKind::Call { callee, args: a } => callee.depth.max(args(a)),
            ExprKind::Filter { value, args: a, .. } | ExprKind::Test { value, args: a, .. } => {
                value.depth.max(args(a))
            }
            ExprKind::Compare(first, rest) => rest
                .iter()
                .map(|(_, e)| e.depth)
                .max()
                .unwrap_or(0)
                .max(first.depth),
            ExprKind::Cond {
                cond,
                then,
                otherwise,
            } => cond.depth.max(then.depth).max(opt(otherwise)),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Target {
    Name(Arc<str>),
    Tuple(Vec<Arc<str>>),
    /// `ns.attr`, assignable only when `ns` is a namespace.
    Attr(Arc<str>, Arc<str>),
}

#[derive(Debug)]
pub(crate) struct ForLoop {
    pub(crate) target: Target,
    pub(crate) iter: Expr,
    pub(crate) filter: Option<Expr>,
    pub(crate) body: Vec<Stmt>,
    pub(crate) otherwise: Vec<Stmt>,
    pub(crate) pos: usize,
}

#[derive(Debug)]
pub(crate) struct MacroDef {
    pub(crate) name: Arc<str>,
    pub(crate) params: Vec<(Arc<str>, Option<Expr>)>,
    /// Parameter name → position, for macros with many parameters.
    pub(crate) param_index: Option<HashMap<Arc<str>, usize>>,
    pub(crate) body: Vec<Stmt>,
}

impl MacroDef {
    pub(crate) fn new(
        name: Arc<str>,
        params: Vec<(Arc<str>, Option<Expr>)>,
        body: Vec<Stmt>,
    ) -> MacroDef {
        let param_index = (params.len() > 8).then(|| {
            params
                .iter()
                .enumerate()
                .map(|(i, (p, _))| (p.clone(), i))
                .collect()
        });
        MacroDef {
            name,
            params,
            param_index,
            body,
        }
    }

    /// The position of the parameter called `name`.
    pub(crate) fn param_position(&self, name: &str) -> Option<usize> {
        match &self.param_index {
            Some(index) => index.get(name).copied(),
            None => self.params.iter().position(|(p, _)| &**p == name),
        }
    }
}

#[derive(Debug)]
pub(crate) enum Stmt {
    Text(String, usize),
    Output(Expr),
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        otherwise: Vec<Stmt>,
    },
    For(Box<ForLoop>),
    Set {
        target: Target,
        value: Expr,
        pos: usize,
    },
    SetBlock {
        name: Arc<str>,
        body: Vec<Stmt>,
    },
    Macro(Arc<MacroDef>),
    Break,
    Continue,
}
