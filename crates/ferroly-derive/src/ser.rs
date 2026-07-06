//! `#[derive(Encode)]` expansion.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields};

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
                    return Err(err(name, "Encode supports only structs with named fields"));
                }
            };
            // Resolve each field's `(ident, wire key, skip_none)` up front so
            // malformed/unknown `#[ferroly(...)]` attributes surface as errors.
            let plan: Vec<(syn::Ident, String, bool)> = fields
                .iter()
                .map(|f| {
                    let id = f.ident.clone().unwrap();
                    let ma = member_attrs(&f.attrs)?;
                    let key = ma.rename.unwrap_or_else(|| {
                        apply_rename_all(&id.to_string(), cattrs.rename_all.as_deref())
                    });
                    Ok((id, key, ma.skip_none))
                })
                .collect::<syn::Result<Vec<_>>>()?;
            // `encode()` — builds a Value (used by XML/YAML and dynamic paths).
            let pushes = plan.iter().map(|(id, key, skip_none)| {
                if *skip_none {
                    quote! {
                        if let ::core::option::Option::Some(__v) = &self.#id {
                            __fields.push((#key.to_string(), ::ferroly::codec::Encode::encode(__v)));
                        }
                    }
                } else {
                    quote! {
                        __fields.push((#key.to_string(), ::ferroly::codec::Encode::encode(&self.#id)));
                    }
                }
            });
            // `encode_to()` — streams straight to the Encoder, no Value built.
            let entries = plan.iter().map(|(id, key, skip_none)| {
                if *skip_none {
                    quote! {
                        if let ::core::option::Option::Some(__v) = &self.#id {
                            ::ferroly::codec::Encoder::map_entry(__e, #key, __v);
                        }
                    }
                } else {
                    quote! {
                        ::ferroly::codec::Encoder::map_entry(__e, #key, &self.#id);
                    }
                }
            });
            let field_count = fields.len();
            Ok(quote! {
                impl ::ferroly::codec::Encode for #name {
                    fn encode(&self) -> ::ferroly::codec::Value {
                        let mut __fields: ::std::vec::Vec<(::std::string::String, ::ferroly::codec::Value)> =
                            ::std::vec::Vec::new();
                        #(#pushes)*
                        ::ferroly::codec::Value::Object(__fields)
                    }
                    fn encode_to<__E: ::ferroly::codec::Encoder>(&self, __e: &mut __E) {
                        __e.begin_map(#field_count);
                        #(#entries)*
                        __e.end_map();
                    }
                }
            })
        }
        Data::Enum(e) => {
            let variants: Vec<_> = e
                .variants
                .iter()
                .map(|v| {
                    if !matches!(v.fields, Fields::Unit) {
                        return Err(err(&v.ident, "Encode enums support only unit variants"));
                    }
                    let vid = &v.ident;
                    let ma = member_attrs(&v.attrs)?;
                    let key = ma.rename.unwrap_or_else(|| {
                        apply_rename_all(&vid.to_string(), cattrs.rename_all.as_deref())
                    });
                    Ok((vid.clone(), key))
                })
                .collect::<syn::Result<Vec<_>>>()?;
            let arms = variants
                .iter()
                .map(|(vid, key)| quote! { Self::#vid => ::ferroly::codec::Value::Str(#key.to_string()), });
            let to_arms = variants
                .iter()
                .map(|(vid, key)| quote! { Self::#vid => ::ferroly::codec::Encoder::encode_str(__e, #key), });
            Ok(quote! {
                impl ::ferroly::codec::Encode for #name {
                    fn encode(&self) -> ::ferroly::codec::Value {
                        match self { #(#arms)* }
                    }
                    fn encode_to<__E: ::ferroly::codec::Encoder>(&self, __e: &mut __E) {
                        match self { #(#to_arms)* }
                    }
                }
            })
        }
        Data::Union(_) => Err(err(name, "Encode cannot be derived for unions")),
    }
}

fn err<T: quote::ToTokens>(tokens: T, msg: &str) -> syn::Error {
    syn::Error::new_spanned(tokens, msg)
}
