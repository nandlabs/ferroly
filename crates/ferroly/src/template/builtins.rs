//! The names the engine knows: filters, tests and global functions. Names are
//! resolved at parse time, so an unknown one is a parse error.

/// A built-in filter (`value | name(args)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filter {
    Abs,
    Attr,
    Capitalize,
    Center,
    Default,
    Dictsort,
    Escape,
    First,
    Float,
    Indent,
    Int,
    Items,
    Join,
    Last,
    Length,
    List,
    Lower,
    Map,
    Max,
    Min,
    Reject,
    Rejectattr,
    Replace,
    Reverse,
    Round,
    Safe,
    Select,
    Selectattr,
    Sort,
    String,
    Sum,
    Title,
    Tojson,
    Trim,
    Unique,
    Upper,
    Wordcount,
}

impl Filter {
    pub(crate) fn from_name(name: &str) -> Option<Filter> {
        Some(match name {
            "abs" => Filter::Abs,
            "attr" => Filter::Attr,
            "capitalize" => Filter::Capitalize,
            "center" => Filter::Center,
            "default" | "d" => Filter::Default,
            "dictsort" => Filter::Dictsort,
            "escape" | "e" => Filter::Escape,
            "first" => Filter::First,
            "float" => Filter::Float,
            "indent" => Filter::Indent,
            "int" => Filter::Int,
            "items" => Filter::Items,
            "join" => Filter::Join,
            "last" => Filter::Last,
            "length" | "count" => Filter::Length,
            "list" => Filter::List,
            "lower" => Filter::Lower,
            "map" => Filter::Map,
            "max" => Filter::Max,
            "min" => Filter::Min,
            "reject" => Filter::Reject,
            "rejectattr" => Filter::Rejectattr,
            "replace" => Filter::Replace,
            "reverse" => Filter::Reverse,
            "round" => Filter::Round,
            "safe" => Filter::Safe,
            "select" => Filter::Select,
            "selectattr" => Filter::Selectattr,
            "sort" => Filter::Sort,
            "string" => Filter::String,
            "sum" => Filter::Sum,
            "title" => Filter::Title,
            "tojson" => Filter::Tojson,
            "trim" => Filter::Trim,
            "unique" => Filter::Unique,
            "upper" => Filter::Upper,
            "wordcount" => Filter::Wordcount,
            _ => return None,
        })
    }
}

/// A built-in test (`value is name(args)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Test {
    Boolean,
    Callable,
    Defined,
    Divisibleby,
    Eq,
    Escaped,
    Even,
    False,
    Float,
    Ge,
    Gt,
    In,
    Integer,
    Iterable,
    Le,
    Lower,
    Lt,
    Mapping,
    Ne,
    None,
    Number,
    Odd,
    Sameas,
    Sequence,
    String,
    True,
    Undefined,
    Upper,
}

impl Test {
    pub(crate) fn from_name(name: &str) -> Option<Test> {
        Some(match name {
            "boolean" => Test::Boolean,
            "callable" => Test::Callable,
            "defined" => Test::Defined,
            "divisibleby" => Test::Divisibleby,
            "eq" | "equalto" | "==" => Test::Eq,
            "escaped" => Test::Escaped,
            "even" => Test::Even,
            "false" => Test::False,
            "float" => Test::Float,
            "ge" | ">=" => Test::Ge,
            "gt" | "greaterthan" | ">" => Test::Gt,
            "in" => Test::In,
            "integer" => Test::Integer,
            "iterable" => Test::Iterable,
            "le" | "<=" => Test::Le,
            "lower" => Test::Lower,
            "lt" | "lessthan" | "<" => Test::Lt,
            "mapping" => Test::Mapping,
            "ne" | "!=" => Test::Ne,
            "none" => Test::None,
            "number" => Test::Number,
            "odd" => Test::Odd,
            "sameas" => Test::Sameas,
            "sequence" => Test::Sequence,
            "string" => Test::String,
            "true" => Test::True,
            "undefined" => Test::Undefined,
            "upper" => Test::Upper,
            _ => return None,
        })
    }

    /// Whether the test takes one argument, which may then be written
    /// without parentheses (`x is divisibleby 3`).
    pub(crate) fn takes_arg(self) -> bool {
        matches!(
            self,
            Test::Divisibleby
                | Test::Eq
                | Test::Ge
                | Test::Gt
                | Test::In
                | Test::Le
                | Test::Lt
                | Test::Ne
                | Test::Sameas
        )
    }
}

/// The global functions every template sees (besides host functions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Global {
    Range,
    Namespace,
    Dict,
    RaiseException,
}

impl Global {
    pub(crate) fn from_name(name: &str, raise_exception: bool) -> Option<Global> {
        Some(match name {
            "range" => Global::Range,
            "namespace" => Global::Namespace,
            "dict" => Global::Dict,
            "raise_exception" if raise_exception => Global::RaiseException,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Global::Range => "range",
            Global::Namespace => "namespace",
            Global::Dict => "dict",
            Global::RaiseException => "raise_exception",
        }
    }
}

/// Methods callable on strings (`s.strip()`).
pub(crate) const STR_METHODS: &[&str] = &[
    "capitalize",
    "count",
    "endswith",
    "find",
    "isalnum",
    "isalpha",
    "isdigit",
    "islower",
    "isspace",
    "isupper",
    "join",
    "lower",
    "lstrip",
    "replace",
    "rsplit",
    "rstrip",
    "split",
    "splitlines",
    "startswith",
    "strip",
    "title",
    "upper",
];

/// Methods callable on maps (`m.items()`).
pub(crate) const MAP_METHODS: &[&str] = &["get", "items", "keys", "values"];

/// Methods callable on lists (`xs.index(x)`); the mutating ones are left out,
/// as in Jinja2's immutable sandbox.
pub(crate) const LIST_METHODS: &[&str] = &["count", "index"];
