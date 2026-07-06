//! `#[derive(FerrolyError)]` expansion — a `thiserror`-subset.
//!
//! Supports enums (per-variant `#[error(...)]`) and structs (container
//! `#[error(...)]`). Generates `Display` from `#[error("...")]` /
//! `#[error(transparent)]`, `std::error::Error` with `source()` from
//! `#[from]`/`#[source]` fields (and transparent single-field carriers), and
//! `From` impls for `#[from]`.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, LitStr};

use crate::attrs::{error_display, has_from, is_source, ErrorDisplay};

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    match &input.data {
        Data::Enum(_) => expand_enum(&input),
        Data::Struct(_) => expand_struct(&input),
        Data::Union(_) => Err(syn::Error::new_spanned(
            &input.ident,
            "FerrolyError supports enums and structs",
        )),
    }
}

// ---- enums ---------------------------------------------------------------

fn expand_enum(input: &DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let data = match &input.data {
        Data::Enum(e) => e,
        _ => unreachable!(),
    };

    let mut display_arms = Vec::new();
    let mut source_arms = Vec::new();
    let mut from_impls = Vec::new();

    for v in &data.variants {
        let vid = &v.ident;
        let spec = error_display(&v.attrs).ok_or_else(|| {
            syn::Error::new_spanned(
                vid,
                "each variant needs #[error(\"...\")] or #[error(transparent)]",
            )
        })?;
        match spec {
            ErrorDisplay::Transparent => {
                emit_transparent(
                    name,
                    v,
                    &mut display_arms,
                    &mut source_arms,
                    &mut from_impls,
                )?;
            }
            ErrorDisplay::Format(lit) => {
                emit_format(
                    name,
                    v,
                    &lit,
                    &mut display_arms,
                    &mut source_arms,
                    &mut from_impls,
                );
            }
        }
    }

    // An enum with all variants cfg'd out becomes empty; `match self {}` is
    // rejected on a reference, so deref to match the (uninhabited) value.
    let display_match = if display_arms.is_empty() {
        quote! { match *self {} }
    } else {
        quote! { match self { #(#display_arms)* } }
    };

    let impls = quote! {
        impl ::core::fmt::Display for #name {
            #[allow(unused_variables)]
            fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                #display_match
            }
        }

        impl ::std::error::Error for #name {
            fn source(&self) -> ::core::option::Option<&(dyn ::std::error::Error + 'static)> {
                #[allow(unreachable_patterns)]
                match self {
                    #(#source_arms)*
                    _ => ::core::option::Option::None,
                }
            }
        }

        #(#from_impls)*
    };
    Ok(wrap(impls))
}

/// Wraps generated impls in an anonymous const so the `AsDynError` helper
/// (used to coerce `#[from]`/`#[source]` fields, including `Box<dyn Error>`,
/// to `&dyn Error`) is scoped per-derive and never collides.
fn wrap(impls: TokenStream) -> TokenStream {
    quote! {
        const _: () = {
            trait AsDynError<'__a> {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a);
            }
            impl<'__a, T: ::std::error::Error + '__a> AsDynError<'__a> for T {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a) { self }
            }
            impl<'__a> AsDynError<'__a> for dyn ::std::error::Error + '__a {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a) { self }
            }
            impl<'__a> AsDynError<'__a>
                for dyn ::std::error::Error + ::core::marker::Send + '__a
            {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a) { self }
            }
            impl<'__a> AsDynError<'__a>
                for dyn ::std::error::Error + ::core::marker::Send + ::core::marker::Sync + '__a
            {
                fn as_dyn_error(&self) -> &(dyn ::std::error::Error + '__a) { self }
            }
            #impls
        };
    }
}

fn emit_transparent(
    name: &syn::Ident,
    v: &syn::Variant,
    display_arms: &mut Vec<TokenStream>,
    source_arms: &mut Vec<TokenStream>,
    from_impls: &mut Vec<TokenStream>,
) -> syn::Result<()> {
    let vid = &v.ident;
    match &v.fields {
        Fields::Unnamed(fu) if fu.unnamed.len() == 1 => {
            display_arms.push(quote! { Self::#vid(__0) => ::core::fmt::Display::fmt(__0, __f), });
            source_arms.push(quote! {
                Self::#vid(__0) =>
                    ::core::option::Option::Some(__0.as_dyn_error()),
            });
            if has_from(&fu.unnamed[0].attrs) {
                let ty = &fu.unnamed[0].ty;
                from_impls.push(quote! {
                    impl ::core::convert::From<#ty> for #name {
                        fn from(__v: #ty) -> Self { #name::#vid(__v) }
                    }
                });
            }
            Ok(())
        }
        Fields::Named(fields) if fields.named.len() == 1 => {
            let id = fields.named[0].ident.as_ref().unwrap();
            display_arms
                .push(quote! { Self::#vid { #id } => ::core::fmt::Display::fmt(#id, __f), });
            source_arms.push(quote! {
                Self::#vid { #id } =>
                    ::core::option::Option::Some(#id.as_dyn_error()),
            });
            Ok(())
        }
        _ => Err(syn::Error::new_spanned(
            vid,
            "#[error(transparent)] requires exactly one field",
        )),
    }
}

fn emit_format(
    name: &syn::Ident,
    v: &syn::Variant,
    lit: &LitStr,
    display_arms: &mut Vec<TokenStream>,
    source_arms: &mut Vec<TokenStream>,
    from_impls: &mut Vec<TokenStream>,
) {
    let vid = &v.ident;
    match &v.fields {
        Fields::Unit => {
            display_arms.push(quote! { Self::#vid => ::core::write!(__f, #lit), });
        }
        Fields::Unnamed(fu) => {
            let n = fu.unnamed.len();
            let binds: Vec<_> = (0..n).map(|i| format_ident!("__{}", i)).collect();
            let transformed = LitStr::new(&transform_positional(&lit.value()), lit.span());
            display_arms.push(quote! {
                Self::#vid( #(#binds),* ) => ::core::write!(__f, #transformed),
            });
            if let Some(idx) = fu.unnamed.iter().position(|f| is_source(&f.attrs)) {
                let pat: Vec<_> = (0..n)
                    .map(|i| if i == idx { quote!(__s) } else { quote!(_) })
                    .collect();
                source_arms.push(quote! {
                    Self::#vid( #(#pat),* ) =>
                        ::core::option::Option::Some(__s.as_dyn_error()),
                });
            }
            if n == 1 && has_from(&fu.unnamed[0].attrs) {
                let ty = &fu.unnamed[0].ty;
                from_impls.push(quote! {
                    impl ::core::convert::From<#ty> for #name {
                        fn from(__v: #ty) -> Self { #name::#vid(__v) }
                    }
                });
            }
        }
        Fields::Named(fields) => {
            let idents: Vec<_> = fields
                .named
                .iter()
                .map(|f| f.ident.clone().unwrap())
                .collect();
            display_arms.push(quote! {
                Self::#vid { #(#idents),* } => ::core::write!(__f, #lit),
            });
            if let Some(src) = fields.named.iter().find(|f| is_source(&f.attrs)) {
                let sid = src.ident.as_ref().unwrap();
                source_arms.push(quote! {
                    Self::#vid { #sid, .. } =>
                        ::core::option::Option::Some(#sid.as_dyn_error()),
                });
            }
        }
    }
}

// ---- structs -------------------------------------------------------------

fn expand_struct(input: &DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let s = match &input.data {
        Data::Struct(s) => s,
        _ => unreachable!(),
    };
    let spec = error_display(&input.attrs).ok_or_else(|| {
        syn::Error::new_spanned(
            name,
            "struct needs #[error(\"...\")] or #[error(transparent)]",
        )
    })?;

    let (display_body, source_body) = match spec {
        ErrorDisplay::Transparent => match &s.fields {
            Fields::Unnamed(fu) if fu.unnamed.len() == 1 => (
                quote! { ::core::fmt::Display::fmt(&self.0, __f) },
                quote! { ::core::option::Option::Some(self.0.as_dyn_error()) },
            ),
            Fields::Named(fields) if fields.named.len() == 1 => {
                let id = fields.named[0].ident.as_ref().unwrap();
                (
                    quote! { ::core::fmt::Display::fmt(&self.#id, __f) },
                    quote! { ::core::option::Option::Some(self.#id.as_dyn_error()) },
                )
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    name,
                    "#[error(transparent)] requires exactly one field",
                ))
            }
        },
        ErrorDisplay::Format(lit) => match &s.fields {
            Fields::Unit => (
                quote! { ::core::write!(__f, #lit) },
                quote! { ::core::option::Option::None },
            ),
            Fields::Named(fields) => {
                let idents: Vec<_> = fields
                    .named
                    .iter()
                    .map(|f| f.ident.clone().unwrap())
                    .collect();
                let src = fields
                    .named
                    .iter()
                    .find(|f| is_source(&f.attrs))
                    .map(|f| f.ident.clone().unwrap());
                let source_body = match src {
                    Some(id) => quote! { ::core::option::Option::Some(self.#id.as_dyn_error()) },
                    None => quote! { ::core::option::Option::None },
                };
                (
                    quote! { let Self { #(#idents),* } = self; ::core::write!(__f, #lit) },
                    source_body,
                )
            }
            Fields::Unnamed(fu) => {
                let n = fu.unnamed.len();
                let binds: Vec<_> = (0..n).map(|i| format_ident!("__{}", i)).collect();
                let transformed = LitStr::new(&transform_positional(&lit.value()), lit.span());
                let source_body = match fu.unnamed.iter().position(|f| is_source(&f.attrs)) {
                    Some(idx) => {
                        let index = syn::Index::from(idx);
                        quote! { ::core::option::Option::Some(self.#index.as_dyn_error()) }
                    }
                    None => quote! { ::core::option::Option::None },
                };
                (
                    quote! { let Self( #(#binds),* ) = self; ::core::write!(__f, #transformed) },
                    source_body,
                )
            }
        },
    };

    Ok(quote! {
        impl ::core::fmt::Display for #name {
            #[allow(unused_variables)]
            fn fmt(&self, __f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                #display_body
            }
        }

        impl ::std::error::Error for #name {
            fn source(&self) -> ::core::option::Option<&(dyn ::std::error::Error + 'static)> {
                #source_body
            }
        }
    })
}

/// Rewrites positional `{0}` placeholders to `{__0}` so they capture the
/// pattern-bound tuple fields (`__0`, `__1`, …) via inline formatting.
fn transform_positional(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                out.push('{');
                out.push('{');
                chars.next();
            }
            '}' if chars.peek() == Some(&'}') => {
                out.push('}');
                out.push('}');
                chars.next();
            }
            '{' => {
                out.push('{');
                let mut digits = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() {
                        digits.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if !digits.is_empty() {
                    out.push_str("__");
                    out.push_str(&digits);
                }
            }
            _ => out.push(c),
        }
    }
    out
}
