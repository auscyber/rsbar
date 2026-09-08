//! `#[derive(EnvFields)]` — a value's fields, flattened into the one
//! environment a shell can read.
//!
//! A shell has no nesting, so a nested struct or enum projects its own fields
//! up to the top level under their own names. For an enum that means the
//! *union* of every variant's fields, always present and empty where the
//! variant it got does not have one: a script tests `[ -z "$X" ]`, and a
//! variable that sometimes does not exist at all is worse than one that is
//! sometimes empty.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Error, Expr, Fields, Ident, Result, Type};

/// One name a value contributes, and where its value comes from.
struct Entry {
    /// The name a script reads, before upper-casing.
    name: String,
    /// What to render through `Field::to_field`.
    value: TokenStream,
    /// The type to project up as well, for a nested struct or enum.
    nested: Option<Type>,
}

impl Entry {
    fn insert(&self, upper: bool) -> TokenStream {
        let key = if upper {
            let name = self.name.to_ascii_uppercase();
            quote!(::std::borrow::Cow::Borrowed(#name))
        } else {
            let name = &self.name;
            quote!(::std::string::String::from(#name))
        };
        let value = &self.value;
        let project = self.nested.as_ref().map(|_| {
            let call = if upper {
                quote!(env_fields)
            } else {
                quote!(fields)
            };
            quote!(fields.extend(::rsbar_protocol::event::EnvFields::#call(#value));)
        });
        quote! {
            fields.insert(#key, ::rsbar_protocol::event::Field::to_field(#value));
            #project
        }
    }

    /// The same name with nothing in it — what a variant that does not carry
    /// this field leaves behind.
    fn blank(&self, upper: bool) -> TokenStream {
        let key = if upper {
            let name = self.name.to_ascii_uppercase();
            quote!(::std::borrow::Cow::Borrowed(#name))
        } else {
            let name = &self.name;
            quote!(::std::string::String::from(#name))
        };
        // A nested value's own names are not knowable from here, so they come
        // from the default of its type -- which, being derived, always spells
        // out its whole union whatever it holds.
        let project = self.nested.as_ref().map(|ty| {
            let call = if upper {
                quote!(env_fields)
            } else {
                quote!(fields)
            };
            quote! {
                fields.extend(
                    ::rsbar_protocol::event::EnvFields::#call(
                        &<#ty as ::std::default::Default>::default(),
                    )
                    .into_keys()
                    .map(|name| (name, ::std::string::String::new())),
                );
            }
        });
        quote! {
            fields.insert(#key, ::std::string::String::new());
            #project
        }
    }
}

pub(crate) fn expand(input: &DeriveInput) -> Result<TokenStream> {
    let (fields_body, env_body) = match &input.data {
        Data::Struct(data) => {
            let entries = entries(&data.fields, |ident| quote!(&self.#ident))?;
            (body(&entries, false), body(&entries, true))
        }
        Data::Enum(data) => (variants(data, false)?, variants(data, true)?),
        Data::Union(_) => {
            return Err(Error::new_spanned(
                &input.ident,
                "EnvFields is for a struct or an enum",
            ));
        }
    };

    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    Ok(quote! {
        impl #impl_generics ::rsbar_protocol::event::EnvFields for #ident #ty_generics
            #where_clause
        {
            fn fields(
                &self,
            ) -> ::std::collections::BTreeMap<::std::string::String, ::std::string::String> {
                #fields_body
            }

            fn env_fields(
                &self,
            ) -> ::std::collections::BTreeMap<
                ::std::borrow::Cow<'static, str>,
                ::std::string::String,
            > {
                #env_body
            }
        }
    })
}

/// A struct's fields, or one variant's.
fn entries(fields: &Fields, mut access: impl FnMut(&Ident) -> TokenStream) -> Result<Vec<Entry>> {
    match fields {
        Fields::Unit => Ok(Vec::new()),
        Fields::Unnamed(unnamed) => Err(Error::new_spanned(
            unnamed,
            "EnvFields needs named fields: a script reads a variable by name",
        )),
        Fields::Named(named) => named
            .named
            .iter()
            .map(|field| {
                let ident = field.ident.clone().expect("a named field has a name");
                let flatten = flattened(field)?;
                Ok(Entry {
                    name: ident.to_string().trim_start_matches("r#").to_owned(),
                    value: access(&ident),
                    nested: flatten.then(|| field.ty.clone()),
                })
            })
            .collect(),
    }
}

fn body(entries: &[Entry], upper: bool) -> TokenStream {
    if entries.is_empty() {
        return quote!(::std::collections::BTreeMap::new());
    }
    let inserts = entries.iter().map(|entry| entry.insert(upper));
    quote! {
        let mut fields = ::std::collections::BTreeMap::new();
        #(#inserts)*
        fields
    }
}

/// An enum's projection: every variant's names, then the ones this variant
/// actually has.
fn variants(data: &syn::DataEnum, upper: bool) -> Result<TokenStream> {
    let mut union: Vec<Entry> = Vec::new();
    let mut arms = Vec::new();

    for variant in &data.variants {
        let ident = &variant.ident;
        let bindings: Vec<Ident> = if let Fields::Named(named) = &variant.fields {
            named
                .named
                .iter()
                .map(|field| {
                    let name = field.ident.as_ref().expect("a named field has a name");
                    format_ident!("field_{name}")
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut bindings = bindings.into_iter();
        let held = entries(&variant.fields, |_| {
            let binding = bindings.next().expect("one binding per named field");
            quote!(#binding)
        })?;

        // A constant the variant implies rather than stores: on battery is
        // discharging by definition, and a script is better told `false` than
        // left an empty it would have to read the power source to interpret.
        let constants = constants(variant)?;
        let mut inserts: Vec<TokenStream> = held.iter().map(|e| e.insert(upper)).collect();
        for (name, value) in &constants {
            let entry = Entry {
                name: name.to_string(),
                value: quote!(&(#value)),
                nested: None,
            };
            inserts.push(entry.insert(upper));
        }

        for entry in held {
            if !union.iter().any(|seen| seen.name == entry.name) {
                union.push(Entry {
                    name: entry.name,
                    value: TokenStream::new(),
                    nested: entry.nested,
                });
            }
        }
        for (name, _) in constants {
            let name = name.to_string();
            if !union.iter().any(|seen| seen.name == name) {
                union.push(Entry {
                    name,
                    value: TokenStream::new(),
                    nested: None,
                });
            }
        }

        let pattern = if let Fields::Named(named) = &variant.fields {
            let pairs = named.named.iter().map(|field| {
                let name = field.ident.as_ref().expect("a named field has a name");
                let binding = format_ident!("field_{name}");
                quote!(#name: #binding)
            });
            quote!(Self::#ident { #(#pairs),* })
        } else {
            quote!(Self::#ident)
        };
        arms.push(quote!(#pattern => { #(#inserts)* }));
    }

    if union.is_empty() {
        return Ok(quote!(::std::collections::BTreeMap::new()));
    }
    let blanks = union.iter().map(|entry| entry.blank(upper));
    Ok(quote! {
        let mut fields = ::std::collections::BTreeMap::new();
        #(#blanks)*
        match self { #(#arms)* }
        fields
    })
}

fn flattened(field: &syn::Field) -> Result<bool> {
    let mut flatten = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("env") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("flatten") {
                flatten = true;
                Ok(())
            } else {
                Err(meta.error("the only option on a field is `flatten`"))
            }
        })?;
    }
    Ok(flatten)
}

fn constants(variant: &syn::Variant) -> Result<Vec<(Ident, Expr)>> {
    let mut constants = Vec::new();
    for attr in &variant.attrs {
        if !attr.path().is_ident("env") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let name = meta
                .path
                .get_ident()
                .cloned()
                .ok_or_else(|| meta.error("expected `name = value`"))?;
            constants.push((name, meta.value()?.parse()?));
            Ok(())
        })?;
    }
    Ok(constants)
}
