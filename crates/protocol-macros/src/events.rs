//! The `events!` declaration list, and everything one entry in it becomes.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Attribute, Error, Ident, LitStr, Result, Token, Type, braced};

pub(crate) struct Events(Vec<Event>);

impl Parse for Events {
    fn parse(input: ParseStream) -> Result<Self> {
        let events = Punctuated::<Event, Token![,]>::parse_terminated(input)?;
        Ok(Self(events.into_iter().collect()))
    }
}

/// One line of the declaration list.
struct Event {
    /// Whatever was written above it — a doc comment lands on both variants
    /// this becomes, which is the thing the tt-muncher could not accept.
    attrs: Vec<Attribute>,
    variant: Ident,
    name: LitStr,
    /// `@scoped`: it happens to one item rather than to the bar, so its
    /// `Kind` carries the item.
    scoped: bool,
    payload: Ident,
    fields: Vec<PayloadField>,
}

struct PayloadField {
    attrs: Vec<Attribute>,
    ident: Ident,
    ty: Type,
}

impl Parse for Event {
    fn parse(input: ParseStream) -> Result<Self> {
        let attrs = Attribute::parse_outer(input)?;
        let variant = input.parse()?;
        input.parse::<Token![=]>()?;
        let name = input.parse()?;
        let scoped = if input.peek(Token![@]) {
            input.parse::<Token![@]>()?;
            let scope: Ident = input.parse()?;
            if scope != "scoped" {
                return Err(Error::new_spanned(scope, "the only scope is `@scoped`"));
            }
            true
        } else {
            false
        };
        input.parse::<Token![=>]>()?;
        let payload = input.parse()?;
        let body;
        braced!(body in input);
        let fields = Punctuated::<PayloadField, Token![,]>::parse_terminated(&body)?;
        Ok(Self {
            attrs,
            variant,
            name,
            scoped,
            payload,
            fields: fields.into_iter().collect(),
        })
    }
}

impl Parse for PayloadField {
    fn parse(input: ParseStream) -> Result<Self> {
        Ok(Self {
            attrs: Attribute::parse_outer(input)?,
            ident: input.parse()?,
            ty: input.parse::<Token![:]>().and_then(|_| input.parse())?,
        })
    }
}

impl Event {
    /// Matches this kind's variant whatever it carries.
    fn pattern(&self) -> TokenStream {
        let variant = &self.variant;
        if self.scoped {
            quote!(Self::#variant(_))
        } else {
            quote!(Self::#variant)
        }
    }

    /// The wire-shaped, item-less kind — `Item = ()`, the only value an
    /// event's own name can produce.
    fn unscoped(&self) -> TokenStream {
        let variant = &self.variant;
        if self.scoped {
            quote!(Kind::#variant(()))
        } else {
            quote!(Kind::#variant)
        }
    }
}

#[expect(clippy::too_many_lines, reason = "one generated item after another")]
pub(crate) fn expand(events: &Events) -> TokenStream {
    let events = &events.0;

    let kind_variants = events.iter().map(|event| {
        let (attrs, variant) = (&event.attrs, &event.variant);
        if event.scoped {
            quote!(#(#attrs)* #variant(Item),)
        } else {
            quote!(#(#attrs)* #variant,)
        }
    });

    let payloads = events.iter().map(|event| {
        let payload = &event.payload;
        let fields = event.fields.iter().map(|field| {
            let (attrs, ident, ty) = (&field.attrs, &field.ident, &field.ty);
            quote!(#(#attrs)* pub #ident: #ty,)
        });
        quote! {
            #[derive(
                Debug,
                Clone,
                PartialEq,
                Default,
                ::serde::Serialize,
                ::serde::Deserialize,
                ::coolabah_protocol::EnvFields,
            )]
            pub struct #payload { #(#fields)* }
        }
    });

    let event_variants = events.iter().map(|event| {
        let (attrs, variant, payload) = (&event.attrs, &event.variant, &event.payload);
        quote!(#(#attrs)* #variant(#payload),)
    });

    let variants: Vec<&Ident> = events.iter().map(|event| &event.variant).collect();
    let unscoped: Vec<TokenStream> = events.iter().map(Event::unscoped).collect();
    let patterns: Vec<TokenStream> = events.iter().map(Event::pattern).collect();
    let names = events.iter().map(|event| &event.name);
    let payload_types = events.iter().map(|event| &event.payload);

    // A whole match arm at a time, which is what needed five helper macros
    // before: `macro_rules!` can expand to the pattern or to the expression
    // inside an arm, but never to both at once, and the two halves then had
    // to agree about a binding neither could name hygienically.
    let map_arms = events.iter().map(|event| {
        let variant = &event.variant;
        if event.scoped {
            quote!(Self::#variant(item) => Kind::#variant(f(item)),)
        } else {
            quote!(Self::#variant => Kind::#variant,)
        }
    });
    let item_arms = events.iter().map(|event| {
        let variant = &event.variant;
        if event.scoped {
            quote!(Self::#variant(item) => Some(item),)
        } else {
            quote!(Self::#variant => None,)
        }
    });

    quote! {
        /// What an item subscribes to: an event without its payload.
        ///
        /// Generic over `Item`, which only a handful of variants carry — the
        /// ones declared `@scoped` above, because they happen to one item
        /// rather than to the bar. `Item` is `()` on the wire: a config writes
        /// a bare `mouse.entered`, meaning "mine", and there is no item to
        /// name in that string — [`Kind::from_str`] can only ever produce
        /// `Kind<()>`. Whoever resolves "mine" to an actual item calls
        /// [`Kind::map`] once, at the one place both are known.
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Kind<Item = ()> {
            #(#kind_variants)*
            Custom(String),
        }

        #(#payloads)*

        /// Something that happened, with what it carries.
        #[derive(Debug, Clone, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        pub enum Event {
            #(#event_variants)*
            /// An event a config invented and triggers itself.
            Custom(Custom),
        }

        impl Event {
            /// What this is, for matching a subscription against.
            ///
            /// Always the wire-shaped, item-less `Kind<()>`: a payload never
            /// carries which item it happened to (that is decided by hit
            /// geometry, not by the event), so this cannot produce a scoped
            /// `Kind` and does not try to.
            #[must_use]
            pub fn kind(&self) -> Kind {
                match self {
                    #( Self::#variants(_) => #unscoped, )*
                    Self::Custom(custom) => Kind::Custom(custom.name.clone()),
                }
            }

            /// The payload's fields, under the names a script's environment
            /// uses.
            fn env_fields(&self) -> BTreeMap<Cow<'static, str>, String> {
                match self {
                    #( Self::#variants(data) => EnvFields::env_fields(data), )*
                    // A custom event's names came from whoever triggered it,
                    // so there is nothing to know at compile time.
                    Self::Custom(custom) => custom
                        .fields()
                        .into_iter()
                        .map(|(name, value)| (Cow::Owned(name.to_uppercase()), value))
                        .collect(),
                }
            }

            /// The payload's fields, as a script sees them.
            #[must_use]
            pub fn fields(&self) -> BTreeMap<String, String> {
                match self {
                    #( Self::#variants(data) => EnvFields::fields(data), )*
                    Self::Custom(custom) => custom.fields(),
                }
            }
        }

        impl<Item> Kind<Item> {
            /// Recasts the item this depends on, keeping everything else.
            ///
            /// An unscoped variant has no item to recast, so `f` never runs
            /// for one — it only fires for the handful declared `@scoped`.
            #[must_use]
            pub fn map<Other>(self, f: impl FnOnce(Item) -> Other) -> Kind<Other> {
                match self {
                    #(#map_arms)*
                    Self::Custom(name) => Kind::Custom(name),
                }
            }

            /// What this kind is bound to, for the variants declared
            /// `@scoped`, and `None` for every other.
            ///
            /// So that something holding a kind as a key can read the item
            /// back out of it without a match arm per variant — which is what
            /// makes an index over the *scopes* currently claimed possible at
            /// all, given `Item` is whatever the holder chose it to be.
            #[must_use]
            pub fn item(&self) -> Option<&Item> {
                match self {
                    #(#item_arms)*
                    Self::Custom(_) => None,
                }
            }
        }

        impl Kind {
            /// The name a config writes, and a script reads in `SENDER`.
            ///
            /// Defined only for the wire-shaped `Kind<()>` — not because a
            /// scoped `Kind<Entity>` prints differently, it never does, but
            /// because nothing ever needs its name: a claim is looked up by
            /// value, never by string, once it carries a real item.
            #[must_use]
            pub fn name(&self) -> &str {
                match self {
                    #( #patterns => #names, )*
                    Self::Custom(name) => name,
                }
            }

            /// Whether `event` is one of these.
            ///
            /// Generated alongside the variants so a new event cannot be added
            /// without the match arm that recognises it. Compares directly
            /// rather than going through [`Event::kind`], which would allocate
            /// a name for every custom event on every dispatch.
            #[must_use]
            pub fn matches(&self, event: &Event) -> bool {
                match (self, event) {
                    #( (#patterns, Event::#variants(_)) => true, )*
                    (Self::Custom(name), Event::Custom(custom)) => *name == custom.name,
                    _ => false,
                }
            }

            /// Every built-in, for validating a subscription and for `--help`.
            ///
            /// Scoped kinds come back as `Kind::Variant(())` — not a stand-in
            /// for a real claim, since a real one is `Kind<Entity>` and no
            /// value of that type is reachable from here. `()` is simply the
            /// only item a name on its own can mean.
            #[must_use]
            pub fn built_in() -> Vec<Kind> {
                vec![ #(#unscoped,)* ]
            }

            /// An event of this kind carrying nothing.
            ///
            /// What `--trigger` produces: a client naming an event knows the
            /// name, not the payload the source would have filled in. Only
            /// meaningful for the wire-shaped `Kind<()>` — a trigger names an
            /// event, not one already bound to an item.
            #[must_use]
            pub fn into_event(self) -> Event {
                match self {
                    #( #patterns => Event::#variants(#payload_types::default()), )*
                    Self::Custom(name) => Event::Custom(Custom::new(name)),
                }
            }
        }
    }
}
