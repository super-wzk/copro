use proc_macro::TokenStream;
use quote::quote;
use syn::{
    Attribute, Data, DeriveInput, Error, LitStr, Path, Result, Type, parse_macro_input, parse_quote,
};

/// Derives `TryInto<copro_core::tool::HostedToolSpec>` for a hosted tool parameter type.
///
/// Required attributes:
///
/// ```ignore
/// #[derive(Serialize, CoproHostedTool)]
/// #[hosted_tool(kind = "image_generation")]
/// pub struct OpenAiImageGenerationTool {
///     pub partial_images: Option<u8>,
/// }
/// ```
#[proc_macro_derive(CoproHostedTool, attributes(hosted_tool))]
pub fn derive_copro_hosted_tool(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    match expand_hosted_tool(input) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

struct HostedToolArgs {
    kind: LitStr,
    core_crate: Path,
}

fn expand_hosted_tool(input: DeriveInput) -> Result<proc_macro2::TokenStream> {
    if !matches!(input.data, Data::Struct(_)) {
        return Err(Error::new(
            input.ident.span(),
            "CoproHostedTool can only be derived for structs",
        ));
    }

    let args = parse_hosted_tool_args(&input.attrs, input.ident.span())?;
    let tool = input.ident;
    let mut generics = input.generics;
    let (_, ty_generics, _) = generics.split_for_impl();
    let tool_ty: Type = parse_quote!(#tool #ty_generics);
    generics
        .make_where_clause()
        .predicates
        .push(parse_quote!(#tool_ty: ::serde::Serialize));
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
    let kind = args.kind;
    let core_crate = args.core_crate;

    Ok(quote! {
        impl #impl_generics ::std::convert::TryInto<#core_crate::tool::HostedToolSpec> for #tool #ty_generics #where_clause {
            type Error = #core_crate::error::Error;

            fn try_into(self) -> ::std::result::Result<#core_crate::tool::HostedToolSpec, Self::Error> {
                #core_crate::tool::HostedToolSpec::new(#kind).with_parameters(self)
            }
        }
    })
}

fn parse_hosted_tool_args(attrs: &[Attribute], span: proc_macro2::Span) -> Result<HostedToolArgs> {
    let mut saw_hosted_tool_attr = false;
    let mut kind = None;
    let mut core_crate = parse_quote!(::copro_core);
    let mut core_crate_set = false;

    for attr in attrs
        .iter()
        .filter(|attr| attr.path().is_ident("hosted_tool"))
    {
        saw_hosted_tool_attr = true;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("kind") {
                reject_duplicate(&kind, &meta, "kind")?;
                kind = Some(meta.value()?.parse::<LitStr>()?);
            } else if meta.path.is_ident("core_crate") {
                if core_crate_set {
                    return Err(meta.error("duplicate `core_crate`"));
                }
                core_crate = meta.value()?.parse::<Path>()?;
                core_crate_set = true;
            } else {
                return Err(meta
                    .error("unsupported hosted_tool attribute; expected `kind` or `core_crate`"));
            }

            Ok(())
        })?;
    }

    if !saw_hosted_tool_attr {
        return Err(Error::new(span, "missing #[hosted_tool(kind = ...)]"));
    }

    Ok(HostedToolArgs {
        kind: kind.ok_or_else(|| Error::new(span, "missing `kind` in #[hosted_tool(...)]"))?,
        core_crate,
    })
}

fn reject_duplicate<T>(
    current: &Option<T>,
    meta: &syn::meta::ParseNestedMeta<'_>,
    name: &str,
) -> Result<()> {
    if current.is_some() {
        return Err(meta.error(format!("duplicate `{name}`")));
    }

    Ok(())
}
