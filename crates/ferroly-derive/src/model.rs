//! The analysed form of a `#[derive(Encode)]` / `#[derive(Decode)]` input.
//!
//! Both derives parse the same model so the wire mapping (keys, aliases,
//! representation) cannot drift between the encode and decode sides. All shape
//! and attribute validation happens here, with spanned errors.

use std::collections::HashSet;

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::{format_ident, quote, ToTokens};
use syn::ext::IdentExt;
use syn::{
    Data, DeriveInput, Fields, GenericArgument, Generics, Ident, LitStr, Member, PathArguments,
    Type, WherePredicate,
};

use crate::attrs::{container_attrs, field_attrs, variant_attrs, DefaultKind};
use crate::case::{apply_rename_all, RenameRule};

/// The syntactic style of a set of fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Named,
    Tuple,
    Unit,
}

/// One field of a struct or of an enum variant.
pub struct Field {
    pub member: Member,
    pub ty: Type,
    /// Wire key (named fields only; unraw'd and renamed).
    pub key: String,
    /// Extra keys accepted when decoding.
    pub aliases: Vec<String>,
    pub skip: bool,
    pub skip_none: bool,
    pub default: Option<DefaultKind>,
    pub flatten: bool,
    /// Whether the type is spelled `Option<...>`.
    pub option: bool,
    /// Local the field is bound to when the value is destructured.
    pub bind: Ident,
    /// Local holding the decoded value until the struct is built.
    pub slot: Ident,
}

impl Field {
    /// Every key the field answers to when decoding.
    pub fn names(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.key).chain(self.aliases.iter())
    }

    /// The expression a skipped or missing field falls back to.
    pub fn default_expr(&self) -> TokenStream {
        match &self.default {
            Some(DefaultKind::Path(path)) => quote! { #path() },
            _ => quote! { ::core::default::Default::default() },
        }
    }

    /// Whether building the field needs `Default` of a type parameter it
    /// mentions (`Option<T>` is `Default` for every `T`).
    fn needs_default_impl(&self) -> bool {
        if self.option {
            return false;
        }
        match &self.default {
            Some(DefaultKind::Trait) => true,
            Some(DefaultKind::Path(_)) => false,
            None => self.skip,
        }
    }
}

/// The fields of a struct or variant.
pub struct FieldSet {
    pub style: Style,
    pub fields: Vec<Field>,
}

impl FieldSet {
    /// The fields that take part in the wire format (not `skip`).
    pub fn active(&self) -> impl Iterator<Item = &Field> {
        self.fields.iter().filter(|f| !f.skip)
    }

    /// A tuple shape with exactly one active field: transparent on the wire.
    pub fn newtype(&self) -> Option<&Field> {
        if self.style != Style::Tuple {
            return None;
        }
        let mut active = self.active();
        match (active.next(), active.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    }

    pub fn has_flatten(&self) -> bool {
        self.active().any(|f| f.flatten)
    }

    /// A pattern binding every active field of `path` to its `bind` local.
    pub fn pattern(&self, path: &TokenStream) -> TokenStream {
        match self.style {
            Style::Unit => quote! { #path },
            Style::Tuple => {
                let slots = self.fields.iter().map(|f| {
                    if f.skip {
                        quote! { _ }
                    } else {
                        f.bind.to_token_stream()
                    }
                });
                quote! { #path( #(#slots),* ) }
            }
            Style::Named => {
                let binds = self.active().map(|f| {
                    let (member, bind) = (&f.member, &f.bind);
                    quote! { #member: #bind }
                });
                quote! { #path { #(#binds,)* .. } }
            }
        }
    }

    /// Builds `path` from one expression per field, in declaration order.
    pub fn construct(
        &self,
        path: &TokenStream,
        mut value: impl FnMut(&Field) -> TokenStream,
    ) -> TokenStream {
        match self.style {
            Style::Unit => quote! { #path },
            Style::Tuple => {
                let values = self.fields.iter().map(&mut value);
                quote! { #path( #(#values),* ) }
            }
            Style::Named => {
                let inits = self.fields.iter().map(|f| {
                    let member = &f.member;
                    let v = value(f);
                    quote! { #member: #v }
                });
                quote! { #path { #(#inits),* } }
            }
        }
    }
}

/// One enum variant.
pub struct Variant {
    pub ident: Ident,
    pub key: String,
    pub aliases: Vec<String>,
    pub fields: FieldSet,
}

impl Variant {
    /// Every name the variant answers to when decoding.
    pub fn names(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.key).chain(self.aliases.iter())
    }
}

/// How an enum is laid out on the wire.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Repr {
    /// `"Unit"` / `{"Variant": content}` — the default.
    External,
    /// `{"<tag>": "Variant", ...fields}` — `#[ferroly(tag = "...")]`.
    Internal(String),
    /// Just the content — `#[ferroly(untagged)]`.
    Untagged,
}

pub enum Body {
    Struct(FieldSet),
    Enum(Vec<Variant>, Repr),
}

/// Which derive is being expanded (selects the bounds to add).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Derive {
    Encode,
    Decode,
}

pub struct Container {
    pub ident: Ident,
    pub generics: Generics,
    pub deny_unknown_fields: bool,
    pub body: Body,
    bound_encode: Option<Vec<WherePredicate>>,
    bound_decode: Option<Vec<WherePredicate>>,
}

impl Container {
    pub fn parse(input: &DeriveInput) -> syn::Result<Self> {
        let attrs = container_attrs(&input.attrs)?;
        let body = match &input.data {
            Data::Struct(s) => {
                if let Some(tag) = &attrs.tag {
                    return Err(syn::Error::new(tag.span(), "`tag` is only valid on enums"));
                }
                if let Some(span) = attrs.untagged {
                    return Err(syn::Error::new(span, "`untagged` is only valid on enums"));
                }
                let fields = field_set(&s.fields, attrs.rename_all)?;
                check_deny_flatten(&fields, attrs.deny_unknown_fields)?;
                Body::Struct(fields)
            }
            Data::Enum(e) => {
                let repr = match (&attrs.tag, attrs.untagged) {
                    (Some(_), Some(span)) => {
                        return Err(syn::Error::new(
                            span,
                            "`untagged` cannot be combined with `tag`",
                        ));
                    }
                    (Some(tag), None) => Repr::Internal(tag.value()),
                    (None, Some(_)) => Repr::Untagged,
                    (None, None) => Repr::External,
                };
                let mut variants = Vec::new();
                for v in &e.variants {
                    let va = variant_attrs(&v.attrs)?;
                    let key = match &va.rename {
                        Some(lit) => lit.value(),
                        None => apply_rename_all(&v.ident.unraw().to_string(), attrs.rename_all),
                    };
                    let fields = field_set(&v.fields, va.rename_all)?;
                    check_deny_flatten(&fields, attrs.deny_unknown_fields)?;
                    if let Repr::Internal(tag) = &repr {
                        check_internal_variant(v, &fields, tag)?;
                    }
                    variants.push((
                        Variant {
                            ident: v.ident.clone(),
                            key,
                            aliases: va.aliases.iter().map(LitStr::value).collect(),
                            fields,
                        },
                        va,
                    ));
                }
                if repr != Repr::Untagged {
                    let mut seen = Keys::default();
                    for (variant, va) in &variants {
                        let span = va
                            .rename
                            .as_ref()
                            .map_or(variant.ident.span(), LitStr::span);
                        seen.claim(&variant.key, span, "variant")?;
                        for alias in &va.aliases {
                            seen.claim(&alias.value(), alias.span(), "variant")?;
                        }
                    }
                }
                Body::Enum(variants.into_iter().map(|(v, _)| v).collect(), repr)
            }
            Data::Union(_) => {
                return Err(syn::Error::new_spanned(
                    &input.ident,
                    "Encode/Decode cannot be derived for unions",
                ));
            }
        };
        Ok(Container {
            ident: input.ident.clone(),
            generics: input.generics.clone(),
            deny_unknown_fields: attrs.deny_unknown_fields.is_some(),
            body,
            bound_encode: attrs.bound_encode,
            bound_decode: attrs.bound_decode,
        })
    }

    fn field_sets(&self) -> Vec<&FieldSet> {
        match &self.body {
            Body::Struct(fs) => vec![fs],
            Body::Enum(variants, _) => variants.iter().map(|v| &v.fields).collect(),
        }
    }

    /// The type's generics with the where-clause the generated impl needs: an
    /// explicit `#[ferroly(bound = "...")]` if given, otherwise one trait bound
    /// per type parameter that an encoded/decoded field mentions.
    pub fn generics_for(&self, derive: Derive) -> Generics {
        let mut generics = self.generics.clone();
        let (explicit, trait_path) = match derive {
            Derive::Encode => (&self.bound_encode, quote! { ::ferroly::codec::Encode }),
            Derive::Decode => (&self.bound_decode, quote! { ::ferroly::codec::Decode }),
        };
        let mut predicates: Vec<WherePredicate> = Vec::new();
        match explicit {
            Some(explicit) => predicates.extend(explicit.iter().cloned()),
            None => {
                let sets = self.field_sets();
                for param in self.generics.type_params() {
                    let id = &param.ident;
                    let fields = || sets.iter().flat_map(|fs| fs.fields.iter());
                    if fields().any(|f| !f.skip && type_mentions(&f.ty, id)) {
                        predicates.push(syn::parse_quote! { #id: #trait_path });
                    }
                    if derive == Derive::Decode
                        && fields().any(|f| f.needs_default_impl() && type_mentions(&f.ty, id))
                    {
                        predicates.push(syn::parse_quote! { #id: ::core::default::Default });
                    }
                }
            }
        }
        if !predicates.is_empty() {
            generics.make_where_clause().predicates.extend(predicates);
        }
        generics
    }
}

/// Tracks wire keys already taken, to reject duplicates at compile time.
#[derive(Default)]
struct Keys(HashSet<String>);

impl Keys {
    fn claim(&mut self, key: &str, span: Span, what: &str) -> syn::Result<()> {
        if !self.0.insert(key.to_string()) {
            return Err(syn::Error::new(
                span,
                format!("duplicate {what} key {key:?}: every {what} must map to a distinct key (check `rename`, `rename_all` and `alias`)"),
            ));
        }
        Ok(())
    }
}

fn check_deny_flatten(fields: &FieldSet, deny: Option<Span>) -> syn::Result<()> {
    match deny {
        Some(span) if fields.has_flatten() => Err(syn::Error::new(
            span,
            "`deny_unknown_fields` cannot be combined with a `flatten` field",
        )),
        _ => Ok(()),
    }
}

fn check_internal_variant(v: &syn::Variant, fields: &FieldSet, tag: &str) -> syn::Result<()> {
    if fields.style == Style::Tuple {
        return Err(syn::Error::new_spanned(
            &v.ident,
            "internally tagged enums (`tag = \"...\"`) support only unit and struct variants",
        ));
    }
    for (field, src) in fields.fields.iter().zip(&v.fields) {
        let clash = !field.skip
            && !field.flatten
            && (field.key == tag || field.aliases.iter().any(|a| a == tag));
        if clash {
            return Err(syn::Error::new_spanned(
                &src.ident,
                format!("field key {tag:?} collides with the enum's `tag`"),
            ));
        }
    }
    Ok(())
}

/// Analyses one set of fields, validating attribute combinations and keys.
fn field_set(fields: &Fields, rename_all: Option<RenameRule>) -> syn::Result<FieldSet> {
    let style = match fields {
        Fields::Named(_) => Style::Named,
        Fields::Unnamed(_) => Style::Tuple,
        Fields::Unit => Style::Unit,
    };
    let mut out = Vec::new();
    let mut seen = Keys::default();
    for (i, f) in fields.iter().enumerate() {
        let a = field_attrs(&f.attrs)?;
        let named = f.ident.is_some();

        if !named {
            let unsupported = [
                (a.rename.as_ref().map(LitStr::span), "rename"),
                (a.aliases.first().map(LitStr::span), "alias"),
                (a.skip_none, "skip_none"),
                (a.flatten, "flatten"),
                (
                    a.default.as_ref().filter(|_| a.skip.is_none()).map(|d| d.1),
                    "default",
                ),
            ];
            for (span, what) in unsupported {
                if let Some(span) = span {
                    return Err(syn::Error::new(
                        span,
                        format!("`{what}` is not supported on tuple fields (only `skip` is)"),
                    ));
                }
            }
        }
        if let Some(span) = a.skip {
            let clashes = [
                (a.rename.is_some(), "rename"),
                (!a.aliases.is_empty(), "alias"),
                (a.skip_none.is_some(), "skip_none"),
                (a.flatten.is_some(), "flatten"),
            ];
            for (present, what) in clashes {
                if present {
                    return Err(syn::Error::new(
                        span,
                        format!("`skip` cannot be combined with `{what}`"),
                    ));
                }
            }
        }
        if let Some(span) = a.flatten {
            let clashes = [
                (a.rename.is_some(), "rename"),
                (!a.aliases.is_empty(), "alias"),
                (a.skip_none.is_some(), "skip_none"),
                (a.default.is_some(), "default"),
            ];
            for (present, what) in clashes {
                if present {
                    return Err(syn::Error::new(
                        span,
                        format!("`flatten` cannot be combined with `{what}`"),
                    ));
                }
            }
        }

        let (member, key) = match &f.ident {
            Some(id) => {
                let key = match &a.rename {
                    Some(lit) => lit.value(),
                    None => apply_rename_all(&id.unraw().to_string(), rename_all),
                };
                (Member::Named(id.clone()), key)
            }
            None => (Member::Unnamed(syn::Index::from(i)), String::new()),
        };
        if named && a.skip.is_none() && a.flatten.is_none() {
            let span = a
                .rename
                .as_ref()
                .map(LitStr::span)
                .or_else(|| f.ident.as_ref().map(Ident::span))
                .unwrap_or_else(Span::call_site);
            seen.claim(&key, span, "field")?;
            for alias in &a.aliases {
                seen.claim(&alias.value(), alias.span(), "field")?;
            }
        }

        out.push(Field {
            member,
            ty: f.ty.clone(),
            key,
            aliases: a.aliases.iter().map(LitStr::value).collect(),
            skip: a.skip.is_some(),
            skip_none: a.skip_none.is_some(),
            default: a.default.map(|d| d.0),
            flatten: a.flatten.is_some(),
            option: is_option(&f.ty),
            bind: format_ident!("__f{}", i),
            slot: format_ident!("__s{}", i),
        });
    }
    Ok(FieldSet { style, fields: out })
}

/// Whether a type is spelled `Option<...>` (aliases cannot be seen here; use
/// `#[ferroly(default)]` for those).
pub fn is_option(ty: &Type) -> bool {
    match ty {
        Type::Path(tp) if tp.qself.is_none() => tp
            .path
            .segments
            .last()
            .is_some_and(|seg| seg.ident == "Option"),
        Type::Paren(p) => is_option(&p.elem),
        Type::Group(g) => is_option(&g.elem),
        _ => false,
    }
}

/// Whether `ty` mentions the type parameter `param`. `PhantomData<T>` does not
/// count: it needs nothing of `T`.
pub fn type_mentions(ty: &Type, param: &Ident) -> bool {
    match ty {
        Type::Path(tp) => {
            if let Some(q) = &tp.qself {
                if type_mentions(&q.ty, param) {
                    return true;
                }
            } else if tp.path.leading_colon.is_none() {
                if let Some(first) = tp.path.segments.first() {
                    if first.ident == *param && matches!(first.arguments, PathArguments::None) {
                        return true;
                    }
                }
            }
            if tp
                .path
                .segments
                .last()
                .is_some_and(|seg| seg.ident == "PhantomData")
            {
                return false;
            }
            tp.path.segments.iter().any(|seg| match &seg.arguments {
                PathArguments::None => false,
                PathArguments::AngleBracketed(args) => args.args.iter().any(|arg| match arg {
                    GenericArgument::Type(t) => type_mentions(t, param),
                    GenericArgument::AssocType(a) => type_mentions(&a.ty, param),
                    _ => false,
                }),
                // `Fn(A) -> B` sugar: a token scan is precise enough here.
                PathArguments::Parenthesized(args) => tokens_mention(args.to_token_stream(), param),
            })
        }
        Type::Reference(r) => type_mentions(&r.elem, param),
        Type::Slice(s) => type_mentions(&s.elem, param),
        Type::Array(a) => type_mentions(&a.elem, param),
        Type::Ptr(p) => type_mentions(&p.elem, param),
        Type::Paren(p) => type_mentions(&p.elem, param),
        Type::Group(g) => type_mentions(&g.elem, param),
        Type::Tuple(t) => t.elems.iter().any(|e| type_mentions(e, param)),
        // Function pointers, trait objects, macros, …: fall back to a token scan.
        other => tokens_mention(other.to_token_stream(), param),
    }
}

fn tokens_mention(tokens: TokenStream, param: &Ident) -> bool {
    tokens.into_iter().any(|tt| match tt {
        TokenTree::Ident(id) => id == *param,
        TokenTree::Group(g) => tokens_mention(g.stream(), param),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn parse(input: DeriveInput) -> syn::Result<Container> {
        Container::parse(&input)
    }

    fn message(input: DeriveInput) -> String {
        match parse(input) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    fn struct_fields(c: &Container) -> &FieldSet {
        match &c.body {
            Body::Struct(fs) => fs,
            Body::Enum(..) => panic!("expected a struct"),
        }
    }

    fn where_clause(c: &Container, derive: Derive) -> String {
        c.generics_for(derive)
            .where_clause
            .map(|w| w.to_token_stream().to_string())
            .unwrap_or_default()
    }

    #[test]
    fn raw_identifiers_lose_their_prefix_in_keys() {
        let c = parse(parse_quote! {
            #[ferroly(rename_all = "SCREAMING_SNAKE_CASE")]
            struct S { r#type: u8, r#match_arm: u8 }
        })
        .unwrap();
        let keys: Vec<&str> = struct_fields(&c)
            .fields
            .iter()
            .map(|f| f.key.as_str())
            .collect();
        assert_eq!(keys, ["TYPE", "MATCH_ARM"]);

        let c = parse(parse_quote! { enum E { r#Self_, r#type } }).unwrap();
        match &c.body {
            Body::Enum(variants, repr) => {
                assert_eq!(*repr, Repr::External);
                assert_eq!(variants[0].key, "Self_");
                assert_eq!(variants[1].key, "type");
            }
            Body::Struct(_) => panic!("expected an enum"),
        }
    }

    #[test]
    fn styles_and_newtype_detection() {
        let c = parse(parse_quote! { struct S; }).unwrap();
        assert_eq!(struct_fields(&c).style, Style::Unit);

        let c = parse(parse_quote! { struct S(u8); }).unwrap();
        assert!(struct_fields(&c).newtype().is_some());

        let c = parse(parse_quote! { struct S(u8, u8); }).unwrap();
        assert!(struct_fields(&c).newtype().is_none());

        // A skipped marker keeps a one-value tuple struct transparent.
        let c = parse(parse_quote! {
            struct Id<T>(u64, #[ferroly(skip)] PhantomData<T>);
        })
        .unwrap();
        let only = struct_fields(&c).newtype().unwrap();
        assert!(matches!(&only.member, Member::Unnamed(i) if i.index == 0));

        // A single named field is still an object.
        let c = parse(parse_quote! { struct S { a: u8 } }).unwrap();
        assert!(struct_fields(&c).newtype().is_none());
    }

    #[test]
    fn duplicate_field_keys_are_rejected() {
        let renamed = message(parse_quote! {
            struct S { a: u8, #[ferroly(rename = "a")] b: u8 }
        });
        assert!(
            renamed.starts_with("duplicate field key \"a\""),
            "{renamed}"
        );

        let by_rule = message(parse_quote! {
            #[ferroly(rename_all = "camelCase")]
            struct S { user_id: u8, userId: u8 }
        });
        assert!(
            by_rule.starts_with("duplicate field key \"userId\""),
            "{by_rule}"
        );

        let alias = message(parse_quote! {
            struct S { a: u8, #[ferroly(alias = "a")] b: u8 }
        });
        assert!(alias.starts_with("duplicate field key \"a\""), "{alias}");

        let alias_twice = message(parse_quote! {
            struct S { #[ferroly(alias = "x", alias = "x")] b: u8 }
        });
        assert!(alias_twice.starts_with("duplicate field key \"x\""));

        let raw = message(parse_quote! {
            struct S { r#type: u8, #[ferroly(rename = "type")] kind: u8 }
        });
        assert!(raw.starts_with("duplicate field key \"type\""), "{raw}");

        // A skipped field claims no key.
        assert!(parse(parse_quote! {
            struct S { a: u8, #[ferroly(skip)] b: u8, #[ferroly(rename = "b")] c: u8 }
        })
        .is_ok());
    }

    #[test]
    fn duplicate_variant_keys_are_rejected() {
        let m = message(parse_quote! {
            #[ferroly(rename_all = "lowercase")]
            enum E { Ab, AB }
        });
        assert!(m.starts_with("duplicate variant key \"ab\""), "{m}");

        let m = message(parse_quote! { enum E { A, #[ferroly(alias = "A")] B } });
        assert!(m.starts_with("duplicate variant key \"A\""), "{m}");

        // Variant names are irrelevant on the wire for untagged enums.
        assert!(parse(parse_quote! {
            #[ferroly(untagged)]
            enum E { #[ferroly(rename = "x")] A(u8), #[ferroly(rename = "x")] B(String) }
        })
        .is_ok());

        // Keys are scoped per variant.
        assert!(parse(parse_quote! { enum E { A { id: u8 }, B { id: u8 } } }).is_ok());
        let m = message(parse_quote! {
            enum E { A { id: u8, #[ferroly(rename = "id")] other: u8 } }
        });
        assert!(m.starts_with("duplicate field key \"id\""), "{m}");
    }

    #[test]
    fn representation_attributes_are_validated() {
        let c = parse(parse_quote! { #[ferroly(tag = "kind")] enum E { A, B { x: u8 } } }).unwrap();
        assert!(matches!(&c.body, Body::Enum(_, Repr::Internal(tag)) if tag == "kind"));
        let c = parse(parse_quote! { #[ferroly(untagged)] enum E { A(u8) } }).unwrap();
        assert!(matches!(&c.body, Body::Enum(_, Repr::Untagged)));

        assert_eq!(
            message(parse_quote! { #[ferroly(tag = "t", untagged)] enum E { A } }),
            "`untagged` cannot be combined with `tag`"
        );
        assert_eq!(
            message(parse_quote! { #[ferroly(tag = "t")] struct S { a: u8 } }),
            "`tag` is only valid on enums"
        );
        assert_eq!(
            message(parse_quote! { #[ferroly(untagged)] struct S { a: u8 } }),
            "`untagged` is only valid on enums"
        );
        assert!(
            message(parse_quote! { #[ferroly(tag = "t")] enum E { A(u8) } })
                .contains("support only unit and struct variants")
        );
        assert_eq!(
            message(parse_quote! { #[ferroly(tag = "t")] enum E { A { t: u8 } } }),
            "field key \"t\" collides with the enum's `tag`"
        );
        assert_eq!(
            message(parse_quote! {
                #[ferroly(tag = "t")] enum E { A { #[ferroly(alias = "t")] x: u8 } }
            }),
            "field key \"t\" collides with the enum's `tag`"
        );
        assert!(message(parse_quote! { union U { a: u8, b: u16 } }).contains("unions"));
    }

    #[test]
    fn field_attribute_combinations_are_validated() {
        assert_eq!(
            message(parse_quote! { struct S { #[ferroly(skip, rename = "x")] a: u8 } }),
            "`skip` cannot be combined with `rename`"
        );
        assert_eq!(
            message(parse_quote! { struct S { #[ferroly(skip, flatten)] a: u8 } }),
            "`skip` cannot be combined with `flatten`"
        );
        assert_eq!(
            message(parse_quote! { struct S { #[ferroly(flatten, default)] a: u8 } }),
            "`flatten` cannot be combined with `default`"
        );
        assert_eq!(
            message(parse_quote! { struct S { #[ferroly(flatten, alias = "x")] a: u8 } }),
            "`flatten` cannot be combined with `alias`"
        );
        assert_eq!(
            message(parse_quote! { struct S(#[ferroly(rename = "x")] u8); }),
            "`rename` is not supported on tuple fields (only `skip` is)"
        );
        assert_eq!(
            message(parse_quote! { struct S(#[ferroly(default)] u8); }),
            "`default` is not supported on tuple fields (only `skip` is)"
        );
        assert_eq!(
            message(parse_quote! {
                #[ferroly(deny_unknown_fields)]
                struct S { #[ferroly(flatten)] rest: Rest }
            }),
            "`deny_unknown_fields` cannot be combined with a `flatten` field"
        );
        // `skip` may name the function that supplies the value.
        assert!(parse(parse_quote! {
            struct S(u8, #[ferroly(skip, default = "make")] Marker);
        })
        .is_ok());
    }

    #[test]
    fn option_detection_is_syntactic() {
        let yes: [Type; 3] = [
            parse_quote!(Option<u8>),
            parse_quote!(::core::option::Option<String>),
            parse_quote!(std::option::Option<Vec<u8>>),
        ];
        for ty in &yes {
            assert!(is_option(ty));
        }
        let no: [Type; 3] = [
            parse_quote!(Maybe<u8>),
            parse_quote!(Vec<Option<u8>>),
            parse_quote!(<T as Trait>::Option),
        ];
        for ty in &no {
            assert!(!is_option(ty));
        }
    }

    #[test]
    fn type_parameter_mentions() {
        let t: Ident = parse_quote!(T);
        let yes: [Type; 8] = [
            parse_quote!(T),
            parse_quote!(Vec<T>),
            parse_quote!(Option<Box<T>>),
            parse_quote!(HashMap<String, T>),
            parse_quote!((u8, T)),
            parse_quote!(&'a [T]),
            parse_quote!(T::Item),
            parse_quote!(<T as Iterator>::Item),
        ];
        for ty in &yes {
            assert!(type_mentions(ty, &t), "{}", ty.to_token_stream());
        }
        let no: [Type; 6] = [
            parse_quote!(u8),
            parse_quote!(Vec<U>),
            parse_quote!(PhantomData<T>),
            parse_quote!(::core::marker::PhantomData<Vec<T>>),
            parse_quote!(Tree),
            parse_quote!(::T),
        ];
        for ty in &no {
            assert!(!type_mentions(ty, &t), "{}", ty.to_token_stream());
        }
        let fn_ptr: Type = parse_quote!(fn(T) -> u8);
        assert!(type_mentions(&fn_ptr, &t));
    }

    #[test]
    fn inferred_bounds_cover_only_used_parameters() {
        let c = parse(parse_quote! {
            struct Page<T, M, U> where T: Clone {
                items: Vec<T>,
                #[ferroly(skip)]
                marker: PhantomData<M>,
                #[ferroly(skip)]
                unused: U,
            }
        })
        .unwrap();
        assert_eq!(
            where_clause(&c, Derive::Encode),
            "where T : Clone , T : :: ferroly :: codec :: Encode"
        );
        // A skipped field is rebuilt from `Default`, so `U` needs it to decode.
        assert_eq!(
            where_clause(&c, Derive::Decode),
            "where T : Clone , T : :: ferroly :: codec :: Decode , U : :: core :: default :: Default"
        );
    }

    #[test]
    fn default_fields_require_default_only_when_no_function_is_named() {
        let c = parse(parse_quote! {
            struct S<A, B> {
                #[ferroly(default)]
                a: A,
                #[ferroly(default = "make_b")]
                b: B,
            }
        })
        .unwrap();
        assert_eq!(
            where_clause(&c, Derive::Decode),
            "where A : :: ferroly :: codec :: Decode , A : :: core :: default :: Default , \
             B : :: ferroly :: codec :: Decode"
        );
    }

    #[test]
    fn explicit_bounds_replace_inference() {
        let c = parse(parse_quote! {
            #[ferroly(bound(encode = "T::Item: ::ferroly::codec::Encode", decode = ""))]
            struct S<T: Iterator> { item: T::Item }
        })
        .unwrap();
        assert_eq!(
            where_clause(&c, Derive::Encode),
            "where T :: Item : :: ferroly :: codec :: Encode"
        );
        assert_eq!(where_clause(&c, Derive::Decode), "");

        // Enum bounds are gathered across every variant.
        let c = parse(parse_quote! { enum E<A, B, C> { One(A), Two { b: B }, Three } }).unwrap();
        assert_eq!(
            where_clause(&c, Derive::Encode),
            "where A : :: ferroly :: codec :: Encode , B : :: ferroly :: codec :: Encode"
        );
    }

    #[test]
    fn patterns_and_constructors() {
        let c = parse(parse_quote! {
            struct S { a: u8, #[ferroly(skip)] b: u8, r#type: u8 }
        })
        .unwrap();
        let fs = struct_fields(&c);
        assert_eq!(
            fs.pattern(&quote!(Self)).to_string(),
            "Self { a : __f0 , r#type : __f2 , .. }"
        );
        let built = fs.construct(&quote!(Self), |f| f.slot.to_token_stream());
        assert_eq!(
            built.to_string(),
            "Self { a : __s0 , b : __s1 , r#type : __s2 }"
        );

        let c = parse(parse_quote! { struct S(u8, #[ferroly(skip)] u8); }).unwrap();
        let fs = struct_fields(&c);
        assert_eq!(fs.pattern(&quote!(Self)).to_string(), "Self (__f0 , _)");
        let c = parse(parse_quote! { struct S; }).unwrap();
        assert_eq!(struct_fields(&c).pattern(&quote!(Self)).to_string(), "Self");
    }
}
