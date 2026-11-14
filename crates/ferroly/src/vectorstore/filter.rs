//! Metadata filters for [`Query`](super::Query) and friends.

use std::cmp::Ordering;

use ferroly::codec::Value;

/// A predicate over a document's metadata.
///
/// A filter names a metadata **field** and a condition on it; filters combine
/// with [`and`](Filter::and), [`or`](Filter::or) and `!` (or
/// [`negate`](Filter::negate)).
///
/// ```
/// use ferroly::codec::Value;
/// use ferroly::vectorstore::Filter;
///
/// let metadata = Value::Object(vec![
///     ("tenant".into(), "acme".into()),
///     ("year".into(), 2024.into()),
///     ("tags".into(), Value::Array(vec!["draft".into(), "legal".into()])),
/// ]);
///
/// let filter = Filter::eq("tenant", "acme")
///     .and(Filter::gte("year", 2020))
///     .and(!Filter::eq("tags", "archived"));
/// assert!(filter.matches(&metadata));
/// ```
///
/// # Semantics
///
/// * **Fields** are looked up in the metadata object by key. A dotted name
///   (`"source.lang"`) descends into nested objects; a key that literally
///   contains a dot is found too, and wins over the nested reading.
/// * **Numbers** compare by value across integer and float representations
///   (`1`, `1u64` and `1.0` are equal). Strings compare lexicographically,
///   booleans as `false < true`. Values of different kinds are never equal
///   and never ordered, so a comparison between them is simply `false`.
/// * **Arrays**: [`Eq`](Filter::Eq) and [`In`](Filter::In) also match when
///   the field is an array *containing* a matching element (`tags` contains
///   `"legal"`). The ordering comparisons apply to scalar fields only.
/// * **Missing fields** fail every condition except [`Ne`](Filter::Ne) (and a
///   negation): "not equal to x" is true of a document that has no such field.
///   Use [`Exists`](Filter::Exists) to require presence.
/// * Metadata that is not an object has no fields.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Filter {
    /// The field equals the value (or is an array containing it).
    Eq(String, Value),
    /// The negation of [`Eq`](Filter::Eq); true when the field is missing.
    Ne(String, Value),
    /// The field equals one of the values (or is an array sharing an element
    /// with them). An empty list matches nothing.
    In(String, Vec<Value>),
    /// The field is greater than the value.
    Gt(String, Value),
    /// The field is greater than or equal to the value.
    Gte(String, Value),
    /// The field is less than the value.
    Lt(String, Value),
    /// The field is less than or equal to the value.
    Lte(String, Value),
    /// The field is present (even if its value is null).
    Exists(String),
    /// Every sub-filter matches. An empty list matches everything.
    And(Vec<Filter>),
    /// At least one sub-filter matches. An empty list matches nothing.
    Or(Vec<Filter>),
    /// The sub-filter does not match.
    Not(Box<Filter>),
}

impl Filter {
    /// `field == value` (or `field` is an array containing `value`).
    pub fn eq(field: impl Into<String>, value: impl Into<Value>) -> Filter {
        Filter::Eq(field.into(), value.into())
    }

    /// `field != value`; also true when the field is missing.
    pub fn ne(field: impl Into<String>, value: impl Into<Value>) -> Filter {
        Filter::Ne(field.into(), value.into())
    }

    /// `field` is one of `values`.
    pub fn is_in<V: Into<Value>>(
        field: impl Into<String>,
        values: impl IntoIterator<Item = V>,
    ) -> Filter {
        Filter::In(field.into(), values.into_iter().map(Into::into).collect())
    }

    /// `field > value`.
    pub fn gt(field: impl Into<String>, value: impl Into<Value>) -> Filter {
        Filter::Gt(field.into(), value.into())
    }

    /// `field >= value`.
    pub fn gte(field: impl Into<String>, value: impl Into<Value>) -> Filter {
        Filter::Gte(field.into(), value.into())
    }

    /// `field < value`.
    pub fn lt(field: impl Into<String>, value: impl Into<Value>) -> Filter {
        Filter::Lt(field.into(), value.into())
    }

    /// `field <= value`.
    pub fn lte(field: impl Into<String>, value: impl Into<Value>) -> Filter {
        Filter::Lte(field.into(), value.into())
    }

    /// `low <= field <= high` — shorthand for a closed range.
    pub fn between(
        field: impl Into<String>,
        low: impl Into<Value>,
        high: impl Into<Value>,
    ) -> Filter {
        let field = field.into();
        Filter::And(vec![
            Filter::Gte(field.clone(), low.into()),
            Filter::Lte(field, high.into()),
        ])
    }

    /// The field is present.
    pub fn exists(field: impl Into<String>) -> Filter {
        Filter::Exists(field.into())
    }

    /// Every one of `filters` matches.
    pub fn all(filters: impl IntoIterator<Item = Filter>) -> Filter {
        Filter::And(filters.into_iter().collect())
    }

    /// At least one of `filters` matches.
    pub fn any(filters: impl IntoIterator<Item = Filter>) -> Filter {
        Filter::Or(filters.into_iter().collect())
    }

    /// Both this filter and `other` match.
    pub fn and(self, other: Filter) -> Filter {
        match self {
            Filter::And(mut filters) => {
                filters.push(other);
                Filter::And(filters)
            }
            first => Filter::And(vec![first, other]),
        }
    }

    /// This filter or `other` matches.
    pub fn or(self, other: Filter) -> Filter {
        match self {
            Filter::Or(mut filters) => {
                filters.push(other);
                Filter::Or(filters)
            }
            first => Filter::Or(vec![first, other]),
        }
    }

    /// This filter does not match (the same as `!filter`).
    pub fn negate(self) -> Filter {
        Filter::Not(Box::new(self))
    }

    /// Evaluates the filter against one document's metadata.
    ///
    /// Public so that other [`VectorStore`](super::VectorStore) backends can
    /// apply the same semantics to results they post-filter.
    pub fn matches(&self, metadata: &Value) -> bool {
        match self {
            Filter::Eq(field, value) => {
                lookup(metadata, field).is_some_and(|found| equals_or_contains(found, value))
            }
            Filter::Ne(field, value) => {
                !lookup(metadata, field).is_some_and(|found| equals_or_contains(found, value))
            }
            Filter::In(field, values) => lookup(metadata, field)
                .is_some_and(|found| values.iter().any(|v| equals_or_contains(found, v))),
            Filter::Gt(field, value) => ordered(metadata, field, value, |o| o == Ordering::Greater),
            Filter::Gte(field, value) => ordered(metadata, field, value, |o| o != Ordering::Less),
            Filter::Lt(field, value) => ordered(metadata, field, value, |o| o == Ordering::Less),
            Filter::Lte(field, value) => {
                ordered(metadata, field, value, |o| o != Ordering::Greater)
            }
            Filter::Exists(field) => lookup(metadata, field).is_some(),
            Filter::And(filters) => filters.iter().all(|f| f.matches(metadata)),
            Filter::Or(filters) => filters.iter().any(|f| f.matches(metadata)),
            Filter::Not(filter) => !filter.matches(metadata),
        }
    }
}

impl std::ops::Not for Filter {
    type Output = Filter;
    fn not(self) -> Filter {
        self.negate()
    }
}

/// Finds `path` in `metadata`: as a literal key first, then as a dotted path
/// through nested objects.
fn lookup<'a>(metadata: &'a Value, path: &str) -> Option<&'a Value> {
    if let Some(found) = metadata.get(path) {
        return Some(found);
    }
    path.match_indices('.').find_map(|(dot, _)| {
        let child = metadata.get(&path[..dot])?;
        lookup(child, &path[dot + 1..])
    })
}

/// Applies an ordering test to a scalar field; false when the field is
/// missing or the two values are not comparable.
fn ordered(metadata: &Value, field: &str, value: &Value, test: impl Fn(Ordering) -> bool) -> bool {
    lookup(metadata, field)
        .and_then(|found| compare(found, value))
        .is_some_and(test)
}

/// A number, kept exact where it is an integer.
enum Number {
    Int(i128),
    Float(f64),
}

fn number(value: &Value) -> Option<Number> {
    match value {
        Value::Int(i) => Some(Number::Int(i128::from(*i))),
        Value::UInt(u) => Some(Number::Int(i128::from(*u))),
        Value::Float(f) => Some(Number::Float(*f)),
        _ => None,
    }
}

/// Orders two values of the same kind; `None` if they are of different kinds
/// (or a float is NaN).
fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    if let (Some(x), Some(y)) = (number(a), number(b)) {
        return match (x, y) {
            // Integers compare exactly, beyond what an `f64` can represent.
            (Number::Int(x), Number::Int(y)) => Some(x.cmp(&y)),
            (Number::Int(x), Number::Float(y)) => (x as f64).partial_cmp(&y),
            (Number::Float(x), Number::Int(y)) => x.partial_cmp(&(y as f64)),
            (Number::Float(x), Number::Float(y)) => x.partial_cmp(&y),
        };
    }
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Equality that treats numbers by value and everything else structurally.
fn equals(a: &Value, b: &Value) -> bool {
    if number(a).is_some() && number(b).is_some() {
        return compare(a, b) == Some(Ordering::Equal);
    }
    a == b
}

/// `found == wanted`, or `found` is an array with an element equal to it.
fn equals_or_contains(found: &Value, wanted: &Value) -> bool {
    if equals(found, wanted) {
        return true;
    }
    match found {
        Value::Array(items) => items.iter().any(|item| equals(item, wanted)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(fields: Vec<(&str, Value)>) -> Value {
        Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    fn sample() -> Value {
        object(vec![
            ("tenant", "acme".into()),
            ("year", 2024.into()),
            ("score", 0.75.into()),
            ("public", true.into()),
            ("tags", Value::Array(vec!["draft".into(), "legal".into()])),
            ("nothing", Value::Null),
            (
                "source",
                object(vec![("lang", "en".into()), ("pages", 12u64.into())]),
            ),
            ("dotted.key", "literal".into()),
        ])
    }

    #[test]
    fn equality_and_membership() {
        let m = sample();
        assert!(Filter::eq("tenant", "acme").matches(&m));
        assert!(!Filter::eq("tenant", "globex").matches(&m));
        assert!(Filter::eq("public", true).matches(&m));
        assert!(Filter::eq("nothing", Value::Null).matches(&m));
        assert!(!Filter::eq("missing", "x").matches(&m));
        // Different kinds are never equal.
        assert!(!Filter::eq("year", "2024").matches(&m));
        assert!(!Filter::eq("tenant", 1).matches(&m));

        assert!(Filter::ne("tenant", "globex").matches(&m));
        assert!(!Filter::ne("tenant", "acme").matches(&m));
        assert!(Filter::ne("missing", "x").matches(&m), "missing is != x");

        assert!(Filter::is_in("tenant", ["globex", "acme"]).matches(&m));
        assert!(!Filter::is_in("tenant", ["globex", "initech"]).matches(&m));
        assert!(!Filter::is_in("tenant", Vec::<&str>::new()).matches(&m));
        assert!(Filter::is_in("year", [2023, 2024]).matches(&m));
        assert!(!Filter::is_in("missing", ["x"]).matches(&m));
    }

    #[test]
    fn arrays_match_by_containment() {
        let m = sample();
        assert!(Filter::eq("tags", "legal").matches(&m));
        assert!(!Filter::eq("tags", "archived").matches(&m));
        assert!(Filter::ne("tags", "archived").matches(&m));
        assert!(Filter::is_in("tags", ["archived", "draft"]).matches(&m));
        // The whole array still equals itself.
        assert!(Filter::eq("tags", Value::Array(vec!["draft".into(), "legal".into()])).matches(&m));
    }

    #[test]
    fn numbers_compare_by_value_across_representations() {
        let m = sample();
        assert!(Filter::eq("year", 2024u64).matches(&m));
        assert!(Filter::eq("year", 2024.0).matches(&m));
        assert!(Filter::eq("source.pages", 12).matches(&m));
        assert!(Filter::gt("year", 2023.5).matches(&m));
        assert!(Filter::gte("year", 2024).matches(&m));
        assert!(!Filter::gt("year", 2024).matches(&m));
        assert!(Filter::lt("score", 1).matches(&m));
        assert!(Filter::lte("score", 0.75).matches(&m));
        assert!(!Filter::lt("score", 0.75).matches(&m));
        assert!(Filter::between("year", 2020, 2024).matches(&m));
        assert!(!Filter::between("year", 2025, 2030).matches(&m));

        // Large integers keep their exact order (they would collide as f64).
        let big = object(vec![("n", Value::UInt(u64::MAX))]);
        assert!(Filter::gt("n", Value::UInt(u64::MAX - 1)).matches(&big));
        assert!(Filter::gt("n", i64::MAX).matches(&big));
        assert!(!Filter::eq("n", Value::UInt(u64::MAX - 1)).matches(&big));

        // NaN is not ordered and not equal to anything.
        let nan = object(vec![("x", Value::Float(f64::NAN))]);
        assert!(!Filter::gt("x", 0).matches(&nan));
        assert!(!Filter::lte("x", 0).matches(&nan));
        assert!(!Filter::eq("x", Value::Float(f64::NAN)).matches(&nan));
    }

    #[test]
    fn ordering_needs_comparable_scalars() {
        let m = sample();
        assert!(Filter::gt("tenant", "aaa").matches(&m));
        assert!(Filter::lt("tenant", "b").matches(&m));
        assert!(Filter::gt("public", false).matches(&m));
        // Mixed kinds, arrays, nulls and missing fields: always false.
        assert!(!Filter::gt("tenant", 1).matches(&m));
        assert!(!Filter::lt("tenant", 1).matches(&m));
        assert!(!Filter::gt("tags", "a").matches(&m));
        assert!(!Filter::gte("nothing", 0).matches(&m));
        assert!(!Filter::lt("missing", 10).matches(&m));
        assert!(!Filter::gte("missing", 10).matches(&m));
    }

    #[test]
    fn exists_and_field_paths() {
        let m = sample();
        assert!(Filter::exists("tenant").matches(&m));
        assert!(Filter::exists("nothing").matches(&m), "null is present");
        assert!(!Filter::exists("missing").matches(&m));
        assert!(Filter::exists("source.lang").matches(&m));
        assert!(Filter::eq("source.lang", "en").matches(&m));
        assert!(!Filter::exists("source.missing").matches(&m));
        assert!(!Filter::exists("tenant.inner").matches(&m));
        // A key that itself contains a dot.
        assert!(Filter::eq("dotted.key", "literal").matches(&m));

        // Metadata that is not an object has no fields.
        assert!(!Filter::exists("x").matches(&Value::Null));
        assert!(!Filter::eq("x", 1).matches(&Value::Str("x".into())));
        assert!(Filter::ne("x", 1).matches(&Value::Null));
    }

    #[test]
    fn boolean_combinators() {
        let m = sample();
        let yes = Filter::eq("tenant", "acme");
        let no = Filter::eq("tenant", "globex");

        assert!(yes.clone().and(Filter::gte("year", 2020)).matches(&m));
        assert!(!yes.clone().and(no.clone()).matches(&m));
        assert!(yes.clone().or(no.clone()).matches(&m));
        assert!(!no.clone().or(Filter::lt("year", 2000)).matches(&m));
        assert!((!no.clone()).matches(&m));
        assert!(!(!yes.clone()).matches(&m));
        assert!(no.clone().negate().matches(&m));

        // Chaining flattens rather than nesting.
        let chained = yes.clone().and(no.clone()).and(Filter::exists("x"));
        assert!(matches!(&chained, Filter::And(parts) if parts.len() == 3));
        let chained = yes.clone().or(no.clone()).or(Filter::exists("x"));
        assert!(matches!(&chained, Filter::Or(parts) if parts.len() == 3));

        // Empty combinators.
        assert!(Filter::all([]).matches(&m));
        assert!(!Filter::any([]).matches(&m));
        assert!(Filter::all([yes.clone(), Filter::exists("tags")]).matches(&m));
        assert!(Filter::any([no.clone(), yes.clone()]).matches(&m));

        // Nested: (acme AND (year < 2000 OR NOT archived)).
        let nested = yes.and(Filter::lt("year", 2000).or(!Filter::eq("tags", "archived")));
        assert!(nested.matches(&m));
    }
}
