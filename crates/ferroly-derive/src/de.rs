//! `#[derive(Decode)]` expansion.
//!
//! Two bodies are generated per type: `decode` (reads a `Value`) and
//! `decode_from` (pulls from a `Decoder`). Shapes that must look at the data
//! before choosing a variant (internally tagged, untagged, and externally
//! tagged enums mixing unit and data variants) pull one `Value` in
//! `decode_from` and reuse `decode`, because a `Decoder` cannot peek.
//!
//! The content generators return an expression that evaluates to the built
//! value and reports failure with `?` / `return Err(..)`, so they must be
//! placed in a function or closure whose error type is `CodecError`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::ext::IdentExt;
use syn::DeriveInput;

use crate::model::{Body, Container, Derive, Field, FieldSet, Repr, Style, Variant};

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let c = Container::parse(&input)?;
    let name = &c.ident;
    let generics = c.generics_for(Derive::Decode);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let (decode, decode_from) = match &c.body {
        Body::Struct(fs) => {
            let this = quote! { Self };
            let value = value_content(fs, &this, &quote! { __value }, c.deny_unknown_fields, None);
            let stream = stream_content(fs, &this, c.deny_unknown_fields);
            (
                quote! { ::core::result::Result::Ok(#value) },
                quote! { ::core::result::Result::Ok(#stream) },
            )
        }
        Body::Enum(variants, repr) => expand_enum(&c, variants, repr),
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::ferroly::codec::Decode for #name #ty_generics #where_clause {
            fn decode(
                __value: &::ferroly::codec::Value,
            ) -> ::core::result::Result<Self, ::ferroly::codec::CodecError> {
                #decode
            }
            fn decode_from<__D: ::ferroly::codec::Decoder>(
                __d: &mut __D,
            ) -> ::core::result::Result<Self, ::ferroly::codec::CodecError> {
                #decode_from
            }
        }
    })
}

fn expand_enum(c: &Container, variants: &[Variant], repr: &Repr) -> (TokenStream, TokenStream) {
    let deny = c.deny_unknown_fields;
    let is_unit = |v: &Variant| v.fields.style == Style::Unit;
    let path = |v: &Variant| {
        let vid = &v.ident;
        quote! { Self::#vid }
    };
    // Pull one `Value`, then decide — for shapes a `Decoder` cannot peek at.
    let via_value = quote! {
        let __value = ::ferroly::codec::Decoder::decode_value(__d)?;
        <Self as ::ferroly::codec::Decode>::decode(&__value)
    };

    match repr {
        // Unit-only enums are plain strings on both paths.
        Repr::External if variants.iter().all(is_unit) => {
            let arms: Vec<_> = variants
                .iter()
                .map(|v| {
                    let (names, path) = (v.names(), path(v));
                    quote! { #(#names)|* => ::core::result::Result::Ok(#path), }
                })
                .collect();
            (
                quote! {
                    let __s = __value
                        .as_str()
                        .ok_or_else(|| ::ferroly::codec::CodecError::expected("string"))?;
                    match __s {
                        #(#arms)*
                        __other => ::core::result::Result::Err(
                            ::ferroly::codec::CodecError::unknown_variant(__other)),
                    }
                },
                quote! {
                    let __s = ::ferroly::codec::Decoder::decode_string(__d)?;
                    match __s.as_str() {
                        #(#arms)*
                        __other => ::core::result::Result::Err(
                            ::ferroly::codec::CodecError::unknown_variant(__other)),
                    }
                },
            )
        }
        Repr::External => {
            let all_data = !variants.iter().any(is_unit);
            let shape = if all_data {
                "single-key object"
            } else {
                "string or single-key object"
            };
            let str_arms = variants.iter().map(|v| {
                let (names, path) = (v.names(), path(v));
                if is_unit(v) {
                    quote! { #(#names)|* => ::core::result::Result::Ok(#path), }
                } else {
                    quote! {
                        #(#names)|* => ::core::result::Result::Err(
                            ::ferroly::codec::CodecError::expected(
                                "single-key object for a data-carrying variant")),
                    }
                }
            });
            let object_arms = variants.iter().map(|v| {
                let (names, path) = (v.names(), path(v));
                if is_unit(v) {
                    quote! {
                        #(#names)|* => match __v {
                            ::ferroly::codec::Value::Null => ::core::result::Result::Ok(#path),
                            _ => ::core::result::Result::Err(
                                ::ferroly::codec::CodecError::expected("null for a unit variant")),
                        },
                    }
                } else {
                    let content = value_content(&v.fields, &path, &quote! { __v }, deny, None);
                    quote! { #(#names)|* => ::core::result::Result::Ok(#content), }
                }
            });
            let decode = quote! {
                match __value {
                    ::ferroly::codec::Value::Str(__s) => match __s.as_str() {
                        #(#str_arms)*
                        __other => ::core::result::Result::Err(
                            ::ferroly::codec::CodecError::unknown_variant(__other)),
                    },
                    ::ferroly::codec::Value::Object(__o) if __o.len() == 1usize => {
                        let (__k, __v) = &__o[0usize];
                        match __k.as_str() {
                            #(#object_arms)*
                            __other => ::core::result::Result::Err(
                                ::ferroly::codec::CodecError::unknown_variant(__other)),
                        }
                    }
                    _ => ::core::result::Result::Err(
                        ::ferroly::codec::CodecError::expected(#shape)),
                }
            };
            let decode_from = if all_data {
                // Every variant is `{"Name": content}`, so the content can be
                // streamed straight out of the single map entry.
                let arms = variants.iter().map(|v| {
                    let names = v.names();
                    let content = stream_content(&v.fields, &path(v), deny);
                    quote! {
                        #(#names)|* => {
                            __out = ::core::option::Option::Some(#content);
                        }
                    }
                });
                quote! {
                    let mut __out: ::core::option::Option<Self> = ::core::option::Option::None;
                    ::ferroly::codec::Decoder::read_map(__d, |__d, __k| {
                        if ::core::option::Option::is_some(&__out) {
                            return ::core::result::Result::Err(
                                ::ferroly::codec::CodecError::expected(#shape));
                        }
                        match __k {
                            #(#arms)*
                            __other => {
                                return ::core::result::Result::Err(
                                    ::ferroly::codec::CodecError::unknown_variant(__other));
                            }
                        }
                        ::core::result::Result::Ok(())
                    })?;
                    match __out {
                        ::core::option::Option::Some(__x) => ::core::result::Result::Ok(__x),
                        ::core::option::Option::None => ::core::result::Result::Err(
                            ::ferroly::codec::CodecError::expected(#shape)),
                    }
                }
            } else {
                via_value
            };
            (decode, decode_from)
        }
        Repr::Internal(tag) => {
            let arms = variants.iter().map(|v| {
                let names = v.names();
                let content =
                    value_content(&v.fields, &path(v), &quote! { __value }, deny, Some(tag));
                quote! { #(#names)|* => ::core::result::Result::Ok(#content), }
            });
            let decode = quote! {
                let __obj = __value
                    .as_object()
                    .ok_or_else(|| ::ferroly::codec::CodecError::expected("object"))?;
                // Last occurrence wins, as for every other key.
                let mut __found: ::core::option::Option<&::ferroly::codec::Value> =
                    ::core::option::Option::None;
                for __entry in __obj {
                    if __entry.0 == #tag {
                        __found = ::core::option::Option::Some(&__entry.1);
                    }
                }
                let __tag = match __found {
                    ::core::option::Option::Some(::ferroly::codec::Value::Str(__s)) => {
                        __s.as_str()
                    }
                    ::core::option::Option::Some(_) => {
                        return ::core::result::Result::Err(
                            ::ferroly::codec::CodecError::expected("string tag"));
                    }
                    ::core::option::Option::None => {
                        return ::core::result::Result::Err(
                            ::ferroly::codec::CodecError::missing_field(#tag));
                    }
                };
                match __tag {
                    #(#arms)*
                    __other => ::core::result::Result::Err(
                        ::ferroly::codec::CodecError::unknown_variant(__other)),
                }
            };
            (decode, via_value)
        }
        Repr::Untagged => {
            // Try each variant in declaration order; the first that decodes wins.
            let attempts = variants.iter().map(|v| {
                let content = value_content(&v.fields, &path(v), &quote! { __value }, deny, None);
                quote! {
                    let __attempt = || -> ::core::result::Result<Self, ::ferroly::codec::CodecError> {
                        ::core::result::Result::Ok(#content)
                    };
                    if let ::core::result::Result::Ok(__x) = __attempt() {
                        return ::core::result::Result::Ok(__x);
                    }
                }
            });
            let message = format!(
                "data did not match any variant of untagged enum {}",
                c.ident.unraw()
            );
            let decode = quote! {
                #(#attempts)*
                ::core::result::Result::Err(::ferroly::codec::CodecError::Message(
                    ::std::string::String::from(#message)))
            };
            (decode, via_value)
        }
    }
}

fn err_expected(what: &str) -> TokenStream {
    quote! {
        return ::core::result::Result::Err(::ferroly::codec::CodecError::expected(#what))
    }
}

fn array_of(len: usize) -> String {
    match len {
        1 => "array of 1 element".to_string(),
        n => format!("array of {n} elements"),
    }
}

/// `let mut __sN: Option<FieldTy> = None;`
fn slot_decl(f: &Field) -> TokenStream {
    let (slot, ty) = (&f.slot, &f.ty);
    quote! { let mut #slot: ::core::option::Option<#ty> = ::core::option::Option::None; }
}

/// The final value of a named field, from its slot (or `__rest` for `flatten`).
fn build_field(f: &Field) -> TokenStream {
    let slot = &f.slot;
    if f.skip {
        return f.default_expr();
    }
    if f.flatten {
        return if f.option {
            // An optional flattened group is simply absent when it does not decode.
            quote! {
                match ::ferroly::codec::Decode::decode(&__rest) {
                    ::core::result::Result::Ok(__x) => __x,
                    ::core::result::Result::Err(_) => ::core::option::Option::None,
                }
            }
        } else {
            quote! { ::ferroly::codec::Decode::decode(&__rest)? }
        };
    }
    if f.default.is_some() {
        let default = f.default_expr();
        return quote! {
            match #slot {
                ::core::option::Option::Some(__x) => __x,
                ::core::option::Option::None => #default,
            }
        };
    }
    if f.option {
        return quote! { ::core::option::Option::flatten(#slot) };
    }
    if f.skip_none {
        // `skip_none` proves the type is an `Option` even when it is an alias.
        return quote! {
            match #slot {
                ::core::option::Option::Some(__x) => __x,
                ::core::option::Option::None => ::core::option::Option::None,
            }
        };
    }
    let key = &f.key;
    quote! {
        #slot.ok_or_else(|| ::ferroly::codec::CodecError::missing_field(#key))?
    }
}

/// An expression decoding `path` from the `&Value` expression `src`. `tag`
/// names an internally-tagged enum's tag key: the content is then always an
/// object, and that key is accepted and ignored.
fn value_content(
    fs: &FieldSet,
    path: &TokenStream,
    src: &TokenStream,
    deny: bool,
    tag: Option<&str>,
) -> TokenStream {
    if tag.is_none() {
        match fs.style {
            Style::Unit => {
                let bad = err_expected("null");
                return quote! {
                    match #src {
                        ::ferroly::codec::Value::Null => #path,
                        _ => #bad,
                    }
                };
            }
            Style::Tuple => {
                if fs.newtype().is_some() {
                    return fs.construct(path, |f| {
                        if f.skip {
                            f.default_expr()
                        } else {
                            quote! { ::ferroly::codec::Decode::decode(#src)? }
                        }
                    });
                }
                let len = fs.active().count();
                let not_array = err_expected("array");
                let wrong_len = err_expected(&array_of(len));
                let mut next = 0usize;
                let built = fs.construct(path, |f| {
                    if f.skip {
                        return f.default_expr();
                    }
                    let i = next;
                    next += 1;
                    quote! { ::ferroly::codec::Decode::decode(&__arr[#i])? }
                });
                return quote! {{
                    let __arr = match #src {
                        ::ferroly::codec::Value::Array(__a) => __a,
                        _ => #not_array,
                    };
                    if __arr.len() != #len {
                        #wrong_len;
                    }
                    #built
                }};
            }
            Style::Named => {}
        }
    }

    let keyed: Vec<&Field> = fs.active().filter(|f| !f.flatten).collect();
    let flatten = fs.has_flatten();
    let decls = keyed.iter().map(|f| slot_decl(f));
    let arms = keyed.iter().map(|f| {
        let (names, slot) = (f.names(), &f.slot);
        quote! {
            #(#names)|* => #slot = ::core::option::Option::Some(
                ::ferroly::codec::Decode::decode(__v)?),
        }
    });
    let tag_arm = tag.map(|tag| quote! { #tag => {} });
    let unknown = if flatten {
        quote! {
            __rest.push((::core::clone::Clone::clone(__k), ::core::clone::Clone::clone(__v)))
        }
    } else if deny {
        unknown_field()
    } else {
        quote! { {} }
    };
    let scan = if keyed.is_empty() && !flatten && !deny {
        TokenStream::new()
    } else {
        quote! {
            for (__k, __v) in __obj {
                match __k.as_str() {
                    #(#arms)*
                    #tag_arm
                    _ => #unknown,
                }
            }
        }
    };
    let (rest_decl, rest_wrap) = rest(flatten);
    let built = fs.construct(path, build_field);
    quote! {{
        let __obj = #src
            .as_object()
            .ok_or_else(|| ::ferroly::codec::CodecError::expected("object"))?;
        #(#decls)*
        #rest_decl
        #scan
        #rest_wrap
        #built
    }}
}

/// An expression decoding `path` from the decoder `__d`. Mirrors
/// [`value_content`] (without a tag: tagged shapes go through a `Value`).
fn stream_content(fs: &FieldSet, path: &TokenStream, deny: bool) -> TokenStream {
    match fs.style {
        Style::Unit => {
            return quote! {{
                ::ferroly::codec::Decoder::decode_null(__d)?;
                #path
            }};
        }
        Style::Tuple => {
            if fs.newtype().is_some() {
                return fs.construct(path, |f| {
                    if f.skip {
                        f.default_expr()
                    } else {
                        quote! { ::ferroly::codec::Decode::decode_from(__d)? }
                    }
                });
            }
            let active: Vec<&Field> = fs.active().collect();
            let wrong_len = err_expected(&array_of(active.len()));
            let decls = active.iter().map(|f| slot_decl(f));
            let built = fs.construct(path, |f| {
                if f.skip {
                    return f.default_expr();
                }
                let slot = &f.slot;
                quote! {
                    match #slot {
                        ::core::option::Option::Some(__x) => __x,
                        ::core::option::Option::None => #wrong_len,
                    }
                }
            });
            if active.is_empty() {
                return quote! {{
                    ::ferroly::codec::Decoder::read_seq(__d, |_| { #wrong_len })?;
                    #built
                }};
            }
            let arms = active.iter().enumerate().map(|(i, f)| {
                let slot = &f.slot;
                quote! {
                    #i => #slot = ::core::option::Option::Some(
                        ::ferroly::codec::Decode::decode_from(__d)?),
                }
            });
            return quote! {{
                #(#decls)*
                let mut __i: usize = 0usize;
                ::ferroly::codec::Decoder::read_seq(__d, |__d| {
                    match __i {
                        #(#arms)*
                        _ => #wrong_len,
                    }
                    __i += 1usize;
                    ::core::result::Result::Ok(())
                })?;
                #built
            }};
        }
        Style::Named => {}
    }

    let keyed: Vec<&Field> = fs.active().filter(|f| !f.flatten).collect();
    let flatten = fs.has_flatten();
    let decls = keyed.iter().map(|f| slot_decl(f));
    let arms = keyed.iter().map(|f| {
        let (names, slot) = (f.names(), &f.slot);
        quote! {
            #(#names)|* => #slot = ::core::option::Option::Some(
                ::ferroly::codec::Decode::decode_from(__d)?),
        }
    });
    let unknown = if flatten {
        quote! {
            __rest.push((
                ::std::string::String::from(__k),
                ::ferroly::codec::Decoder::decode_value(__d)?,
            ))
        }
    } else if deny {
        unknown_field()
    } else {
        quote! { ::ferroly::codec::Decoder::skip_value(__d)? }
    };
    let (rest_decl, rest_wrap) = rest(flatten);
    let built = fs.construct(path, build_field);
    quote! {{
        #(#decls)*
        #rest_decl
        ::ferroly::codec::Decoder::read_map(__d, |__d, __k| {
            match __k {
                #(#arms)*
                _ => #unknown,
            }
            ::core::result::Result::Ok(())
        })?;
        #rest_wrap
        #built
    }}
}

/// The `deny_unknown_fields` failure for the key bound to `__k`.
fn unknown_field() -> TokenStream {
    quote! {
        return ::core::result::Result::Err(::ferroly::codec::CodecError::Message(
            ::std::format!("unknown field: {}", __k)))
    }
}

/// Declaration and final wrapping of `__rest`, the entries no keyed field
/// claimed, which `flatten` fields decode from.
fn rest(flatten: bool) -> (TokenStream, TokenStream) {
    if !flatten {
        return (TokenStream::new(), TokenStream::new());
    }
    (
        quote! {
            let mut __rest: ::std::vec::Vec<(::std::string::String, ::ferroly::codec::Value)> =
                ::std::vec::Vec::new();
        },
        quote! { let __rest = ::ferroly::codec::Value::Object(__rest); },
    )
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
    fn raw_identifier_fields_use_the_plain_key() {
        let out = generated(parse_quote! { struct S { r#type: u8 } });
        assert!(out.contains("\"type\" =>"), "{out}");
        assert!(out.contains("missing_field (\"type\")"), "{out}");
        assert!(!out.contains("\"r#type\""), "{out}");
        // The field itself is still addressed by its raw identifier.
        assert!(out.contains("r#type :"), "{out}");
    }

    #[test]
    fn aliases_share_the_field_arm() {
        let out = generated(parse_quote! {
            struct S { #[ferroly(alias = "old", alias = "older")] name: u8 }
        });
        assert!(out.contains("\"name\" | \"old\" | \"older\" =>"), "{out}");
    }

    #[test]
    fn generics_are_carried_onto_the_impl() {
        let out = generated(parse_quote! {
            struct Page<T, const N: usize> where T: Clone { items: Vec<T> }
        });
        assert!(
            out.contains(
                "impl < T , const N : usize > :: ferroly :: codec :: Decode for Page < T , N > \
                 where T : Clone , T : :: ferroly :: codec :: Decode"
            ),
            "{out}"
        );
    }

    #[test]
    fn unknown_keys_are_skipped_unless_denied() {
        let lax = generated(parse_quote! { struct S { a: u8 } });
        assert!(lax.contains("skip_value") && !lax.contains("unknown field"));
        let strict = generated(parse_quote! {
            #[ferroly(deny_unknown_fields)] struct S { a: u8 }
        });
        assert!(strict.contains("unknown field: {}") && !strict.contains("skip_value"));
    }

    #[test]
    fn only_all_data_external_enums_stream_their_content() {
        let data = generated(parse_quote! { enum E { A(u8), B { x: u8 } } });
        assert!(data.contains("read_map") && !data.contains("decode_value"));
        let mixed = generated(parse_quote! { enum E { A, B { x: u8 } } });
        assert!(mixed.contains("decode_value"));
        let unit = generated(parse_quote! { enum E { A, B } });
        assert!(unit.contains("decode_string") && !unit.contains("decode_value"));
    }

    #[test]
    fn tuple_lengths_are_spelled_out() {
        assert_eq!(array_of(0), "array of 0 elements");
        assert_eq!(array_of(1), "array of 1 element");
        assert_eq!(array_of(3), "array of 3 elements");
        let out = generated(parse_quote! { struct P(u8, u8); });
        assert!(out.contains("\"array of 2 elements\""), "{out}");
    }

    #[test]
    fn rejected_shapes_report_spanned_errors() {
        assert!(message(parse_quote! { union U { a: u8, b: u16 } }).contains("unions"));
        assert!(
            message(parse_quote! { #[ferroly(tag = "t")] enum E { A(u8) } })
                .contains("support only unit and struct variants")
        );
        assert!(
            message(parse_quote! { #[ferroly(rename_all = "nope")] struct S { a: u8 } })
                .contains("unknown rename_all rule")
        );
        assert!(
            message(parse_quote! { enum E { A, #[ferroly(rename = "A")] B } })
                .contains("duplicate variant key \"A\"")
        );
    }
}
