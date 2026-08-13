// Copyright 2025 FastLabs Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// Modified for k10s: backported from stacksafe-macro 1.0.3 while retaining
// version 0.1.4 for compatibility with stacksafe 0.1.4.

//! Procedural macro implementation for the `stacksafe` crate.

use proc_macro2::TokenStream;
use quote::{quote, ToTokens as _};
use syn::spanned::Spanned as _;
use syn::{parse_quote, Item, Path, ReturnType, Type};

#[proc_macro_attribute]
pub fn stacksafe(
    args: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    match expand(args.into(), item.into()) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn expand(args: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
    let mut crate_path: Option<Path> = None;
    let arg_parser = syn::meta::parser(|meta| {
        if !meta.path.is_ident("crate") {
            return Err(meta.error(format!(
                "unknown attribute parameter `{}`",
                meta.path.to_token_stream()
            )));
        }
        if crate_path.is_some() {
            return Err(meta.error("duplicate attribute parameter `crate`"));
        }
        crate_path = Some(meta.value()?.parse()?);
        Ok(())
    });
    syn::parse::Parser::parse2(arg_parser, args)?;

    let mut item_fn = match syn::parse2::<Item>(item)? {
        Item::Fn(item_fn) => item_fn,
        item => {
            return Err(syn::Error::new_spanned(
                item,
                "#[stacksafe] can only be applied to functions",
            ));
        }
    };

    if let Some(asyncness) = item_fn.sig.asyncness {
        return Err(syn::Error::new(
            asyncness.span(),
            "#[stacksafe] does not support async functions",
        ));
    }
    if let Some(constness) = item_fn.sig.constness {
        return Err(syn::Error::new(
            constness.span(),
            "#[stacksafe] does not support const functions",
        ));
    }

    let return_type = match &item_fn.sig.output {
        ReturnType::Type(_, ty) if matches!(**ty, Type::ImplTrait(_)) => None,
        return_type => Some(return_type),
    };
    let stacksafe_crate = crate_path.unwrap_or_else(|| parse_quote!(::stacksafe));
    let block = &item_fn.block;
    let wrapped_block = quote! {
        {
            #stacksafe_crate::internal::stacker::maybe_grow(
                #stacksafe_crate::get_minimum_stack_size(),
                #stacksafe_crate::get_stack_allocation_size(),
                #stacksafe_crate::internal::with_protected(move || #return_type { #block })
            )
        }
    };

    *item_fn.block = syn::parse2(wrapped_block)?;
    Ok(item_fn.into_token_stream())
}

#[cfg(test)]
mod tests {
    use quote::quote;

    use super::expand;

    #[test]
    fn expands_plain_and_impl_trait_functions() {
        assert!(expand(
            quote!(),
            quote!(
                fn plain() -> u8 {
                    1
                }
            )
        )
        .is_ok());
        assert!(expand(
            quote!(crate = crate),
            quote!(
                fn opaque() -> impl Iterator<Item = u8> {
                    [1].into_iter()
                }
            )
        )
        .is_ok());
    }

    fn flattened(tokens: proc_macro2::TokenStream) -> String {
        tokens
            .to_string()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    #[test]
    fn the_expansion_keeps_the_shape_stacksafe_0_1_4_expects() {
        let plain = flattened(
            expand(
                quote!(),
                quote!(
                    fn plain() -> u8 {
                        1
                    }
                ),
            )
            .expect("a plain function expands"),
        );
        assert!(
            plain.contains("::stacksafe::internal::stacker::maybe_grow(::stacksafe::get_minimum_stack_size(),::stacksafe::get_stack_allocation_size(),::stacksafe::internal::with_protected(move||->u8{{1}}))"),
            "{plain}"
        );

        let overridden = flattened(
            expand(
                quote!(crate = my_crate),
                quote!(
                    fn plain() {}
                ),
            )
            .expect("an overridden crate path expands"),
        );
        assert!(
            overridden.contains("my_crate::internal::stacker::maybe_grow"),
            "{overridden}"
        );

        let opaque = flattened(
            expand(
                quote!(),
                quote!(
                    fn opaque() -> impl Iterator<Item = u8> {
                        [1].into_iter()
                    }
                ),
            )
            .expect("an opaque return expands"),
        );
        assert!(opaque.contains("with_protected(move||{{"), "{opaque}");
        assert_eq!(
            opaque.matches("implIterator").count(),
            1,
            "an opaque return type is written once, on the signature: {opaque}"
        );
    }

    #[test]
    fn rejects_invalid_targets_and_parameters() {
        let cases = [
            expand(
                quote!(unknown = crate),
                quote!(
                    fn f() {}
                ),
            ),
            expand(
                quote!(crate = crate, crate = ::stacksafe),
                quote!(
                    fn f() {}
                ),
            ),
            expand(
                quote!(),
                quote!(
                    struct NotAFunction;
                ),
            ),
            expand(
                quote!(),
                quote!(
                    async fn f() {}
                ),
            ),
            expand(
                quote!(),
                quote!(
                    const fn f() {}
                ),
            ),
        ];
        assert!(cases.iter().all(Result::is_err));
    }
}
