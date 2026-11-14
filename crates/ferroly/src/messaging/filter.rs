//! Attribute filters for topic subscriptions.

use ferroly::messaging::{AttrValue, Message};

/// A condition on a message's properties and attributes, used to decide
/// whether a subscription receives a message.
///
/// A name is looked up with [`Message::lookup`]: a property under its
/// conventional name first, then the first application attribute.
///
/// - A missing name makes every leaf filter ([`Exists`](Filter::Exists),
///   [`Eq`](Filter::Eq), [`In`](Filter::In), [`Prefix`](Filter::Prefix) and
///   the four comparisons) false, so `not(exists("x"))` is true when `x` is
///   missing.
/// - Equality compares two numbers (`Int` or `Float`, in any combination) as
///   numbers; any other pair must be the same variant with the same value, so
///   `Str("5")` does not equal `Int(5)`.
/// - [`Prefix`](Filter::Prefix) matches only string values.
/// - The comparisons need a number; a non-number or a `NaN` on either side is
///   false.
///
/// ```
/// use ferroly::messaging::{Filter, Message};
///
/// let big_eu_orders = Filter::all([
///     Filter::eq("event-type", "order.created"),
///     Filter::eq("region", "eu"),
///     Filter::gte("amount", 100.0),
/// ]);
/// let msg = Message::text("{}")
///     .with_event_type("order.created")
///     .with_attribute("region", "eu")
///     .with_attribute("amount", 250);
/// assert!(big_eu_orders.matches(&msg));
/// assert!(!big_eu_orders.matches(&msg.clone().with_attribute("amount", 50)));
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Filter {
    /// The name is present.
    Exists(String),
    /// The value equals the given one.
    Eq(String, AttrValue),
    /// The value equals one of the given ones.
    In(String, Vec<AttrValue>),
    /// The value is a string that starts with the given prefix.
    Prefix(String, String),
    /// The value is a number greater than the given one.
    Gt(String, f64),
    /// The value is a number greater than or equal to the given one.
    Gte(String, f64),
    /// The value is a number less than the given one.
    Lt(String, f64),
    /// The value is a number less than or equal to the given one.
    Lte(String, f64),
    /// Every inner filter matches (an empty list matches everything).
    And(Vec<Filter>),
    /// At least one inner filter matches (an empty list matches nothing).
    Or(Vec<Filter>),
    /// The inner filter does not match.
    Not(Box<Filter>),
}

impl Filter {
    /// [`Filter::Exists`]: the name is present.
    pub fn exists(name: impl Into<String>) -> Filter {
        Filter::Exists(name.into())
    }

    /// [`Filter::Eq`]: the value equals `value`.
    pub fn eq(name: impl Into<String>, value: impl Into<AttrValue>) -> Filter {
        Filter::Eq(name.into(), value.into())
    }

    /// [`Filter::In`]: the value equals one of `values`.
    pub fn one_of(
        name: impl Into<String>,
        values: impl IntoIterator<Item = impl Into<AttrValue>>,
    ) -> Filter {
        Filter::In(name.into(), values.into_iter().map(Into::into).collect())
    }

    /// [`Filter::Prefix`]: the value is a string starting with `prefix`.
    pub fn prefix(name: impl Into<String>, prefix: impl Into<String>) -> Filter {
        Filter::Prefix(name.into(), prefix.into())
    }

    /// [`Filter::Gt`]: the value is a number greater than `n`.
    pub fn gt(name: impl Into<String>, n: f64) -> Filter {
        Filter::Gt(name.into(), n)
    }

    /// [`Filter::Gte`]: the value is a number greater than or equal to `n`.
    pub fn gte(name: impl Into<String>, n: f64) -> Filter {
        Filter::Gte(name.into(), n)
    }

    /// [`Filter::Lt`]: the value is a number less than `n`.
    pub fn lt(name: impl Into<String>, n: f64) -> Filter {
        Filter::Lt(name.into(), n)
    }

    /// [`Filter::Lte`]: the value is a number less than or equal to `n`.
    pub fn lte(name: impl Into<String>, n: f64) -> Filter {
        Filter::Lte(name.into(), n)
    }

    /// [`Filter::And`]: every filter matches.
    pub fn all(filters: impl IntoIterator<Item = Filter>) -> Filter {
        Filter::And(filters.into_iter().collect())
    }

    /// [`Filter::Or`]: at least one filter matches.
    pub fn any(filters: impl IntoIterator<Item = Filter>) -> Filter {
        Filter::Or(filters.into_iter().collect())
    }

    /// [`Filter::Not`]: `filter` does not match.
    #[allow(clippy::should_implement_trait)]
    pub fn not(filter: Filter) -> Filter {
        Filter::Not(Box::new(filter))
    }

    /// Whether `msg` satisfies this filter.
    pub fn matches(&self, msg: &Message) -> bool {
        match self {
            Filter::Exists(name) => msg.lookup(name).is_some(),
            Filter::Eq(name, expected) => msg
                .lookup(name)
                .is_some_and(|actual| values_equal(&actual, expected)),
            Filter::In(name, options) => msg
                .lookup(name)
                .is_some_and(|actual| options.iter().any(|option| values_equal(&actual, option))),
            Filter::Prefix(name, prefix) => msg.lookup(name).is_some_and(|actual| {
                actual
                    .as_str()
                    .is_some_and(|s| s.starts_with(prefix.as_str()))
            }),
            Filter::Gt(name, n) => compare(msg, name, *n, |a, b| a > b),
            Filter::Gte(name, n) => compare(msg, name, *n, |a, b| a >= b),
            Filter::Lt(name, n) => compare(msg, name, *n, |a, b| a < b),
            Filter::Lte(name, n) => compare(msg, name, *n, |a, b| a <= b),
            Filter::And(filters) => filters.iter().all(|f| f.matches(msg)),
            Filter::Or(filters) => filters.iter().any(|f| f.matches(msg)),
            Filter::Not(filter) => !filter.matches(msg),
        }
    }
}

fn is_number(value: &AttrValue) -> bool {
    matches!(value, AttrValue::Int(_) | AttrValue::Float(_))
}

/// Two numbers compare as numbers; anything else must be the same variant
/// with the same value.
fn values_equal(a: &AttrValue, b: &AttrValue) -> bool {
    if is_number(a) && is_number(b) {
        a.as_f64() == b.as_f64()
    } else {
        a == b
    }
}

/// Applies `op` to the named number and `n`; false for a missing name, a
/// non-number, or a `NaN` on either side.
fn compare(msg: &Message, name: &str, n: f64, op: impl Fn(f64, f64) -> bool) -> bool {
    match msg.lookup(name).and_then(|v| v.as_f64()) {
        Some(actual) if !actual.is_nan() && !n.is_nan() => op(actual, n),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg() -> Message {
        Message::text("x")
            .with_attribute("s", "invoice-17")
            .with_attribute("i", 5)
            .with_attribute("f", 2.5)
            .with_attribute("b", true)
            .with_attribute("nan", f64::NAN)
    }

    #[test]
    fn missing_names_do_not_match() {
        let m = msg();
        for filter in [
            Filter::exists("x"),
            Filter::eq("x", 1),
            Filter::one_of("x", [1, 2]),
            Filter::prefix("x", ""),
            Filter::gt("x", 0.0),
            Filter::gte("x", 0.0),
            Filter::lt("x", 0.0),
            Filter::lte("x", 0.0),
        ] {
            assert!(!filter.matches(&m), "{filter:?}");
        }
        assert!(Filter::not(Filter::exists("x")).matches(&m));
        assert!(Filter::exists("s").matches(&m));
    }

    #[test]
    fn numbers_compare_across_int_and_float() {
        let m = msg();
        assert!(Filter::eq("i", 5.0).matches(&m));
        assert!(Filter::eq("f", AttrValue::Float(2.5)).matches(&m));
        assert!(!Filter::eq("f", 2).matches(&m));
        assert!(Filter::one_of("i", [AttrValue::Float(1.0), AttrValue::Int(5)]).matches(&m));
        assert!(Filter::gt("i", 4.5).matches(&m));
        assert!(!Filter::gt("i", 5.0).matches(&m));
        assert!(Filter::gte("i", 5.0).matches(&m));
        assert!(Filter::lt("f", 3.0).matches(&m));
        assert!(Filter::lte("f", 2.5).matches(&m));
        // NaN on either side is false.
        assert!(!Filter::gt("nan", 0.0).matches(&m));
        assert!(!Filter::lte("nan", 0.0).matches(&m));
        assert!(!Filter::lt("i", f64::NAN).matches(&m));
        assert!(!Filter::gte("i", f64::NAN).matches(&m));
        // A comparison needs a number.
        assert!(!Filter::gt("s", 0.0).matches(&m));
        assert!(!Filter::gt("b", 0.0).matches(&m));
    }

    #[test]
    fn strings_do_not_equal_numbers() {
        let m = Message::text("x").with_attribute("n", "5");
        assert!(!Filter::eq("n", 5).matches(&m));
        assert!(Filter::eq("n", "5").matches(&m));
        let m = Message::text("x").with_attribute("n", 5);
        assert!(!Filter::eq("n", "5").matches(&m));
        assert!(!Filter::eq("n", true).matches(&m));
    }

    #[test]
    fn prefix_applies_to_strings_only() {
        let m = msg();
        assert!(Filter::prefix("s", "invoice-").matches(&m));
        assert!(!Filter::prefix("s", "order-").matches(&m));
        assert!(!Filter::prefix("i", "5").matches(&m));
        let bytes = Message::text("x").with_attribute("y", b"invoice".to_vec());
        assert!(!Filter::prefix("y", "inv").matches(&bytes));
    }

    #[test]
    fn and_or_not_compose() {
        let m = msg();
        let yes = Filter::eq("i", 5);
        let no = Filter::eq("i", 6);
        assert!(Filter::all([yes.clone(), Filter::exists("s")]).matches(&m));
        assert!(!Filter::all([yes.clone(), no.clone()]).matches(&m));
        assert!(Filter::any([no.clone(), yes.clone()]).matches(&m));
        assert!(!Filter::any([no.clone(), Filter::exists("zz")]).matches(&m));
        assert!(Filter::not(no.clone()).matches(&m));
        assert!(!Filter::not(yes.clone()).matches(&m));
        assert!(
            Filter::any([Filter::all([yes, Filter::not(no)]), Filter::exists("zz")]).matches(&m)
        );
    }

    #[test]
    fn empty_and_matches_empty_or_does_not() {
        let m = msg();
        assert!(Filter::all([]).matches(&m));
        assert!(!Filter::any([]).matches(&m));
    }

    #[test]
    fn properties_are_visible_under_their_wire_names() {
        let m = Message::text("x").with_subject("invoice");
        assert!(Filter::eq("subject", "invoice").matches(&m));
        assert!(Filter::exists("subject").matches(&m));
        let p = Message::text("x").with_priority(7);
        assert!(Filter::gte("priority", 5.0).matches(&p));
        // The property wins over an attribute with the same name.
        let both = Message::text("x")
            .with_subject("invoice")
            .with_attribute("subject", "other");
        assert!(Filter::eq("subject", "invoice").matches(&both));
    }
}
