//! `#[derive(FerrolyError)]` expansion — a `thiserror`-subset.
//!
//! Supports enums (per-variant `#[error(...)]`, with an optional enum-level
//! fallback) and structs (container `#[error(...)]`). Generates `Display` from
//! `#[error("...")]` / `#[error(transparent)]`, `std::error::Error` with
//! `source()` from `#[from]`/`#[source]` fields (and transparent single-field
//! carriers), and `From` impls for `#[from]`.
//!
//! Structs and enum variants go through the same [`carrier`] routine, so every
//! attribute behaves identically on both.

use proc_macro2::TokenStream;
use quote::{format_ident, quote, ToTokens};
use syn::ext::IdentExt;
use syn::{
    Attribute, Data, DeriveInput, Fields, GenericArgument, Ident, LitStr, PathArguments, Type,
};

use crate::attrs::{error_display, marker, ErrorDisplay};
use crate::fmtstr;
use crate::model::{is_option, type_mentions};

/// Everything gathered from the carriers of one error type.
#[derive(Default)]
struct Parts {
    display_arms: Vec<TokenStream>,
    source_arms: Vec<TokenStream>,
    /// `(source type, constructor from the local __v)`.
    from_impls: Vec<(Type, TokenStream)>,
    /// Where-predicates the `Display` impl needs (generic types only).
    display_bounds: Vec<TokenStream>,
    /// Where-predicates the `Error` impl needs (generic types only).
    error_bounds: Vec<TokenStream>,
}

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let type_params: Vec<Ident> = input
        .generics
        .type_params()
        .map(|p| p.ident.clone())
        .collect();
    let mut parts = Parts::default();

    reject_markers(&input.attrs)?;
    match &input.data {
        Data::Enum(e) => {
            // An enum-level `#[error(...)]` is the fallback for variants
            // that do not carry their own.
            let fallback = error_display(&input.attrs)?;
            for v in &e.variants {
                reject_markers(&v.attrs)?;
                let spec = match error_display(&v.attrs)? {
                    Some(spec) => spec,
                    None => fallback.clone().ok_or_else(|| {
                        syn::Error::new_spanned(
                            &v.ident,
                            "each variant needs #[error(\"...\")] or #[error(transparent)]",
                        )
                    })?,
                };
                let vid = &v.ident;
                carrier(
                    &quote! { Self::#vid },
                    vid,
                    &v.fields,
                    spec,
                    &type_params,
                    &mut parts,
                )?;
            }
        }
        Data::Struct(s) => {
            let spec = error_display(&input.attrs)?.ok_or_else(|| {
                syn::Error::new_spanned(
                    name,
                    "struct needs #[error(\"...\")] or #[error(transparent)]",
                )
            })?;
            carrier(
                &quote! { Self },
                name,
                &s.fields,
                spec,
                &type_params,
                &mut parts,
            )?;
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                name,
                "FerrolyError supports enums and structs, not unions",
            ));
        }
    }

    // An enum with no variants (e.g. all cfg'd out) is uninhabited; `match self
    // {}` is rejected on a reference, so match the dereferenced value.
    let display_arms = &parts.display_arms;
    let display_match = if display_arms.is_empty() {
        quote! { match *self {} }
    } else {
        quote! { match self { #(#display_arms)* } }
    };
    let source_arms = &parts.source_arms;

    let mut display_generics = input.generics.clone();
    add_predicates(&mut display_generics, &parts.display_bounds);
    let mut error_generics = input.generics.clone();
    add_predicates(&mut error_generics, &parts.error_bounds);
    if !type_params.is_empty() {
        // `Error` requires `Debug + Display`, which a generic type only has
        // under bounds this derive cannot see (e.g. those of `#[derive(Debug)]`).
        add_predicates(
            &mut error_generics,
            &[quote! { Self: ::core::fmt::Debug + ::core::fmt::Display }],
        );
    }
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    let (d_impl, _, d_where) = display_generics.split_for_impl();
    let (e_impl, _, e_where) = error_generics.split_for_impl();

    let from_impls = parts.from_impls.iter().map(|(ty, construct)| {
        quote! {
            #[automatically_derived]
            impl #impl_generics ::core::convert::From<#ty> for #name #ty_generics #where_clause {
                fn from(__v: #ty) -> Self {
                    #construct
                }
            }
        }
    });

    // The impls live in an anonymous const so the `__FerrolyAsDynError` helper
    // (which coerces `#[from]`/`#[source]` fields, including `Box<dyn Error>`,
    // to `&dyn Error`) is scoped to this one expansion.
    Ok(quote! {
        const _: () = {
            #[allow(dead_code)]
            trait __FerrolyAsDynError<'__a> {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a);
            }
            impl<'__a, __T: ::std::error::Error + '__a> __FerrolyAsDynError<'__a> for __T {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a) { self }
            }
            impl<'__a> __FerrolyAsDynError<'__a> for dyn ::std::error::Error + '__a {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a) { self }
            }
            impl<'__a> __FerrolyAsDynError<'__a>
                for dyn ::std::error::Error + ::core::marker::Send + '__a
            {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a) { self }
            }
            impl<'__a> __FerrolyAsDynError<'__a>
                for dyn ::std::error::Error + ::core::marker::Send + ::core::marker::Sync + '__a
            {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a) { self }
            }

            #[automatically_derived]
            impl #d_impl ::core::fmt::Display for #name #ty_generics #d_where {
                #[allow(unused_variables)]
                fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    #display_match
                }
            }

            #[automatically_derived]
            impl #e_impl ::std::error::Error for #name #ty_generics #e_where {
                #[allow(unused_variables, unreachable_patterns)]
                fn source(&self) -> ::core::option::Option<&(dyn ::std::error::Error + 'static)> {
                    match self {
                        #(#source_arms)*
                        _ => ::core::option::Option::None,
                    }
                }
            }

            #(#from_impls)*
        };
    })
}

/// `#[from]` / `#[source]` describe a field; reject them anywhere else.
fn reject_markers(attrs: &[Attribute]) -> syn::Result<()> {
    for name in ["from", "source"] {
        if let Some(a) = attrs.iter().find(|a| a.path().is_ident(name)) {
            return Err(syn::Error::new_spanned(
                a,
                format!("#[{name}] belongs on a field"),
            ));
        }
    }
    Ok(())
}

fn add_predicates(generics: &mut syn::Generics, predicates: &[TokenStream]) {
    let mut seen: Vec<String> = Vec::new();
    for p in predicates {
        let key = p.to_string();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let parsed: syn::WherePredicate = syn::parse_quote! { #p };
        generics.make_where_clause().predicates.push(parsed);
    }
}

/// The `T` of a type spelled `Option<T>`.
fn option_inner(ty: &Type) -> Option<&Type> {
    let Type::Path(tp) = ty else { return None };
    let seg = tp.path.segments.last()?;
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first()? {
        GenericArgument::Type(inner) if args.args.len() == 1 => Some(inner),
        _ => None,
    }
}

/// Generates the `Display` arm, `source()` arm and `From` impl for one struct
/// or enum variant. `path` is `Self` or `Self::Variant`; `at` anchors errors
/// about the carrier as a whole.
fn carrier(
    path: &TokenStream,
    at: &Ident,
    fields: &Fields,
    spec: ErrorDisplay,
    type_params: &[Ident],
    parts: &mut Parts,
) -> syn::Result<()> {
    // One local per field: `__0`, `__1`, … for tuple fields and `__field_<name>`
    // for named ones (raw identifiers lose their `r#`), so nothing generated
    // can collide with — or be shadowed by — a user's field name.
    struct Slot<'a> {
        field: &'a syn::Field,
        /// The name a `{placeholder}` uses for this field.
        name: String,
        bind: Ident,
    }
    let slots: Vec<Slot> = fields
        .iter()
        .enumerate()
        .map(|(i, field)| match &field.ident {
            Some(id) => {
                let name = id.unraw().to_string();
                let bind = format_ident!("__field_{}", name);
                Slot { field, name, bind }
            }
            None => Slot {
                field,
                name: i.to_string(),
                bind: format_ident!("__{}", i),
            },
        })
        .collect();
    let binds = slots.iter().map(|s| &s.bind);
    let pattern = match fields {
        Fields::Unit => quote! { #path },
        Fields::Unnamed(_) => quote! { #path( #(#binds),* ) },
        Fields::Named(_) => {
            let ids = slots.iter().map(|s| &s.field.ident);
            quote! { #path { #(#ids: #binds),* } }
        }
    };
    let is_generic = |ty: &Type| type_params.iter().any(|p| type_mentions(ty, p));

    let mut from: Option<&Attribute> = None;
    let mut source: Option<usize> = None;
    for (i, slot) in slots.iter().enumerate() {
        let attrs = &slot.field.attrs;
        if let Some(a) = attrs.iter().find(|a| a.path().is_ident("error")) {
            return Err(syn::Error::new_spanned(
                a,
                "#[error(...)] belongs on a struct or enum variant, not on a field",
            ));
        }
        if let Some(a) = marker(attrs, "from")? {
            if slots.len() != 1 {
                return Err(syn::Error::new_spanned(
                    a,
                    "#[from] requires exactly one field: the generated `From` impl must build \
                     the whole value from it (use #[source] to only mark the cause)",
                ));
            }
            if is_option(&slot.field.ty) {
                return Err(syn::Error::new_spanned(
                    a,
                    "#[from] is not supported on an `Option` field",
                ));
            }
            from = Some(a);
            source = Some(i);
        }
        if let Some(a) = marker(attrs, "source")? {
            if source.is_some_and(|s| s != i) {
                return Err(syn::Error::new_spanned(
                    a,
                    "only one field may be marked #[source]",
                ));
            }
            source = Some(i);
        }
    }

    let transparent = matches!(spec, ErrorDisplay::Transparent);
    match spec {
        ErrorDisplay::Transparent => {
            if slots.len() != 1 {
                return Err(syn::Error::new_spanned(
                    at,
                    "#[error(transparent)] requires exactly one field",
                ));
            }
            let (bind, ty) = (&slots[0].bind, &slots[0].field.ty);
            parts
                .display_arms
                .push(quote! { #pattern => ::core::fmt::Display::fmt(#bind, __f), });
            if is_generic(ty) {
                parts
                    .display_bounds
                    .push(quote! { #ty: ::core::fmt::Display });
            }
            source = Some(0);
        }
        ErrorDisplay::Format(lit) => {
            let rewritten = fmtstr::rewrite(&lit.value(), &|name| {
                slots
                    .iter()
                    .position(|s| s.name == name)
                    .map(|i| (i, slots[i].bind.to_string()))
            });
            let text = LitStr::new(&rewritten.text, lit.span());
            // A field used as a `name$` width/precision must reach `write!` as a
            // `usize`, not as the reference the pattern binds.
            let counts = rewritten.counts.iter().map(|&i| &slots[i].bind);
            parts.display_arms.push(quote! {
                #pattern => ::core::write!(__f, #text #(, #counts = *#counts)*),
            });
            for (i, trait_name) in rewritten.uses {
                let ty = &slots[i].field.ty;
                if is_generic(ty) {
                    let trait_name = format_ident!("{}", trait_name);
                    parts
                        .display_bounds
                        .push(quote! { #ty: ::core::fmt::#trait_name });
                }
            }
        }
    }

    if let Some(i) = source {
        let (bind, ty) = (&slots[i].bind, &slots[i].field.ty);
        let optional = if transparent {
            None
        } else {
            option_inner(ty).filter(|_| is_option(ty))
        };
        let (value, error_ty) = match optional {
            // `#[source] cause: Option<E>` — a source only when present.
            Some(inner) => (
                quote! {
                    match #bind {
                        ::core::option::Option::Some(__e) => {
                            ::core::option::Option::Some(__e.as_dyn_error())
                        }
                        ::core::option::Option::None => ::core::option::Option::None,
                    }
                },
                inner,
            ),
            None => (
                quote! { ::core::option::Option::Some(#bind.as_dyn_error()) },
                ty,
            ),
        };
        parts.source_arms.push(quote! { #pattern => #value, });
        if is_generic(error_ty) {
            parts
                .error_bounds
                .push(quote! { #error_ty: ::std::error::Error + 'static });
        }
    }

    if let Some(attr) = from {
        let slot = &slots[0];
        let ty = &slot.field.ty;
        let key = ty.to_token_stream().to_string();
        let taken = parts
            .from_impls
            .iter()
            .any(|(other, _)| other.to_token_stream().to_string() == key);
        if taken {
            return Err(syn::Error::new_spanned(
                attr,
                format!(
                    "duplicate #[from] for `{}`: only one variant can be built from a given type",
                    key.replace(' ', "")
                ),
            ));
        }
        let construct = match &slot.field.ident {
            Some(id) => quote! { #path { #id: __v } },
            None => quote! { #path(__v) },
        };
        parts.from_impls.push((ty.clone(), construct));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    fn generated(input: DeriveInput) -> String {
        expand(input).unwrap().to_string()
    }

    fn message(input: DeriveInput) -> String {
        match expand(input) {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn struct_expansions_include_the_source_helper_and_from() {
        let out = generated(parse_quote! {
            #[error("request {id} failed")]
            struct E { id: u64, #[source] cause: std::io::Error }
        });
        assert!(out.contains("trait __FerrolyAsDynError"), "{out}");
        assert!(out.contains("__field_cause . as_dyn_error ()"), "{out}");
        assert!(out.contains("\"request {__field_id} failed\""), "{out}");

        let out = generated(parse_quote! {
            #[error(transparent)]
            struct E(#[from] std::io::Error);
        });
        assert!(
            out.contains("impl :: core :: convert :: From < std :: io :: Error > for E"),
            "{out}"
        );

        let out = generated(parse_quote! {
            #[error("io")]
            struct E { #[from] inner: std::io::Error }
        });
        assert!(out.contains("Self { inner : __v }"), "{out}");
    }

    #[test]
    fn named_variant_from_generates_a_constructor() {
        let out = generated(parse_quote! {
            enum E {
                #[error("io failed")]
                Io { #[from] cause: std::io::Error },
                #[error(transparent)]
                Other { #[from] inner: std::fmt::Error },
            }
        });
        assert!(out.contains("Self :: Io { cause : __v }"), "{out}");
        assert!(out.contains("Self :: Other { inner : __v }"), "{out}");
    }

    #[test]
    fn unsupported_from_shapes_are_errors() {
        let two_fields = message(parse_quote! {
            enum E { #[error("x")] A(#[from] std::io::Error, u8) }
        });
        assert!(
            two_fields.starts_with("#[from] requires exactly one field"),
            "{two_fields}"
        );

        let named_two = message(parse_quote! {
            #[error("x")]
            struct E { #[from] cause: std::io::Error, code: u8 }
        });
        assert!(named_two.starts_with("#[from] requires exactly one field"));

        let both = message(parse_quote! {
            enum E { #[error("x")] A(#[from] std::io::Error, #[from] std::fmt::Error) }
        });
        assert!(both.starts_with("#[from] requires exactly one field"));

        assert_eq!(
            message(parse_quote! { enum E { #[error("x")] A(#[from(io)] std::io::Error) } }),
            "#[from] takes no arguments"
        );
        assert_eq!(
            message(parse_quote! { enum E { #[error("x")] A(#[from] Option<std::io::Error>) } }),
            "#[from] is not supported on an `Option` field"
        );
        assert_eq!(
            message(parse_quote! { enum E { #[error("x")] #[from] A(std::io::Error) } }),
            "#[from] belongs on a field"
        );
        assert_eq!(
            message(parse_quote! { #[error("x")] #[source] struct E(std::io::Error); }),
            "#[source] belongs on a field"
        );
        let dup = message(parse_quote! {
            enum E {
                #[error("a")] A(#[from] std::io::Error),
                #[error("b")] B(#[from] std::io::Error),
            }
        });
        assert!(
            dup.starts_with("duplicate #[from] for `std::io::Error`"),
            "{dup}"
        );
    }

    #[test]
    fn other_rejected_shapes() {
        assert!(message(parse_quote! { enum E { A } }).contains("each variant needs #[error"));
        assert!(message(parse_quote! { struct E; }).contains("struct needs #[error"));
        assert!(
            message(parse_quote! { #[error("x")] union U { a: u8, b: u16 } })
                .contains("not unions")
        );
        assert_eq!(
            message(parse_quote! { #[error(transparent)] struct E(u8, u8); }),
            "#[error(transparent)] requires exactly one field"
        );
        assert_eq!(
            message(parse_quote! { enum E { #[error(transparent)] A } }),
            "#[error(transparent)] requires exactly one field"
        );
        assert_eq!(
            message(parse_quote! {
                #[error("x")]
                struct E { #[source] a: std::io::Error, #[source] b: std::io::Error }
            }),
            "only one field may be marked #[source]"
        );
        assert!(message(parse_quote! {
            #[error("x")] struct E { #[error("y")] a: u8 }
        })
        .contains("not on a field"));
        assert!(
            message(parse_quote! { enum E { #[error("x", 1)] A } }).contains("expected #[error(")
        );
    }

    #[test]
    fn enum_level_error_is_a_fallback() {
        let out = generated(parse_quote! {
            #[error("generic failure")]
            enum E { A, #[error("specific")] B }
        });
        assert!(out.contains("\"generic failure\"") && out.contains("\"specific\""));
    }

    #[test]
    fn placeholders_are_bound_to_hygienic_locals() {
        let out = generated(parse_quote! {
            enum E {
                #[error("{0:?} {{0}} {1}")]
                T(u8, u8),
                #[error("{type} {r#kind}")]
                N { r#type: u8, kind: u8 },
            }
        });
        assert!(out.contains("Self :: T (__0 , __1)"), "{out}");
        assert!(out.contains("\"{__0:?} {{0}} {__1}\""), "{out}");
        assert!(
            out.contains("Self :: N { r#type : __field_type , kind : __field_kind }"),
            "{out}"
        );
        // `{r#kind}` is not a placeholder name this derive rewrites.
        assert!(out.contains("\"{__field_type} {r#kind}\""), "{out}");
    }

    #[test]
    fn generic_errors_get_inferred_bounds() {
        let out = generated(parse_quote! {
            #[error("{code:?}: {detail}")]
            struct E<C, D, S> where C: Copy {
                code: C,
                detail: Vec<D>,
                plain: u8,
                #[source] cause: S,
            }
        });
        assert!(
            out.contains(
                "impl < C , D , S > :: core :: fmt :: Display for E < C , D , S > where C : Copy , \
                 C : :: core :: fmt :: Debug , Vec < D > : :: core :: fmt :: Display"
            ),
            "{out}"
        );
        assert!(
            out.contains(
                "impl < C , D , S > :: std :: error :: Error for E < C , D , S > where C : Copy , \
                 S : :: std :: error :: Error + 'static , \
                 Self : :: core :: fmt :: Debug + :: core :: fmt :: Display"
            ),
            "{out}"
        );
    }

    #[test]
    fn optional_sources_bound_the_inner_type() {
        let out = generated(parse_quote! {
            #[error("failed")]
            struct E<S> { #[source] cause: Option<S> }
        });
        assert!(
            out.contains("S : :: std :: error :: Error + 'static"),
            "{out}"
        );
        assert!(
            !out.contains("Option < S > : :: std :: error :: Error"),
            "{out}"
        );
    }

    #[test]
    fn non_generic_errors_get_no_where_clause() {
        let out = generated(parse_quote! { #[error("x")] struct E; });
        assert!(
            out.contains("impl :: std :: error :: Error for E {"),
            "{out}"
        );
    }

    #[test]
    fn empty_enums_expand() {
        let out = generated(parse_quote! { enum Never {} });
        assert!(out.contains("match * self { }"), "{out}");
    }
}
