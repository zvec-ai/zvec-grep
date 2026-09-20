use proc_macro::TokenStream;

mod file_formats;

/// Generates file formats and name lookups.
#[proc_macro]
pub fn file_formats(input: TokenStream) -> TokenStream {
    syn::parse::<file_formats::Catalog>(input)
        .and_then(|catalog| catalog.expand())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
