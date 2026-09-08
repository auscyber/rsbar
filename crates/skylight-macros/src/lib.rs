//! The attribute behind [`skylight`]'s main-thread proof.
//!
//! One macro, [`macro@main_thread`]. See its own documentation for what it
//! generates and why the proof is acquired rather than passed.

use proc_macro::TokenStream;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::visit_mut::{self, VisitMut};
use syn::{
    DeriveInput, Expr, ExprCall, FnArg, ForeignItem, Ident, ItemFn, ItemForeignMod, Meta, Path,
    Token, parse_macro_input, parse_quote,
};

/// Mints the process's one main-thread proof, in the one place it is free.
///
/// ```ignore
/// #[skylight::main(mtm)]
/// fn main() -> ExitCode {
///     // `mtm` is bound here, and it is the root: everything main-thread-only
///     // in the process is reached by passing it on, and nothing anywhere
///     // checks a thread.
/// }
/// ```
///
/// The signature stays exactly as written, the way `#[tokio::main]` leaves
/// one, and the binding is introduced into the body.
///
/// **The binding has no name you can write.** It is a fresh uuid per
/// expansion, so nothing in the body can refer to it, shadow it or forge it —
/// only the rewriting this attribute does can. For a body that merely *calls*
/// main-thread-only things that is the whole story; for one that has to hand
/// the proof to a function of its own, name that function in `also(..)` and
/// the call gets it:
///
/// ```ignore
/// #[skylight::main(also(build, Registry::new))]
/// fn main() {
///     let registry = Registry::new(config, waker);  // proof supplied
///     let app = build(inbox, settings, ..);         // and here
/// }
/// ```
///
/// Or say `pass` and the marker is in scope as `mtm`, for a body that has to
/// do something with it a rewrite cannot reach — put it in a struct field,
/// hand it to an `objc2` API, keep it in a local:
///
/// ```ignore
/// #[skylight::main(pass)]
/// fn main() {
///     let screens = NSScreen::screens(mtm);
/// }
/// ```
///
/// `mtm` rather than a name of the caller's choosing, so that the one nameable
/// binding in the crate is always spelled the same way and is always the same
/// thing.
///
/// # Why this needs no check
///
/// Rust runs `main` on the thread the process started on, and on macOS that is
/// the main thread — the one the window server talks to and the one
/// `pthread_main_np` answers for. So the proof is a fact about where `main`
/// is, not a question to ask at runtime, and this mints it without asking.
///
/// A `debug_assert` still holds it to that in debug builds, because the
/// attribute is only correct on an actual `main`: applied to some other
/// function it would be minting proof of nothing. Release builds emit nothing
/// at all.
///
/// # The point of doing it here
///
/// It is the difference between "this panics if you got it wrong" and "you
/// cannot get it wrong". With the root minted here and passed by value, every
/// main-thread-only call takes proof it was given, no function checks a
/// thread, and a caller with no proof does not compile. The one place that
/// could have been a runtime failure is the one place that cannot be wrong.
#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    let given = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let (also, pass) = match self::options(&given) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };
    let name = generated_marker();
    let passed = passed_marker(pass, &name);

    let mut function = parse_macro_input!(item as ItemFn);
    Supply {
        marker: &name,
        also: &also,
    }
    .visit_block_mut(&mut function.block);

    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = function;

    quote! {
        #(#attrs)*
        #vis #sig {
            debug_assert!(
                ::objc2::MainThreadMarker::new().is_some(),
                "`#[skylight::main]` is only correct on a real `main`",
            );
            // SAFETY: Rust runs `main` on the thread the process started on,
            // which on macOS is the main thread. See this attribute's own doc
            // comment; the assertion above holds it to that in debug builds.
            //
            // A `MainThread` rather than a bare marker: it reads this thread's
            // run loop once, here, where doing so is correct, and carries it
            // with the proof from then on. So nothing downstream has to ask
            // `CFRunLoop::current()` which loop it is on -- see
            // `skylight::MainThread`.
            let #name = ::skylight::MainThread::new(unsafe {
                ::objc2::MainThreadMarker::new_unchecked()
            });
            #passed
            #block
        }
    }
    .into()
}

/// The calls that take proof of the main thread and get it supplied, and
/// where in their arguments it goes.
///
/// Matched on the last one or two path segments, so `Window::new`,
/// `skylight::Window::new` and `crate::Window::new` all count. Everything else
/// in this crate either needs no proof or carries it in an argument it already
/// takes — `draw(&window, ..)` is proof by virtue of the window.
const SUPPLIED: &[(&[&str], Where)] = &[
    (&["Window", "new"], Where::Last),
    (&["batched"], Where::First),
    (&["without_implicit_animations"], Where::First),
];

/// Which argument the proof is, for a call it is supplied to.
///
/// Not a convention this could assume: proof reads best last after a plain
/// value (`Window::new(frame, proof)`) and first before a closure
/// (`batched(proof, || ..)`), because a closure wants to be the argument the
/// eye ends on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Where {
    First,
    Last,
}

/// Puts main-thread proof in front of the raw window server calls that need
/// it.
///
/// Goes on an `unsafe extern` block and applies to everything in it that does
/// not already carry proof: each such declaration is made private and given a
/// public wrapper of the same name that takes proof first. A declaration whose
/// first argument is a `ConnectionId` is left alone — that type is
/// main-thread-only itself, so having one to pass *is* the proof. Which is why the calls that need proof and
/// the calls that do not live in two separate blocks — the split is the
/// statement, rather than a marker to be read one line at a time.
///
/// ```ignore
/// #[skylight::main_thread_ffi]
/// unsafe extern "C" {
///     pub fn SLSDisableUpdate(cid: ConnectionId) -> CGError;
/// }
///
/// // A block of its own, and the comment above it says why.
/// unsafe extern "C" {
///     pub fn SLSGetScreenRectForWindow(..) -> CGError;
/// }
/// ```
///
/// So `ffi::SLSDisableUpdate(proof, cid)` is the only way to reach it, from
/// inside this crate as much as outside — which matters, because this crate
/// is where the 85 `unsafe` blocks that call these live.
///
/// # Why the block and not each declaration
///
/// Two reasons, and the second is the real one. An attribute on a foreign
/// item may only expand to foreign items, and a wrapper is not one — so a
/// per-declaration attribute could mark but not enforce. And a block sorted
/// into "needs the main thread" and "does not" says something a reader can
/// check at a glance, which a column of markers does not.
///
/// # Which calls should carry the marker
///
/// The ones the window server answers only for the thread it is talking to:
/// making, mutating, ordering and releasing windows, and suspending
/// compositing. Deliberately *not* reading a window's picture or its screen
/// rectangle: those run on the capture pool today, measured, and marking them
/// would put 115ms of every second back on the thread that draws.
#[proc_macro_attribute]
pub fn main_thread_ffi(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new_spanned(
            proc_macro2::TokenStream::from(attr),
            "`#[skylight::main_thread_ffi]` takes no arguments",
        )
        .to_compile_error()
        .into();
    }

    let mut block = parse_macro_input!(item as ItemForeignMod);
    let mut wrappers = Vec::new();

    for item in &mut block.items {
        let ForeignItem::Fn(declared) = item else {
            continue;
        };

        // A call whose first argument is the connection is already gated: a
        // `ConnectionId` is main-thread-only itself, so having one to pass is
        // the proof, and a second argument saying the same thing would be
        // noise. Left exactly as declared.
        if first_is_connection(&declared.sig) {
            continue;
        }

        let name = declared.sig.ident.clone();
        let hidden = Ident::new(&format!("__unproven_{name}"), name.span());
        let link = name.to_string();
        let vis = std::mem::replace(&mut declared.vis, syn::Visibility::Inherited);
        declared.sig.ident = hidden.clone();
        declared.attrs.push(parse_quote!(#[link_name = #link]));

        let docs: Vec<_> = declared
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("doc"))
            .cloned()
            .collect();
        let inputs = &declared.sig.inputs;
        let output = &declared.sig.output;
        let names: Vec<_> = inputs
            .iter()
            .filter_map(|arg| match arg {
                FnArg::Typed(typed) => Some(typed.pat.clone()),
                FnArg::Receiver(_) => None,
            })
            .collect();

        wrappers.push(quote! {
            // The wrapper keeps the C name, because it stands exactly where
            // the declaration used to; and it is one argument wider than a
            // declaration that may already have been wide.
            #[allow(non_snake_case, clippy::too_many_arguments, reason = "it is a C entry point")]
            #(#docs)*
            ///
            /// Main-thread-only: takes proof, which is why the raw
            /// declaration is private.
            ///
            /// # Safety
            ///
            /// The raw call's own contract, unchanged.
            #vis unsafe fn #name(
                _proof: impl ::skylight::MainThreadProof,
                #inputs
            ) #output {
                // SAFETY: the caller carries the raw call's contract, and the
                // thread it wanted is the one `_proof` is proof of.
                unsafe { #hidden(#(#names),*) }
            }
        });
    }

    quote! {
        #block
        #(#wrappers)*
    }
    .into()
}

/// Derives [`MainThreadProof`] for a type that cannot leave the main thread.
///
/// ```ignore
/// #[derive(MainThreadOnly)]
/// pub struct Window {
///     id: WindowId,
///     /// What makes it so. `OnlyOnMain` is neither `Send` nor `Sync`.
///     _main: skylight::OnlyOnMain,
/// }
/// ```
///
/// # What it checks
///
/// The `unsafe impl` this emits is sound only if the type really cannot reach
/// another thread, so the derive **proves that** rather than trusting it: it
/// emits a compile-time assertion that the type is not `Send`, and a type
/// that is gets a build error naming the problem instead of a silent
/// soundness hole.
///
/// So the two halves cannot drift apart. Add a `Send` impl later, or drop the
/// [`OnlyOnMain`] field, and the derive stops compiling — which is the whole
/// reason to have it rather than a hand-written `unsafe impl` beside a
/// hand-written field.
///
/// [`MainThreadProof`]: skylight::MainThreadProof
/// [`OnlyOnMain`]: skylight::OnlyOnMain
#[proc_macro_derive(MainThreadOnly)]
pub fn derive_main_thread_only(item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as DeriveInput);
    let name = &item.ident;
    let (impl_generics, ty_generics, where_clause) = item.generics.split_for_impl();

    quote! {
        const _: () = {
            /// Resolves to the inherent `IS_SEND` when the type is `Send`, and
            /// falls back to the trait's when it is not. Inherent associated
            /// constants win where they apply, which is what makes this a
            /// question the compiler will answer.
            struct Check<T>(::core::marker::PhantomData<T>);
            trait Fallback {
                const IS_SEND: bool = false;
            }
            impl<T> Fallback for Check<T> {}
            impl<T: Send> Check<T> {
                const IS_SEND: bool = true;
            }

            assert!(
                !Check::<#name #ty_generics>::IS_SEND,
                "a type deriving `MainThreadOnly` must not be `Send`: give it a \
                 `skylight::OnlyOnMain` field, and do not impl `Send` for it",
            );
        };

        // SAFETY: the assertion above is the invariant -- this type cannot
        // reach another thread, and its constructors take proof, so a value of
        // it existing at all is proof of the thread that built it.
        unsafe impl #impl_generics ::skylight::MainThreadProof for #name #ty_generics
            #where_clause
        {
            fn marker(&self) -> ::objc2::MainThreadMarker {
                // SAFETY: as above -- this value existing here is the proof.
                unsafe { ::objc2::MainThreadMarker::new_unchecked() }
            }
        }
    }
    .into()
}

/// Marks a function as belonging to the thread the window server talks to.
///
/// The signature grows a `proof` parameter, the body gets the marker bound as
/// `mtm`, and the calls inside that would have asked for proof stop asking.
/// Nothing is checked at runtime: being on the main thread is a fact the
/// compiler carries, and a caller that cannot show it does not compile.
///
/// # What it does
///
/// ```ignore
/// #[main_thread]
/// fn open(frame: CGRect) -> skylight::Result<Window> {
///     let window = Window::new(frame)?;   // proof supplied by the attribute
///     window.set_alpha(1.0)?;             // proof came with the value
///     skylight::batched(|| { /* .. */ }); // supplied too
///     Ok(window)
/// }
///
/// // and a caller passes the proof it was given:
/// let window = open(mtm, frame)?;
/// ```
///
/// The marker is also bound as `mtm` for anything the list below does not
/// cover — `#[main_thread(marker)]` names it something else.
///
/// The list is not closed: `#[main_thread(also(open_panel, Sheet::new))]`
/// adds calls of your own, whose proof is taken to be their last argument. It has to be
/// said per function rather than registered once, because a macro cannot keep
/// state a later crate's compilation would see — each crate is its own rustc
/// invocation with its own expansion, in an order nothing promises.
///
/// # What it rewrites, and the one thing to know about it
///
/// A fixed list: `Window::new`, `batched`, `without_implicit_animations` —
/// every call in this crate that takes proof and has no argument able to
/// carry it. Matching is **by name**, on the last one or two path segments,
/// because an attribute macro runs before name resolution and cannot ask what
/// a path means. A local `Window::new` of your own, inside an annotated
/// function, would be rewritten too. Within a codebase that imports
/// `skylight::Window` that has not come up, and the failure is a compile
/// error about arity rather than anything silent.
///
/// Method calls are never rewritten. They do not need to be: a
/// `skylight::Window` can only have been made on the main thread and cannot
/// leave it, so every method on one is already proof of where it is.
///
/// # Why the parameter, rather than a check
///
/// Rust has no effect system: a bound cannot be inferred from what a body
/// happens to call, and an attribute sees one item at a time, so it cannot
/// know which of a body's callees need proof either. An earlier version of
/// this made each annotated function acquire its own proof, which was tidy at
/// the call site and wrong in the way that matters — a worker calling one got
/// a panic rather than a compile error, and the requirement was invisible in
/// the signature.
///
/// Taking it instead makes the requirement propagate outward on its own:
/// every caller must have proof, so every caller is annotated too, all the
/// way up to `#[skylight::main]`, where it is minted for free. There is no
/// runtime check anywhere on that path — proof is a zero-sized type, so an
/// annotated call costs exactly what it cost before.
///
/// To call one *from* a worker, see `skylight::MainOnly::dispatch`, which
/// sends the work to the main thread rather than pretending to be there.
///
/// `async fn` is rejected: a marker cannot be held across an await point, and
/// a future resumed elsewhere would be holding proof of nothing.
#[proc_macro_attribute]
pub fn main_thread(attr: TokenStream, item: TokenStream) -> TokenStream {
    let given = parse_macro_input!(attr with Punctuated::<Meta, Token![,]>::parse_terminated);
    let (also, pass) = match self::options(&given) {
        Ok(parsed) => parsed,
        Err(err) => return err.to_compile_error().into(),
    };
    let name = generated_marker();
    let passed = passed_marker(pass, &name);
    let mut function = parse_macro_input!(item as ItemFn);

    if let Some(asyncness) = function.sig.asyncness {
        return syn::Error::new_spanned(
            asyncness,
            "`#[main_thread]` cannot be applied to an async function: the marker would not \
             survive an await",
        )
        .to_compile_error()
        .into();
    }

    Supply {
        marker: &name,
        also: &also,
    }
    .visit_block_mut(&mut function.block);

    demand(function, &name, &passed)
}

/// Grows the signature by a `proof` parameter and binds the marker from it.
///
/// The parameter goes after `self` and before everything else. Nothing is
/// checked at runtime and nothing is emitted for it: proof is a zero-sized
/// type, so an annotated function costs exactly what it cost unannotated.
fn demand(mut function: ItemFn, name: &Ident, passed: &proc_macro2::TokenStream) -> TokenStream {
    // `Copy`, because a body that hands the proof to two things would
    // otherwise move it into the first. Everything that is proof already is:
    // a marker, a reference, a connection id.
    let proof: FnArg = parse_quote!(proof: impl ::skylight::MainThreadProof + Copy);
    let at = usize::from(matches!(
        function.sig.inputs.first(),
        Some(FnArg::Receiver(_))
    ));
    function.sig.inputs.insert(at, proof);

    let ItemFn {
        attrs,
        vis,
        sig,
        block,
    } = function;

    quote! {
        #(#attrs)*
        #vis #sig {
            // The proof itself, not its marker: whatever the caller had is
            // what gets handed on, so a `MainThread`'s run loop survives every
            // hop rather than being flattened to a bare marker at the first
            // one.
            let #name = proof;
            let _ = &#name;
            #passed
            #block
        }
    }
    .into()
}

/// The attribute's arguments: which calls of the caller's own to supply the
/// proof to, and whether the body wants to see the marker itself.
fn options(given: &Punctuated<Meta, Token![,]>) -> syn::Result<(Vec<Vec<String>>, bool)> {
    let mut also = Vec::new();
    let mut pass = false;

    for option in given {
        match option {
            Meta::List(list) if list.path.is_ident("also") => {
                let paths =
                    list.parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)?;
                also.extend(paths.iter().map(segments));
            }
            Meta::Path(path) if path.is_ident("pass") => {
                pass = true;
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "expected `also(path, ..)` or `pass`; the marker's name is not the caller's to \
                     choose -- `pass` binds it as `mtm`",
                ));
            }
        }
    }

    Ok((also, pass))
}

/// What `pass` adds: `let mtm = <proof>.marker();`, or nothing.
///
/// Two bindings rather than one, and they are for different jobs. The
/// generated one holds the **proof** — for the daemon a `MainThread`, carrying
/// the run loop — and only the rewriting reaches it, so a call it supplies
/// keeps the loop. `mtm` holds the plain marker, because that is what a body
/// asking to see it wants: every `objc2` API takes a `MainThreadMarker`, and
/// unwrapping the proof at each of them was the noise `pass` exists to remove.
///
/// A body that needs the proof itself still has the `proof` parameter.
fn passed_marker(pass: bool, name: &Ident) -> proc_macro2::TokenStream {
    if pass {
        quote! {
            let mtm = ::skylight::MainThreadProof::marker(&#name);
        }
    } else {
        quote!()
    }
}

/// Appends the marker to every call in [`SUPPLIED`], and to the caller's own
/// `also` list.
struct Supply<'a> {
    marker: &'a Ident,
    also: &'a [Vec<String>],
}

impl VisitMut for Supply<'_> {
    fn visit_expr_call_mut(&mut self, call: &mut ExprCall) {
        // Inner calls first: an argument of a supplied call may be one too.
        visit_mut::visit_expr_call_mut(self, call);

        if let Expr::Path(path) = call.func.as_ref()
            && let Some(where_) = self.wanted(&path.path)
        {
            let marker = self.marker;
            // By reference: the proof may be a `MainThread`, which holds a
            // retained run loop and so is not `Copy`. A reference to one is,
            // and is proof in its own right.
            match where_ {
                Where::First => call.args.insert(0, syn::parse_quote!(&#marker)),
                Where::Last => call.args.push(syn::parse_quote!(&#marker)),
            }
        }
    }

    /// A method call named in `also` gets it too, first — which is where every
    /// signature this attribute generates puts it.
    fn visit_expr_method_call_mut(&mut self, call: &mut syn::ExprMethodCall) {
        visit_mut::visit_expr_method_call_mut(self, call);

        let named = call.method.to_string();
        if self
            .also
            .iter()
            .any(|wanted| wanted.last().is_some_and(|last| *last == named))
        {
            let marker = self.marker;
            call.args.insert(0, syn::parse_quote!(&#marker));
        }
    }

    /// A nested item has its own scope and its own opinion about threads; the
    /// marker is not in scope there and rewriting into it would not compile.
    fn visit_item_mut(&mut self, _item: &mut syn::Item) {}
}

impl Supply<'_> {
    /// Where this call wants its proof, if it wants one.
    fn wanted(&self, path: &Path) -> Option<Where> {
        SUPPLIED
            .iter()
            .find(|(wanted, _)| tail_matches(path, wanted))
            .map(|(_, where_)| *where_)
            .or_else(|| {
                self.also
                    .iter()
                    .any(|wanted| tail_matches_owned(path, wanted))
                    .then_some(Where::First)
            })
    }
}

/// Whether a declaration's first parameter is the window server connection.
fn first_is_connection(sig: &syn::Signature) -> bool {
    let Some(FnArg::Typed(first)) = sig.inputs.first() else {
        return false;
    };
    let syn::Type::Path(path) = &*first.ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "ConnectionId")
}

/// The name the marker is bound to inside an annotated body.
///
/// A fresh uuid per expansion, so it is not a name anyone can write: the only
/// thing that can reach the binding is the rewriting below, which is the point.
/// A body that has to hand the proof on by hand names the *parameter* —
/// `proof`, added by [`macro@main_thread`] — rather than the binding.
fn generated_marker() -> Ident {
    Ident::new(
        &format!("__mtm_{}", uuid::Uuid::new_v4().simple()),
        proc_macro2::Span::call_site(),
    )
}

/// A path's last segments, for matching against a wanted name.
fn segments(path: &Path) -> Vec<String> {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect()
}

/// Whether `path` ends with `wanted`, so `Window::new` matches
/// `skylight::Window::new`.
fn tail_matches(path: &Path, wanted: &[&str]) -> bool {
    let have = segments(path);
    have.len() >= wanted.len() && have[have.len() - wanted.len()..] == *wanted
}

/// The same, against a name the attribute was given rather than a constant.
fn tail_matches_owned(path: &Path, wanted: &[String]) -> bool {
    let borrowed: Vec<&str> = wanted.iter().map(String::as_str).collect();
    tail_matches(path, &borrowed)
}
