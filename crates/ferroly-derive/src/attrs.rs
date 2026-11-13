//! Helper-attribute parsing for the derives.
//!
//! Every parser here is strict: an unknown key, a repeated key, or a value of
//! the wrong kind is a spanned `syn::Error`, never silently ignored.

use proc_macro2::Span;
use syn::meta::ParseNestedMeta;
use syn::punctuated::Punctuated;
use syn::{Attribute, ExprPath, LitStr, Token, WherePredicate};

use crate::case::{RenameRule, RULES};

// ---- #[ferroly(...)] on a struct or enum ---------------------------------

/// Container-level `#[ferroly(...)]` options.
#[derive(Default)]
pub struct ContainerAttrs {
    pub rename_all: Option<RenameRule>,
    /// `tag = "..."` — internally tagged enum representation.
    pub tag: Option<LitStr>,
    /// `untagged` — untagged enum representation.
    pub untagged: Option<Span>,
    pub deny_unknown_fields: Option<Span>,
    /// Explicit where-predicates replacing the inferred `Encode` bounds.
    pub bound_encode: Option<Vec<WherePredicate>>,
    /// Explicit where-predicates replacing the inferred `Decode` bounds.
    pub bound_decode: Option<Vec<WherePredicate>>,
}

pub fn container_attrs(attrs: &[Attribute]) -> syn::Result<ContainerAttrs> {
    let mut c = ContainerAttrs::default();
    for a in ferroly_attrs(attrs) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident("rename_all") {
                set_once(&mut c.rename_all, rename_rule(&m)?, &m)
            } else if m.path.is_ident("tag") {
                set_once(&mut c.tag, m.value()?.parse()?, &m)
            } else if m.path.is_ident("untagged") {
                set_once(&mut c.untagged, flag(&m)?, &m)
            } else if m.path.is_ident("deny_unknown_fields") {
                set_once(&mut c.deny_unknown_fields, flag(&m)?, &m)
            } else if m.path.is_ident("bound") {
                if m.input.peek(Token![=]) {
                    // `bound = "..."` applies to both derives.
                    let preds = predicates(&m)?;
                    set_once(&mut c.bound_encode, preds.clone(), &m)?;
                    set_once(&mut c.bound_decode, preds, &m)
                } else {
                    // `bound(encode = "...", decode = "...")`.
                    m.parse_nested_meta(|inner| {
                        if inner.path.is_ident("encode") {
                            set_once(&mut c.bound_encode, predicates(&inner)?, &inner)
                        } else if inner.path.is_ident("decode") {
                            set_once(&mut c.bound_decode, predicates(&inner)?, &inner)
                        } else {
                            Err(inner.error("expected `encode = \"...\"` or `decode = \"...\"`"))
                        }
                    })
                }
            } else {
                Err(m.error(
                    "unknown ferroly container attribute; expected one of `rename_all`, `tag`, \
                     `untagged`, `deny_unknown_fields`, `bound`",
                ))
            }
        })?;
    }
    Ok(c)
}

// ---- #[ferroly(...)] on a field ------------------------------------------

/// How a missing (or skipped) field obtains its value.
#[derive(Clone)]
pub enum DefaultKind {
    /// `default` — `Default::default()`.
    Trait,
    /// `default = "path"` — call the function at `path`.
    Path(ExprPath),
}

/// Field-level `#[ferroly(...)]` options. Flags keep the span of the key that
/// set them so later validation can point at it.
#[derive(Default)]
pub struct FieldAttrs {
    pub rename: Option<LitStr>,
    pub aliases: Vec<LitStr>,
    pub skip: Option<Span>,
    pub skip_none: Option<Span>,
    pub default: Option<(DefaultKind, Span)>,
    pub flatten: Option<Span>,
}

pub fn field_attrs(attrs: &[Attribute]) -> syn::Result<FieldAttrs> {
    let mut f = FieldAttrs::default();
    for a in ferroly_attrs(attrs) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident("rename") {
                set_once(&mut f.rename, m.value()?.parse()?, &m)
            } else if m.path.is_ident("alias") {
                f.aliases.push(m.value()?.parse()?);
                Ok(())
            } else if m.path.is_ident("skip") {
                set_once(&mut f.skip, flag(&m)?, &m)
            } else if m.path.is_ident("skip_none") {
                set_once(&mut f.skip_none, flag(&m)?, &m)
            } else if m.path.is_ident("flatten") {
                set_once(&mut f.flatten, flag(&m)?, &m)
            } else if m.path.is_ident("default") {
                let span = path_span(&m);
                let kind = if m.input.peek(Token![=]) {
                    let lit: LitStr = m.value()?.parse()?;
                    DefaultKind::Path(lit.parse()?)
                } else {
                    DefaultKind::Trait
                };
                set_once(&mut f.default, (kind, span), &m)
            } else {
                Err(m.error(
                    "unknown ferroly field attribute; expected one of `rename`, `alias`, \
                     `default`, `skip`, `skip_none`, `flatten`",
                ))
            }
        })?;
    }
    Ok(f)
}

// ---- #[ferroly(...)] on an enum variant ----------------------------------

/// Variant-level `#[ferroly(...)]` options.
#[derive(Default)]
pub struct VariantAttrs {
    pub rename: Option<LitStr>,
    pub aliases: Vec<LitStr>,
    /// Case rule for the fields of this (struct) variant.
    pub rename_all: Option<RenameRule>,
}

pub fn variant_attrs(attrs: &[Attribute]) -> syn::Result<VariantAttrs> {
    let mut v = VariantAttrs::default();
    for a in ferroly_attrs(attrs) {
        a.parse_nested_meta(|m| {
            if m.path.is_ident("rename") {
                set_once(&mut v.rename, m.value()?.parse()?, &m)
            } else if m.path.is_ident("alias") {
                v.aliases.push(m.value()?.parse()?);
                Ok(())
            } else if m.path.is_ident("rename_all") {
                set_once(&mut v.rename_all, rename_rule(&m)?, &m)
            } else {
                Err(m.error(
                    "unknown ferroly variant attribute; expected one of `rename`, `alias`, \
                     `rename_all`",
                ))
            }
        })?;
    }
    Ok(v)
}

// ---- shared helpers ------------------------------------------------------

fn ferroly_attrs(attrs: &[Attribute]) -> impl Iterator<Item = &Attribute> {
    attrs.iter().filter(|a| a.path().is_ident("ferroly"))
}

fn path_span(m: &ParseNestedMeta) -> Span {
    m.path
        .get_ident()
        .map(syn::Ident::span)
        .unwrap_or_else(Span::call_site)
}

/// Stores a value, rejecting a second occurrence of the same key.
fn set_once<T>(slot: &mut Option<T>, value: T, m: &ParseNestedMeta) -> syn::Result<()> {
    if slot.is_some() {
        let key = m
            .path
            .get_ident()
            .map(ToString::to_string)
            .unwrap_or_default();
        return Err(syn::Error::new(
            path_span(m),
            format!("duplicate ferroly attribute `{key}`"),
        ));
    }
    *slot = Some(value);
    Ok(())
}

/// A bare flag such as `skip`: rejects `skip = ...` / `skip(...)`.
fn flag(m: &ParseNestedMeta) -> syn::Result<Span> {
    if m.input.peek(Token![=]) || m.input.peek(syn::token::Paren) {
        return Err(m.error("this ferroly attribute takes no value"));
    }
    Ok(path_span(m))
}

fn rename_rule(m: &ParseNestedMeta) -> syn::Result<RenameRule> {
    let lit: LitStr = m.value()?.parse()?;
    RenameRule::parse(&lit.value()).ok_or_else(|| {
        syn::Error::new(
            lit.span(),
            format!(
                "unknown rename_all rule {:?}; expected one of: {}",
                lit.value(),
                RULES.join(", ")
            ),
        )
    })
}

/// Parses `= "T: Bound, U: Other"` into where-predicates (empty string → none).
fn predicates(m: &ParseNestedMeta) -> syn::Result<Vec<WherePredicate>> {
    let lit: LitStr = m.value()?.parse()?;
    let parsed = lit.parse_with(Punctuated::<WherePredicate, Token![,]>::parse_terminated)?;
    Ok(parsed.into_iter().collect())
}

// ---- #[error], #[from], #[source] ----------------------------------------

/// The display strategy declared by `#[error(...)]`.
#[derive(Clone)]
pub enum ErrorDisplay {
    /// `#[error(transparent)]` — delegate `Display`/`source` to the sole field.
    Transparent,
    /// `#[error("...")]` — a format string.
    Format(LitStr),
}

/// Parses the `#[error(...)]` attribute, if present. A malformed or repeated
/// attribute is an error rather than being treated as absent.
pub fn error_display(attrs: &[Attribute]) -> syn::Result<Option<ErrorDisplay>> {
    let mut found = None;
    for a in attrs.iter().filter(|a| a.path().is_ident("error")) {
        if found.is_some() {
            return Err(syn::Error::new_spanned(a, "duplicate #[error(...)]"));
        }
        let spec = if let Ok(lit) = a.parse_args::<LitStr>() {
            ErrorDisplay::Format(lit)
        } else if a
            .parse_args::<syn::Ident>()
            .is_ok_and(|id| id == "transparent")
        {
            ErrorDisplay::Transparent
        } else {
            return Err(syn::Error::new_spanned(
                a,
                "expected #[error(\"format string\")] or #[error(transparent)]",
            ));
        };
        found = Some(spec);
    }
    Ok(found)
}

/// Finds a bare marker attribute (`#[from]` / `#[source]`), rejecting a
/// repeated one and any form that carries arguments.
pub fn marker<'a>(attrs: &'a [Attribute], name: &str) -> syn::Result<Option<&'a Attribute>> {
    let mut found = None;
    for a in attrs.iter().filter(|a| a.path().is_ident(name)) {
        if found.is_some() {
            return Err(syn::Error::new_spanned(a, format!("duplicate #[{name}]")));
        }
        if a.meta.require_path_only().is_err() {
            return Err(syn::Error::new_spanned(
                a,
                format!("#[{name}] takes no arguments"),
            ));
        }
        found = Some(a);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::ToTokens;
    use syn::{parse_quote, DeriveInput};

    fn container(input: DeriveInput) -> syn::Result<ContainerAttrs> {
        container_attrs(&input.attrs)
    }

    fn field(input: DeriveInput) -> syn::Result<FieldAttrs> {
        match &input.data {
            syn::Data::Struct(s) => field_attrs(&s.fields.iter().next().unwrap().attrs),
            _ => unreachable!(),
        }
    }

    fn variant(input: DeriveInput) -> syn::Result<VariantAttrs> {
        match &input.data {
            syn::Data::Enum(e) => variant_attrs(&e.variants[0].attrs),
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
    fn container_defaults_are_empty() {
        let c = container(parse_quote! { struct S; }).unwrap();
        assert!(c.rename_all.is_none() && c.tag.is_none() && c.untagged.is_none());
        assert!(c.deny_unknown_fields.is_none());
        assert!(c.bound_encode.is_none() && c.bound_decode.is_none());
    }

    #[test]
    fn container_parses_every_key_across_attributes() {
        let c = container(parse_quote! {
            #[ferroly(rename_all = "camelCase", deny_unknown_fields)]
            #[ferroly(tag = "type")]
            #[derive(Debug)]
            enum E { A }
        })
        .unwrap();
        assert_eq!(c.rename_all, Some(RenameRule::Camel));
        assert_eq!(c.tag.unwrap().value(), "type");
        assert!(c.deny_unknown_fields.is_some());

        let c = container(parse_quote! { #[ferroly(untagged)] enum E { A } }).unwrap();
        assert!(c.untagged.is_some());
    }

    #[test]
    fn container_bound_forms() {
        let c = container(parse_quote! {
            #[ferroly(bound = "T: Clone, U: Copy")]
            struct S<T, U>(T, U);
        })
        .unwrap();
        assert_eq!(c.bound_encode.as_ref().unwrap().len(), 2);
        assert_eq!(c.bound_decode.as_ref().unwrap().len(), 2);

        let c = container(parse_quote! {
            #[ferroly(bound(encode = "T: Clone", decode = ""))]
            struct S<T>(T);
        })
        .unwrap();
        let encode = c.bound_encode.unwrap();
        assert_eq!(encode[0].to_token_stream().to_string(), "T : Clone");
        assert!(c.bound_decode.unwrap().is_empty());

        let c = container(parse_quote! {
            #[ferroly(bound(encode = "T: Clone"))]
            struct S<T>(T);
        })
        .unwrap();
        assert!(c.bound_encode.is_some() && c.bound_decode.is_none());
    }

    #[test]
    fn container_rejects_bad_input() {
        let unknown = message(container(
            parse_quote! { #[ferroly(renam_all = "x")] struct S; },
        ));
        assert!(unknown.contains("unknown ferroly container attribute"));

        let rule = message(container(
            parse_quote! { #[ferroly(rename_all = "snakecase")] struct S; },
        ));
        assert!(rule.contains("unknown rename_all rule \"snakecase\""));
        assert!(rule.contains("SCREAMING_SNAKE_CASE"));

        let dup = message(container(
            parse_quote! { #[ferroly(tag = "a")] #[ferroly(tag = "b")] enum E { A } },
        ));
        assert_eq!(dup, "duplicate ferroly attribute `tag`");

        let dup_bound = message(container(parse_quote! {
            #[ferroly(bound = "T: Clone", bound(decode = "T: Copy"))]
            struct S<T>(T);
        }));
        assert_eq!(dup_bound, "duplicate ferroly attribute `decode`");

        let valued = message(container(
            parse_quote! { #[ferroly(untagged = true)] enum E { A } },
        ));
        assert_eq!(valued, "this ferroly attribute takes no value");

        assert!(container(parse_quote! { #[ferroly(rename_all = 1)] struct S; }).is_err());
        assert!(container(parse_quote! { #[ferroly(bound = "T Clone")] struct S<T>(T); }).is_err());
        let inner = message(container(
            parse_quote! { #[ferroly(bound(both = "T: Clone"))] struct S<T>(T); },
        ));
        assert!(inner.contains("expected `encode"));
    }

    #[test]
    fn field_parses_every_key() {
        let f = field(parse_quote! {
            struct S {
                #[ferroly(rename = "n", alias = "old", alias = "older", default)]
                #[ferroly(skip_none)]
                name: Option<String>,
            }
        })
        .unwrap();
        assert_eq!(f.rename.unwrap().value(), "n");
        let aliases: Vec<String> = f.aliases.iter().map(LitStr::value).collect();
        assert_eq!(aliases, ["old", "older"]);
        assert!(matches!(f.default, Some((DefaultKind::Trait, _))));
        assert!(f.skip_none.is_some() && f.skip.is_none() && f.flatten.is_none());

        let f = field(parse_quote! {
            struct S { #[ferroly(skip, default = "defaults::port")] port: u16 }
        })
        .unwrap();
        assert!(f.skip.is_some());
        match f.default {
            Some((DefaultKind::Path(p), _)) => {
                assert_eq!(p.to_token_stream().to_string(), "defaults :: port");
            }
            _ => panic!("expected a default path"),
        }

        let f = field(parse_quote! { struct S { #[ferroly(flatten)] extra: Extra } }).unwrap();
        assert!(f.flatten.is_some());
    }

    #[test]
    fn field_rejects_bad_input() {
        let unknown = message(field(
            parse_quote! { struct S { #[ferroly(skip_if_none)] a: u8 } },
        ));
        assert!(unknown.contains("unknown ferroly field attribute"));

        let dup = message(field(
            parse_quote! { struct S { #[ferroly(rename = "a", rename = "b")] a: u8 } },
        ));
        assert_eq!(dup, "duplicate ferroly attribute `rename`");

        let valued = message(field(
            parse_quote! { struct S { #[ferroly(skip = "yes")] a: u8 } },
        ));
        assert_eq!(valued, "this ferroly attribute takes no value");

        assert!(field(parse_quote! { struct S { #[ferroly(default = "1 +")] a: u8 } }).is_err());
        assert!(field(parse_quote! { struct S { #[ferroly(rename = 3)] a: u8 } }).is_err());
        // Container-only keys are not accepted on a field.
        assert!(field(parse_quote! { struct S { #[ferroly(tag = "t")] a: u8 } }).is_err());
    }

    #[test]
    fn variant_parses_and_rejects() {
        let v = variant(parse_quote! {
            enum E {
                #[ferroly(rename = "a", alias = "b", rename_all = "kebab-case")]
                A { some_field: u8 },
            }
        })
        .unwrap();
        assert_eq!(v.rename.unwrap().value(), "a");
        assert_eq!(v.aliases.len(), 1);
        assert_eq!(v.rename_all, Some(RenameRule::Kebab));

        let bad = message(variant(parse_quote! { enum E { #[ferroly(skip_none)] A } }));
        assert!(bad.contains("unknown ferroly variant attribute"));
    }

    #[test]
    fn error_display_forms() {
        let di: DeriveInput = parse_quote! { #[error("code {0}")] struct E(u8); };
        match error_display(&di.attrs).unwrap() {
            Some(ErrorDisplay::Format(lit)) => assert_eq!(lit.value(), "code {0}"),
            _ => panic!("expected a format string"),
        }
        let di: DeriveInput = parse_quote! { #[error(transparent)] struct E(u8); };
        assert!(matches!(
            error_display(&di.attrs).unwrap(),
            Some(ErrorDisplay::Transparent)
        ));
        let di: DeriveInput = parse_quote! { struct E(u8); };
        assert!(error_display(&di.attrs).unwrap().is_none());

        let di: DeriveInput = parse_quote! { #[error(opaque)] struct E(u8); };
        assert!(message(error_display(&di.attrs)).contains("expected #[error("));
        let di: DeriveInput = parse_quote! { #[error("{}", 1)] struct E(u8); };
        assert!(message(error_display(&di.attrs)).contains("expected #[error("));
        let di: DeriveInput = parse_quote! { #[error] struct E(u8); };
        assert!(error_display(&di.attrs).is_err());
        let di: DeriveInput = parse_quote! { #[error("a")] #[error("b")] struct E(u8); };
        assert_eq!(message(error_display(&di.attrs)), "duplicate #[error(...)]");
    }

    #[test]
    fn marker_attributes() {
        let di: DeriveInput = parse_quote! { #[from] #[other] struct E; };
        assert!(marker(&di.attrs, "from").unwrap().is_some());
        assert!(marker(&di.attrs, "source").unwrap().is_none());

        let di: DeriveInput = parse_quote! { #[from(std::io::Error)] struct E; };
        assert_eq!(
            message(marker(&di.attrs, "from")),
            "#[from] takes no arguments"
        );
        let di: DeriveInput = parse_quote! { #[source] #[source] struct E; };
        assert_eq!(message(marker(&di.attrs, "source")), "duplicate #[source]");
    }
}
