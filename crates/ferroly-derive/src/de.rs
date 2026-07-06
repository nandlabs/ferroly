//! `#[derive(Decode)]` expansion.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, Type};

use crate::attrs::{container_attrs, member_attrs};
use crate::case::apply_rename_all;

pub fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let cattrs = container_attrs(&input.attrs)?;

    match &input.data {
        Data::Struct(s) => {
            let fields = match &s.fields {
                Fields::Named(n) => &n.named,
                _ => {
                    return Err(err(name, "Decode supports only structs with named fields"));
                }
            };
            // Per field: (ident, wire key, type, temp-slot ident, is-Option).
            let plan: Vec<(syn::Ident, String, Type, syn::Ident, bool)> = fields
                .iter()
                .map(|f| {
                    let id = f.ident.as_ref().unwrap();
                    let ma = member_attrs(&f.attrs)?;
                    let key = ma.rename.unwrap_or_else(|| {
                        apply_rename_all(&id.to_string(), cattrs.rename_all.as_deref())
                    });
                    let slot = format_ident!("__slot_{}", id);
                    Ok((id.clone(), key, f.ty.clone(), slot, is_option(&f.ty)))
                })
                .collect::<syn::Result<Vec<_>>>()?;

            // One `Option<FieldTy>` slot per field, filled in a single pass
            // (O(fields), not a lookup per field). Generated for both paths.
            let decls_a = plan.iter().map(slot_decl);
            let decls_b = plan.iter().map(slot_decl);
            let builds_a = plan.iter().map(build_field);
            let builds_b = plan.iter().map(build_field);
            // `decode` matches keys from an already-parsed Value object.
            let value_arms = plan.iter().map(|(_, key, _, slot, _)| {
                quote! {
                    #key => #slot = ::core::option::Option::Some(
                        ::ferroly::codec::Decode::decode(__v)?),
                }
            });
            // `decode_from` pulls each field's value straight from the Decoder.
            let from_arms = plan.iter().map(|(_, key, _, slot, _)| {
                quote! {
                    #key => #slot = ::core::option::Option::Some(
                        ::ferroly::codec::Decode::decode_from(__d)?),
                }
            });

            let scan = if plan.is_empty() {
                quote! {}
            } else {
                quote! {
                    for (__k, __v) in __obj {
                        match __k.as_str() {
                            #(#value_arms)*
                            _ => {}
                        }
                    }
                }
            };
            let read = if plan.is_empty() {
                quote! {
                    __d.read_map(|__d, _k| { __d.skip_value()?; ::core::result::Result::Ok(()) })?;
                }
            } else {
                quote! {
                    __d.read_map(|__d, __k| {
                        match __k {
                            #(#from_arms)*
                            _ => __d.skip_value()?,
                        }
                        ::core::result::Result::Ok(())
                    })?;
                }
            };

            Ok(quote! {
                impl ::ferroly::codec::Decode for #name {
                    fn decode(
                        __value: &::ferroly::codec::Value,
                    ) -> ::core::result::Result<Self, ::ferroly::codec::CodecError> {
                        let __obj = __value
                            .as_object()
                            .ok_or_else(|| ::ferroly::codec::CodecError::expected("object"))?;
                        #(#decls_a)*
                        #scan
                        ::core::result::Result::Ok(Self { #(#builds_a),* })
                    }
                    fn decode_from<__D: ::ferroly::codec::Decoder>(
                        __d: &mut __D,
                    ) -> ::core::result::Result<Self, ::ferroly::codec::CodecError> {
                        #(#decls_b)*
                        #read
                        ::core::result::Result::Ok(Self { #(#builds_b),* })
                    }
                }
            })
        }
        Data::Enum(e) => {
            let variants: Vec<(syn::Ident, String)> = e
                .variants
                .iter()
                .map(|v| {
                    if !matches!(v.fields, Fields::Unit) {
                        return Err(err(&v.ident, "Decode enums support only unit variants"));
                    }
                    let vid = v.ident.clone();
                    let ma = member_attrs(&v.attrs)?;
                    let key = ma.rename.unwrap_or_else(|| {
                        apply_rename_all(&vid.to_string(), cattrs.rename_all.as_deref())
                    });
                    Ok((vid, key))
                })
                .collect::<syn::Result<Vec<_>>>()?;
            let arms_a = variants
                .iter()
                .map(|(vid, key)| quote! { #key => ::core::result::Result::Ok(Self::#vid), });
            let arms_b = variants
                .iter()
                .map(|(vid, key)| quote! { #key => ::core::result::Result::Ok(Self::#vid), });
            Ok(quote! {
                impl ::ferroly::codec::Decode for #name {
                    fn decode(
                        __value: &::ferroly::codec::Value,
                    ) -> ::core::result::Result<Self, ::ferroly::codec::CodecError> {
                        let __s = __value
                            .as_str()
                            .ok_or_else(|| ::ferroly::codec::CodecError::expected("string"))?;
                        match __s {
                            #(#arms_a)*
                            __other => ::core::result::Result::Err(
                                ::ferroly::codec::CodecError::unknown_variant(__other)),
                        }
                    }
                    fn decode_from<__D: ::ferroly::codec::Decoder>(
                        __d: &mut __D,
                    ) -> ::core::result::Result<Self, ::ferroly::codec::CodecError> {
                        let __s = __d.decode_string()?;
                        match __s.as_str() {
                            #(#arms_b)*
                            __other => ::core::result::Result::Err(
                                ::ferroly::codec::CodecError::unknown_variant(__other)),
                        }
                    }
                }
            })
        }
        Data::Union(_) => Err(err(name, "Decode cannot be derived for unions")),
    }
}

/// `let mut __slot_x: Option<FieldTy> = None;`
fn slot_decl((_, _, ty, slot, _): &(syn::Ident, String, Type, syn::Ident, bool)) -> TokenStream {
    quote! { let mut #slot: ::core::option::Option<#ty> = ::core::option::Option::None; }
}

/// Builds a struct field from its filled slot: `flatten()` for `Option`, else
/// require it via `missing_field`.
fn build_field(
    (id, key, _, slot, opt): &(syn::Ident, String, Type, syn::Ident, bool),
) -> TokenStream {
    if *opt {
        quote! { #id: #slot.flatten() }
    } else {
        quote! {
            #id: #slot.ok_or_else(|| ::ferroly::codec::CodecError::missing_field(#key))?
        }
    }
}

/// Whether a type is spelled `Option<...>`.
fn is_option(ty: &Type) -> bool {
    if let Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            return seg.ident == "Option";
        }
    }
    false
}

fn err<T: quote::ToTokens>(tokens: T, msg: &str) -> syn::Error {
    syn::Error::new_spanned(tokens, msg)
}
