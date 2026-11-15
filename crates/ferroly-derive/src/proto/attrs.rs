//! `#[proto(...)]` attribute parsing for the protobuf derives.
//!
//! As with `#[ferroly(...)]`, every parser is strict: an unknown key, a
//! repeated key, or a value of the wrong kind is a spanned `syn::Error`.

use proc_macro2::Span;
use syn::meta::ParseNestedMeta;
use syn::{Attribute, LitBool, LitInt, LitStr, Token};

/// A protobuf scalar or composite kind, as named in attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Int32,
    Int64,
    UInt32,
    UInt64,
    SInt32,
    SInt64,
    Fixed32,
    Fixed64,
    SFixed32,
    SFixed64,
    Float,
    Double,
    Bool,
    Str,
    Bytes,
    Msg,
    Enum,
}

/// Every attribute spelling, in the order error messages list them.
pub const KIND_NAMES: &[(&str, Kind)] = &[
    ("int32", Kind::Int32),
    ("int64", Kind::Int64),
    ("uint32", Kind::UInt32),
    ("uint64", Kind::UInt64),
    ("sint32", Kind::SInt32),
    ("sint64", Kind::SInt64),
    ("fixed32", Kind::Fixed32),
    ("fixed64", Kind::Fixed64),
    ("sfixed32", Kind::SFixed32),
    ("sfixed64", Kind::SFixed64),
    ("float", Kind::Float),
    ("double", Kind::Double),
    ("bool", Kind::Bool),
    ("string", Kind::Str),
    ("bytes", Kind::Bytes),
    ("message", Kind::Msg),
    ("enumeration", Kind::Enum),
];

impl Kind {
    pub fn from_name(name: &str) -> Option<Kind> {
        if name == "enum" {
            return Some(Kind::Enum);
        }
        KIND_NAMES.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
    }

    /// The marker type's name in `ferroly::proto::__private`.
    pub fn marker(self) -> &'static str {
        match self {
            Kind::Int32 => "Int32",
            Kind::Int64 => "Int64",
            Kind::UInt32 => "UInt32",
            Kind::UInt64 => "UInt64",
            Kind::SInt32 => "SInt32",
            Kind::SInt64 => "SInt64",
            Kind::Fixed32 => "Fixed32",
            Kind::Fixed64 => "Fixed64",
            Kind::SFixed32 => "SFixed32",
            Kind::SFixed64 => "SFixed64",
            Kind::Float => "Float",
            Kind::Double => "Double",
            Kind::Bool => "Bool",
            Kind::Str => "Str",
            Kind::Bytes => "Bytes",
            Kind::Msg => "Msg",
            Kind::Enum => "Enum",
        }
    }

    /// Whether a repeated field of this kind can be packed.
    pub fn packable(self) -> bool {
        !matches!(self, Kind::Str | Kind::Bytes | Kind::Msg)
    }

    /// Whether the kind may be a map key (integral, bool or string).
    pub fn map_key(self) -> bool {
        !matches!(
            self,
            Kind::Float | Kind::Double | Kind::Bytes | Kind::Msg | Kind::Enum
        )
    }
}

fn kind_list() -> String {
    KIND_NAMES
        .iter()
        .map(|(n, _)| format!("`{n}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Field-level `#[proto(...)]` options (also used for oneof variants).
#[derive(Default)]
pub struct FieldAttrs {
    pub tag: Option<(u32, Span)>,
    pub kind: Option<(Kind, Span)>,
    pub packed: Option<(bool, Span)>,
    pub map: Option<(Option<Kind>, Option<Kind>, Span)>,
    pub oneof: Option<Span>,
    pub unknown: Option<Span>,
    pub required: Option<Span>,
    pub skip: Option<Span>,
}

impl FieldAttrs {
    /// Every key other than `tag` and the type that is set, with its span.
    fn extras(&self) -> Vec<(Span, &'static str)> {
        let mut out = Vec::new();
        if let Some((_, s)) = self.packed {
            out.push((s, "packed"));
        }
        if let Some((_, _, s)) = self.map {
            out.push((s, "map"));
        }
        for (slot, key) in [
            (self.oneof, "oneof"),
            (self.unknown, "unknown"),
            (self.required, "required"),
            (self.skip, "skip"),
        ] {
            if let Some(s) = slot {
                out.push((s, key));
            }
        }
        out
    }

    /// The first key other than `tag`, the type and `except`, for rejecting
    /// it where it is not allowed.
    pub fn extra_except(&self, except: &str) -> Option<(Span, &'static str)> {
        self.extras().into_iter().find(|(_, k)| *k != except)
    }
}

pub fn field_attrs(attrs: &[Attribute]) -> syn::Result<FieldAttrs> {
    let mut f = FieldAttrs::default();
    for a in proto_attrs(attrs) {
        a.parse_nested_meta(|m| {
            let span = path_span(&m);
            if m.path.is_ident("tag") {
                let lit: LitInt = m.value()?.parse()?;
                let tag = lit.base10_parse::<u64>().map_err(|_| {
                    syn::Error::new(lit.span(), "expected an unsigned field number")
                })?;
                validate_tag(tag, lit.span())?;
                set_once(&mut f.tag, (tag as u32, lit.span()), &m)
            } else if m.path.is_ident("packed") {
                let value = if m.input.peek(Token![=]) {
                    let lit: LitBool = m.value()?.parse()?;
                    lit.value
                } else {
                    true
                };
                set_once(&mut f.packed, (value, span), &m)
            } else if m.path.is_ident("map") {
                let mut key = None;
                let mut value = None;
                if m.input.peek(syn::token::Paren) {
                    m.parse_nested_meta(|inner| {
                        let slot = if inner.path.is_ident("key") {
                            &mut key
                        } else if inner.path.is_ident("value") {
                            &mut value
                        } else {
                            return Err(
                                inner.error("expected `key = \"...\"` or `value = \"...\"`")
                            );
                        };
                        let lit: LitStr = inner.value()?.parse()?;
                        let kind = Kind::from_name(&lit.value()).ok_or_else(|| {
                            syn::Error::new(
                                lit.span(),
                                format!(
                                    "unknown protobuf type {:?}; expected one of {}",
                                    lit.value(),
                                    kind_list()
                                ),
                            )
                        })?;
                        set_once(slot, kind, &inner)
                    })?;
                } else if m.input.peek(Token![=]) {
                    return Err(
                        m.error("expected `map(key = \"...\", value = \"...\")` or bare `map`")
                    );
                }
                set_once(&mut f.map, (key, value, span), &m)
            } else if m.path.is_ident("oneof") {
                set_once(&mut f.oneof, flag(&m)?, &m)
            } else if m.path.is_ident("unknown") {
                set_once(&mut f.unknown, flag(&m)?, &m)
            } else if m.path.is_ident("required") {
                set_once(&mut f.required, flag(&m)?, &m)
            } else if m.path.is_ident("skip") {
                set_once(&mut f.skip, flag(&m)?, &m)
            } else if let Some(kind) = m
                .path
                .get_ident()
                .and_then(|i| Kind::from_name(&i.to_string()))
            {
                flag(&m)?;
                if let Some((_, _)) = f.kind {
                    return Err(syn::Error::new(
                        span,
                        "a field has one protobuf type; this one is already set",
                    ));
                }
                f.kind = Some((kind, span));
                Ok(())
            } else {
                Err(m.error(format!(
                    "unknown proto field attribute; expected `tag`, `packed`, `map`, `oneof`, \
                     `unknown`, `required`, `skip` or a protobuf type ({})",
                    kind_list()
                )))
            }
        })?;
    }
    Ok(f)
}

/// Rejects field number 0, numbers above 2^29 - 1, and the reserved range.
pub fn validate_tag(tag: u64, span: Span) -> syn::Result<()> {
    if tag == 0 {
        return Err(syn::Error::new(
            span,
            "field number 0 is not allowed; field numbers start at 1",
        ));
    }
    if tag > (1 << 29) - 1 {
        return Err(syn::Error::new(
            span,
            format!("field number {tag} is above the maximum 536870911"),
        ));
    }
    if (19_000..=19_999).contains(&tag) {
        return Err(syn::Error::new(
            span,
            format!("field number {tag} is in the range 19000-19999 reserved for the protobuf implementation"),
        ));
    }
    Ok(())
}

/// Variant-level `#[proto(...)]` options of an `Enumeration`.
#[derive(Default)]
pub struct EnumVariantAttrs {
    pub name: Option<LitStr>,
}

pub fn enum_variant_attrs(attrs: &[Attribute]) -> syn::Result<EnumVariantAttrs> {
    let mut v = EnumVariantAttrs::default();
    for a in proto_attrs(attrs) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident("name") {
                set_once(&mut v.name, m.value()?.parse()?, &m)
            } else {
                Err(m.error("unknown proto enum value attribute; expected `name = \"...\"`"))
            }
        })?;
    }
    Ok(v)
}

/// Rejects any `#[proto(...)]` on the type itself (no container options exist).
pub fn no_container_attrs(attrs: &[Attribute], derive: &str) -> syn::Result<()> {
    match proto_attrs(attrs).next() {
        Some(a) => Err(syn::Error::new_spanned(
            a,
            format!("#[derive({derive})] takes no #[proto(...)] attribute on the type"),
        )),
        None => Ok(()),
    }
}

fn proto_attrs(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs.iter().filter(|a| a.path().is_ident("proto"))
}

fn path_span(m: &ParseNestedMeta) -> Span {
    m.path
        .get_ident()
        .map(syn::Ident::span)
        .unwrap_or_else(Span::call_site)
}

fn set_once<T>(slot: &mut Option<T>, value: T, m: &ParseNestedMeta) -> syn::Result<()> {
    if slot.is_some() {
        let key = m
            .path
            .get_ident()
            .map(ToString::to_string)
            .unwrap_or_default();
        return Err(syn::Error::new(
            path_span(m),
            format!("duplicate proto attribute `{key}`"),
        ));
    }
    *slot = Some(value);
    Ok(())
}

fn flag(m: &ParseNestedMeta) -> syn::Result<Span> {
    if m.input.peek(Token![=]) || m.input.peek(syn::token::Paren) {
        return Err(m.error("this proto attribute takes no value"));
    }
    Ok(path_span(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::{parse_quote, DeriveInput};

    fn field(input: DeriveInput) -> syn::Result<FieldAttrs> {
        match &input.data {
            syn::Data::Struct(s) => field_attrs(&s.fields.iter().next().unwrap().attrs),
            _ => unreachable!(),
        }
    }

    fn message<T>(r: syn::Result<T>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn parses_every_key() {
        let f = field(parse_quote! {
            struct S { #[proto(tag = 3, sint64, packed = false)] a: Vec<i64> }
        })
        .unwrap();
        assert_eq!(f.tag.unwrap().0, 3);
        assert_eq!(f.kind.unwrap().0, Kind::SInt64);
        assert!(!f.packed.unwrap().0);

        let f = field(parse_quote! {
            struct S { #[proto(map(key = "string", value = "enum"), tag = 4)] m: M }
        })
        .unwrap();
        let (k, v, _) = f.map.unwrap();
        assert_eq!((k, v), (Some(Kind::Str), Some(Kind::Enum)));

        let f = field(parse_quote! { struct S { #[proto(tag = 1, packed)] a: Vec<u32> } }).unwrap();
        assert!(f.packed.unwrap().0);
        let f = field(parse_quote! { struct S { #[proto(oneof)] a: Option<O> } }).unwrap();
        assert!(f.oneof.is_some());
        let f = field(parse_quote! { struct S { #[proto(map)] #[proto(tag = 9)] a: M } }).unwrap();
        assert_eq!(f.map.unwrap().0, None);
        assert_eq!(f.tag.unwrap().0, 9);
    }

    #[test]
    fn rejects_bad_tags() {
        let zero = message(field(
            parse_quote! { struct S { #[proto(tag = 0)] a: i32 } },
        ));
        assert_eq!(
            zero,
            "field number 0 is not allowed; field numbers start at 1"
        );
        let reserved = message(field(
            parse_quote! { struct S { #[proto(tag = 19500)] a: i32 } },
        ));
        assert!(
            reserved.contains("reserved for the protobuf implementation"),
            "{reserved}"
        );
        let big = message(field(
            parse_quote! { struct S { #[proto(tag = 536870912)] a: i32 } },
        ));
        assert!(big.contains("above the maximum"), "{big}");
        let neg = message(field(
            parse_quote! { struct S { #[proto(tag = -1)] a: i32 } },
        ));
        assert!(!neg.is_empty());
        assert!(field(parse_quote! { struct S { #[proto(tag = 19999)] a: i32 } }).is_err());
        assert!(field(parse_quote! { struct S { #[proto(tag = 18999)] a: i32 } }).is_ok());
        assert!(field(parse_quote! { struct S { #[proto(tag = 20000)] a: i32 } }).is_ok());
    }

    #[test]
    fn rejects_bad_keys() {
        let dup = message(field(
            parse_quote! { struct S { #[proto(tag = 1, tag = 2)] a: i32 } },
        ));
        assert_eq!(dup, "duplicate proto attribute `tag`");
        let two = message(field(
            parse_quote! { struct S { #[proto(tag = 1, sint32, fixed32)] a: i32 } },
        ));
        assert_eq!(
            two,
            "a field has one protobuf type; this one is already set"
        );
        let unknown = message(field(
            parse_quote! { struct S { #[proto(tagg = 1)] a: i32 } },
        ));
        assert!(
            unknown.starts_with("unknown proto field attribute"),
            "{unknown}"
        );
        let valued = message(field(
            parse_quote! { struct S { #[proto(oneof = "X")] a: i32 } },
        ));
        assert_eq!(valued, "this proto attribute takes no value");
        let map = message(field(
            parse_quote! { struct S { #[proto(map(key = "map"))] a: M } },
        ));
        assert!(map.starts_with("unknown protobuf type \"map\""), "{map}");
        let inner = message(field(
            parse_quote! { struct S { #[proto(map(k = "int32"))] a: M } },
        ));
        assert!(inner.contains("expected `key"), "{inner}");
        let packed = message(field(
            parse_quote! { struct S { #[proto(packed = "no")] a: M } },
        ));
        assert!(!packed.is_empty());
    }

    #[test]
    fn enum_values_and_containers() {
        let di: DeriveInput = parse_quote! { enum E { #[proto(name = "E_A")] A } };
        match &di.data {
            syn::Data::Enum(e) => {
                let v = enum_variant_attrs(&e.variants[0].attrs).unwrap();
                assert_eq!(v.name.unwrap().value(), "E_A");
            }
            _ => unreachable!(),
        }
        let di: DeriveInput = parse_quote! { enum E { #[proto(tag = 1)] A } };
        match &di.data {
            syn::Data::Enum(e) => {
                let err = message(enum_variant_attrs(&e.variants[0].attrs));
                assert!(err.starts_with("unknown proto enum value attribute"));
            }
            _ => unreachable!(),
        }
        let di: DeriveInput = parse_quote! { #[proto(tag = 1)] struct S; };
        assert_eq!(
            message(no_container_attrs(&di.attrs, "Message")),
            "#[derive(Message)] takes no #[proto(...)] attribute on the type"
        );
    }
}
