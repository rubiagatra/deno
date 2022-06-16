// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.
use once_cell::sync::Lazy;
use proc_macro::TokenStream;
use proc_macro2::Span;
use proc_macro2::TokenStream as TokenStream2;
use proc_macro_crate::crate_name;
use proc_macro_crate::FoundCrate;
use quote::quote;
use quote::ToTokens;
use regex::Regex;
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::FnArg;
use syn::GenericParam;
use syn::Ident;

// Identifier to the `deno_core` crate.
//
// If macro called in deno_core, `crate` is used.
// If macro called outside deno_core, `deno_core` OR the renamed
// version from Cargo.toml is used.
fn core_import() -> TokenStream2 {
  let found_crate =
    crate_name("deno_core").expect("deno_core not present in `Cargo.toml`");

  match found_crate {
    FoundCrate::Itself => {
      // TODO(@littledivy): This won't work for `deno_core` examples
      // since `crate` does not refer to `deno_core`.
      // examples must re-export deno_core to make this work
      // until Span inspection APIs are stabalized.
      //
      // https://github.com/rust-lang/rust/issues/54725
      quote!(crate)
    }
    FoundCrate::Name(name) => {
      let ident = Ident::new(&name, Span::call_site());
      quote!(#ident)
    }
  }
}

#[derive(Copy, Clone, Debug, Default)]
struct MacroArgs {
  is_unstable: bool,
  is_v8: bool,
}

impl syn::parse::Parse for MacroArgs {
  fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
    let vars =
      syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated(
        input,
      )?;
    let vars: Vec<_> = vars.iter().map(Ident::to_string).collect();
    let vars: Vec<_> = vars.iter().map(String::as_str).collect();
    for var in vars.iter() {
      if !["unstable", "v8"].contains(var) {
        return Err(syn::Error::new(
          input.span(),
          "Ops expect #[op] or #[op(unstable)]",
        ));
      }
    }
    Ok(Self {
      is_unstable: vars.contains(&"unstable"),
      is_v8: vars.contains(&"v8"),
    })
  }
}

#[proc_macro_attribute]
pub fn op(attr: TokenStream, item: TokenStream) -> TokenStream {
  let margs = syn::parse_macro_input!(attr as MacroArgs);
  let MacroArgs { is_unstable, is_v8 } = margs;
  let func = syn::parse::<syn::ItemFn>(item).expect("expected a function");
  let name = &func.sig.ident;
  let mut generics = func.sig.generics.clone();
  let scope_lifetime =
    syn::LifetimeDef::new(syn::Lifetime::new("'scope", Span::call_site()));
  if !generics.lifetimes().any(|def| *def == scope_lifetime) {
    generics
      .params
      .push(syn::GenericParam::Lifetime(scope_lifetime));
  }
  let type_params = exclude_lifetime_params(&func.sig.generics.params);
  let where_clause = &func.sig.generics.where_clause;

  // Preserve the original func as op_foo::call()
  let original_func = {
    let mut func = func.clone();
    func.sig.ident = quote::format_ident!("call");
    func
  };

  let core = core_import();

  let asyncness = func.sig.asyncness.is_some();
  let is_async = asyncness || is_future(&func.sig.output);
  let v8_body = if is_async {
    codegen_v8_async(&core, &func, margs, asyncness)
  } else {
    codegen_v8_sync(&core, &func, margs)
  };

  let docline = format!("Use `{name}::decl()` to get an op-declaration");
  // Generate wrapper
  quote! {
    #[allow(non_camel_case_types)]
    #[doc="Auto-generated by `deno_ops`, i.e: `#[op]`"]
    #[doc=""]
    #[doc=#docline]
    #[doc="you can include in a `deno_core::Extension`."]
    pub struct #name;

    #[doc(hidden)]
    impl #name {
      pub fn name() -> &'static str {
        stringify!(#name)
      }

      pub fn v8_fn_ptr #generics () -> #core::v8::FunctionCallback #where_clause {
        use #core::v8::MapFnTo;
        Self::v8_func::<#type_params>.map_fn_to()
      }

      pub fn decl #generics () -> #core::OpDecl #where_clause {
        #core::OpDecl {
          name: Self::name(),
          v8_fn_ptr: Self::v8_fn_ptr::<#type_params>(),
          enabled: true,
          is_async: #is_async,
          is_unstable: #is_unstable,
          is_v8: #is_v8,
        }
      }

      #[inline]
      #[allow(clippy::too_many_arguments)]
      #original_func

      pub fn v8_func #generics (
        scope: &mut #core::v8::HandleScope<'scope>,
        args: #core::v8::FunctionCallbackArguments,
        mut rv: #core::v8::ReturnValue,
      ) #where_clause {
        #v8_body
      }
    }
  }.into()
}

/// Generate the body of a v8 func for an async op
fn codegen_v8_async(
  core: &TokenStream2,
  f: &syn::ItemFn,
  margs: MacroArgs,
  asyncness: bool,
) -> TokenStream2 {
  let MacroArgs { is_v8, .. } = margs;
  let special_args = f
    .sig
    .inputs
    .iter()
    .map_while(|a| {
      (if is_v8 { scope_arg(a) } else { None }).or_else(|| opstate_arg(a))
    })
    .collect::<Vec<_>>();
  let rust_i0 = special_args.len();
  let args_head = special_args.into_iter().collect::<TokenStream2>();

  let (arg_decls, args_tail) = codegen_args(core, f, rust_i0, 1);
  let type_params = exclude_lifetime_params(&f.sig.generics.params);

  let (pre_result, mut result_fut) = match asyncness {
    true => (
      quote! {},
      quote! { Self::call::<#type_params>(#args_head #args_tail).await; },
    ),
    false => (
      quote! { let result_fut = Self::call::<#type_params>(#args_head #args_tail); },
      quote! { result_fut.await; },
    ),
  };
  let result_wrapper = match is_result(&f.sig.output) {
    true => {
      // Support `Result<impl Future<Output = Result<T, AnyError>> + 'static, AnyError>`
      if !asyncness {
        result_fut = quote! { result_fut; };
        quote! {
          let result = match result {
            Ok(fut) => fut.await,
            Err(e) => return (promise_id, op_id, #core::_ops::to_op_result::<()>(get_class, Err(e))),
          };
        }
      } else {
        quote! {}
      }
    }
    false => quote! { let result = Ok(result); },
  };

  quote! {
    use #core::futures::FutureExt;
    // SAFETY: #core guarantees args.data() is a v8 External pointing to an OpCtx for the isolates lifetime
    let ctx = unsafe {
      &*(#core::v8::Local::<#core::v8::External>::cast(args.data().unwrap_unchecked()).value()
      as *const #core::_ops::OpCtx)
    };
    let op_id = ctx.id;

    let promise_id = args.get(0);
    let promise_id = #core::v8::Local::<#core::v8::Integer>::try_from(promise_id)
      .map(|l| l.value() as #core::PromiseId)
      .map_err(#core::anyhow::Error::from);
    // Fail if promise id invalid (not an int)
    let promise_id: #core::PromiseId = match promise_id {
      Ok(promise_id) => promise_id,
      Err(err) => {
        #core::_ops::throw_type_error(scope, format!("invalid promise id: {}", err));
        return;
      }
    };

    #arg_decls

    let state = ctx.state.clone();

    // Track async call & get copy of get_error_class_fn
    let get_class = {
      let state = state.borrow();
      state.tracker.track_async(op_id);
      state.get_error_class_fn
    };

    #pre_result
    #core::_ops::queue_async_op(scope, async move {
      let result = #result_fut
      #result_wrapper
      (promise_id, op_id, #core::_ops::to_op_result(get_class, result))
    });
  }
}

fn scope_arg(arg: &FnArg) -> Option<TokenStream2> {
  if is_handle_scope(arg) {
    Some(quote! { scope, })
  } else {
    None
  }
}

fn opstate_arg(arg: &FnArg) -> Option<TokenStream2> {
  match arg {
    arg if is_rc_refcell_opstate(arg) => Some(quote! { ctx.state.clone(), }),
    arg if is_mut_ref_opstate(arg) => {
      Some(quote! { &mut ctx.state.borrow_mut(), })
    }
    _ => None,
  }
}

/// Generate the body of a v8 func for a sync op
fn codegen_v8_sync(
  core: &TokenStream2,
  f: &syn::ItemFn,
  margs: MacroArgs,
) -> TokenStream2 {
  let MacroArgs { is_v8, .. } = margs;
  let special_args = f
    .sig
    .inputs
    .iter()
    .map_while(|a| {
      (if is_v8 { scope_arg(a) } else { None }).or_else(|| opstate_arg(a))
    })
    .collect::<Vec<_>>();
  let rust_i0 = special_args.len();
  let args_head = special_args.into_iter().collect::<TokenStream2>();

  let (arg_decls, args_tail) = codegen_args(core, f, rust_i0, 0);
  let ret = codegen_sync_ret(core, &f.sig.output);
  let type_params = exclude_lifetime_params(&f.sig.generics.params);

  quote! {
    // SAFETY: #core guarantees args.data() is a v8 External pointing to an OpCtx for the isolates lifetime
    let ctx = unsafe {
      &*(#core::v8::Local::<#core::v8::External>::cast(args.data().unwrap_unchecked()).value()
      as *const #core::_ops::OpCtx)
    };

    #arg_decls

    let result = Self::call::<#type_params>(#args_head #args_tail);

    let op_state = &mut ctx.state.borrow();
    op_state.tracker.track_sync(ctx.id);

    #ret
  }
}

fn codegen_args(
  core: &TokenStream2,
  f: &syn::ItemFn,
  rust_i0: usize, // Index of first generic arg in rust
  v8_i0: usize,   // Index of first generic arg in v8/js
) -> (TokenStream2, TokenStream2) {
  let inputs = &f.sig.inputs.iter().skip(rust_i0).enumerate();
  let ident_seq: TokenStream2 = inputs
    .clone()
    .map(|(i, _)| format!("arg_{i}"))
    .collect::<Vec<_>>()
    .join(", ")
    .parse()
    .unwrap();
  let decls: TokenStream2 = inputs
    .clone()
    .map(|(i, arg)| {
      codegen_arg(core, arg, format!("arg_{i}").as_ref(), v8_i0 + i)
    })
    .collect();
  (decls, ident_seq)
}

fn codegen_arg(
  core: &TokenStream2,
  arg: &syn::FnArg,
  name: &str,
  idx: usize,
) -> TokenStream2 {
  let ident = quote::format_ident!("{name}");
  let pat = match arg {
    syn::FnArg::Typed(pat) => &pat.pat,
    _ => unreachable!(),
  };
  // Fast path if arg should be skipped
  if matches!(**pat, syn::Pat::Wild(_)) {
    return quote! { let #ident = (); };
  }
  // Otherwise deserialize it via serde_v8
  quote! {
    let #ident = args.get(#idx as i32);
    let #ident = match #core::serde_v8::from_v8(scope, #ident) {
      Ok(v) => v,
      Err(err) => {
        let msg = format!("Error parsing args at position {}: {}", #idx, #core::anyhow::Error::from(err));
        return #core::_ops::throw_type_error(scope, msg);
      }
    };
  }
}

fn codegen_sync_ret(
  core: &TokenStream2,
  output: &syn::ReturnType,
) -> TokenStream2 {
  if is_void(output) {
    return quote! {};
  }

  // Optimize Result<(), Err> to skip serde_v8 when Ok(...)
  let ok_block = if is_unit_result(output) {
    quote! {}
  } else {
    quote! {
      match #core::serde_v8::to_v8(scope, result) {
        Ok(ret) => rv.set(ret),
        Err(err) => #core::_ops::throw_type_error(
          scope,
          format!("Error serializing return: {}", #core::anyhow::Error::from(err)),
        ),
      };
    }
  };

  if !is_result(output) {
    return ok_block;
  }

  quote! {
    match result {
      Ok(result) => {
        #ok_block
      },
      Err(err) => {
        let err = #core::OpError::new(op_state.get_error_class_fn, err);
        rv.set(#core::serde_v8::to_v8(scope, err).unwrap());
      },
    };
  }
}

fn is_void(ty: impl ToTokens) -> bool {
  tokens(ty).is_empty()
}

fn is_result(ty: impl ToTokens) -> bool {
  let tokens = tokens(ty);
  if tokens.trim_start_matches("-> ").starts_with("Result <") {
    return true;
  }
  // Detect `io::Result<...>`, `anyhow::Result<...>`, etc...
  // i.e: Result aliases/shorthands which are unfortunately "opaque" at macro-time
  match tokens.find(":: Result <") {
    Some(idx) => !tokens.split_at(idx).0.contains('<'),
    None => false,
  }
}

/// Detects if a type is of the form Result<(), Err>
fn is_unit_result(ty: impl ToTokens) -> bool {
  is_result(&ty) && tokens(&ty).contains("Result < ()")
}

fn is_mut_ref_opstate(arg: &syn::FnArg) -> bool {
  static RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#": & mut (?:deno_core :: )?OpState$"#).unwrap());
  RE.is_match(&tokens(arg))
}

fn is_rc_refcell_opstate(arg: &syn::FnArg) -> bool {
  static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#": Rc < RefCell < (?:deno_core :: )?OpState > >$"#).unwrap()
  });
  RE.is_match(&tokens(arg))
}

fn is_handle_scope(arg: &syn::FnArg) -> bool {
  static RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#": & mut (?:deno_core :: )?v8 :: HandleScope(?: < '\w+ >)?$"#)
      .unwrap()
  });
  RE.is_match(&tokens(arg))
}

fn is_future(ty: impl ToTokens) -> bool {
  tokens(&ty).contains("impl Future < Output =")
}

fn tokens(x: impl ToTokens) -> String {
  x.to_token_stream().to_string()
}

fn exclude_lifetime_params(
  generic_params: &Punctuated<GenericParam, Comma>,
) -> Punctuated<GenericParam, Comma> {
  generic_params
    .iter()
    .filter(|t| !tokens(t).starts_with('\''))
    .cloned()
    .collect::<Punctuated<GenericParam, Comma>>()
}
