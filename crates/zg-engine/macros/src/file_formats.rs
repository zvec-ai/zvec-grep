use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    Ident, LitInt, LitStr, Result, Token, bracketed, parenthesized,
    parse::{Parse, ParseStream},
    punctuated::Punctuated,
};

struct Format {
    variant: Ident,
    id: LitInt,
    name: LitStr,
    categories: Punctuated<Ident, Token![,]>,
    extensions: Punctuated<LitStr, Token![,]>,
    file_names: Punctuated<LitStr, Token![,]>,
}

impl Format {
    fn validate(&self) -> Result<()> {
        if self.categories.is_empty() {
            return Err(syn::Error::new_spanned(
                &self.variant,
                "formats must have at least one category",
            ));
        }
        let mut categories = BTreeSet::new();
        for category in &self.categories {
            let name = category.to_string();
            let name = name.strip_prefix("r#").unwrap_or(&name);
            if !categories.insert(name.to_owned()) {
                return Err(syn::Error::new_spanned(
                    category,
                    "duplicate category in the same format",
                ));
            }
        }
        if categories.contains("Unknown") && categories.len() > 1 {
            return Err(syn::Error::new_spanned(
                &self.categories,
                "Unknown cannot be combined with specific categories",
            ));
        }
        let name = self.name.value();
        if name.is_empty()
            || !name.is_ascii()
            || name.bytes().any(|byte| {
                byte.is_ascii_uppercase()
                    || byte.is_ascii_whitespace()
                    || byte.is_ascii_control()
                    || matches!(byte, b'/' | b'\\')
            })
        {
            return Err(syn::Error::new_spanned(
                &self.name,
                "format names must be nonempty lowercase ASCII without whitespace, control characters, or path separators",
            ));
        }
        let mut extensions = BTreeSet::new();
        for extension in &self.extensions {
            let value = extension.value();
            if !value.is_ascii()
                || value.split('.').any(str::is_empty)
                || value.bytes().any(|byte| {
                    byte.is_ascii_uppercase()
                        || byte.is_ascii_whitespace()
                        || byte.is_ascii_control()
                        || matches!(byte, b'/' | b'\\')
                })
            {
                return Err(syn::Error::new_spanned(
                    extension,
                    "extensions must contain nonempty lowercase ASCII components separated by dots, without whitespace, control characters, or path separators",
                ));
            }
            if !extensions.insert(value) {
                return Err(syn::Error::new_spanned(
                    extension,
                    "duplicate extension in the same format",
                ));
            }
        }
        let mut file_names = BTreeSet::new();
        for name in &self.file_names {
            let value = name.value();
            if value.is_empty()
                || matches!(value.as_str(), "." | "..")
                || value
                    .chars()
                    .any(|ch| ch.is_control() || matches!(ch, '/' | '\\'))
            {
                return Err(syn::Error::new_spanned(
                    name,
                    "exact file names must be nonempty basenames without control characters or path separators; . and .. are not file names",
                ));
            }
            if !file_names.insert(value) {
                return Err(syn::Error::new_spanned(
                    name,
                    "duplicate file name in the same format",
                ));
            }
        }
        Ok(())
    }
}

impl Parse for Format {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let variant = input.parse()?;
        input.parse::<Token![=>]>()?;
        let fields;
        parenthesized!(fields in input);
        let id = fields.parse()?;
        fields.parse::<Token![,]>()?;
        let name = fields.parse()?;
        fields.parse::<Token![,]>()?;
        let category_list;
        bracketed!(category_list in fields);
        let categories = category_list.parse_terminated(<Ident as Parse>::parse, Token![,])?;
        fields.parse::<Token![,]>()?;
        let aliases;
        bracketed!(aliases in fields);
        let extensions = aliases.parse_terminated(<LitStr as Parse>::parse, Token![,])?;
        fields.parse::<Token![,]>()?;
        let names;
        bracketed!(names in fields);
        let file_names = names.parse_terminated(<LitStr as Parse>::parse, Token![,])?;
        if !fields.is_empty() {
            fields.parse::<Token![,]>()?;
        }
        if !fields.is_empty() {
            return Err(fields.error("unexpected format fields"));
        }
        Ok(Self {
            variant,
            id,
            name,
            categories,
            extensions,
            file_names,
        })
    }
}

pub(super) struct Catalog(Punctuated<Format, Token![,]>);

impl Parse for Catalog {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        Ok(Self(input.parse_terminated(Format::parse, Token![,])?))
    }
}

impl Catalog {
    fn validate(&self) -> Result<()> {
        let mut ids = BTreeSet::from([0]);
        let mut variants = BTreeSet::from(["Unknown".to_owned()]);
        let mut names = BTreeSet::from(["unknown".to_owned()]);
        for format in &self.0 {
            format.validate()?;
            if !ids.insert(format.id.base10_parse::<u16>()?) {
                return Err(syn::Error::new_spanned(
                    &format.id,
                    "duplicate format ID; 0 is reserved for Unknown",
                ));
            }
            let variant = format.variant.to_string();
            let variant = variant.strip_prefix("r#").unwrap_or(&variant);
            if !variants.insert(variant.to_owned()) {
                return Err(syn::Error::new_spanned(
                    &format.variant,
                    "duplicate format variant; Unknown is reserved",
                ));
            }
            if !names.insert(format.name.value()) {
                return Err(syn::Error::new_spanned(
                    &format.name,
                    "duplicate format name; unknown is reserved",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn expand(&self) -> Result<TokenStream> {
        self.validate()?;
        let variants: Vec<_> = self.0.iter().map(|format| &format.variant).collect();
        let ids: Vec<_> = self.0.iter().map(|format| &format.id).collect();
        let names: Vec<_> = self.0.iter().map(|format| &format.name).collect();
        let category_arms = self.0.iter().map(|format| {
            let variant = &format.variant;
            let categories = format.categories.iter();
            quote! { Self::#variant => &[#(FileCategory::#categories),*], }
        });
        let mut extensions: BTreeMap<String, Vec<&Ident>> = BTreeMap::new();
        let mut extension_entries = Vec::new();
        let mut name_entries = Vec::new();
        let mut file_names: BTreeMap<String, Vec<&Ident>> = BTreeMap::new();
        for format in &self.0 {
            let variant = &format.variant;
            for extension in &format.extensions {
                extensions
                    .entry(extension.value())
                    .or_default()
                    .push(variant);
                extension_entries.push(quote! { (#extension, FileFormat::#variant) });
            }
            for name in &format.file_names {
                file_names.entry(name.value()).or_default().push(variant);
                name_entries.push(quote! { (#name, FileFormat::#variant) });
            }
        }
        let max_extension_len = extensions.keys().map(String::len).max().unwrap_or(0);
        let extension_arms = extensions.iter().map(|(extension, candidates)| {
            quote! { #extension => &[#(FileFormat::#candidates),*], }
        });

        let name_arms = file_names.iter().map(|(name, candidates)| {
            quote! { #name => &[#(FileFormat::#candidates),*], }
        });

        Ok(quote! {
            #[repr(u16)]
            #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
            pub(crate) enum FileFormat {
                Unknown = 0,
                #(#variants = #ids,)*
            }

            impl FileFormat {
                pub(crate) const fn categories(self) -> &'static [FileCategory] {
                    match self {
                        Self::Unknown => &[FileCategory::Unknown],
                        #(#category_arms)*
                    }
                }

                pub(crate) const fn as_str(self) -> &'static str {
                    match self {
                        Self::Unknown => "unknown",
                        #(Self::#variants => #names,)*
                    }
                }

                pub(crate) const fn from_id(id: u16) -> Option<Self> {
                    match id {
                        0 => Some(Self::Unknown),
                        #(#ids => Some(Self::#variants),)*
                        _ => None,
                    }
                }
            }

            pub(super) fn lookup_name(name: &str) -> &'static [FileFormat] {
                match name {
                    #(#name_arms)*
                    _ => &[],
                }
            }

            pub(super) fn lookup_extension(extension: &str) -> &'static [FileFormat] {
                match extension {
                    #(#extension_arms)*
                    _ => &[],
                }
            }

            pub(super) const MAX_EXTENSION_LEN: usize = #max_extension_len;

            #[cfg(test)]
            pub(super) const FORMATS: &[FileFormat] = &[
                FileFormat::Unknown,
                #(FileFormat::#variants,)*
            ];

            #[cfg(test)]
            pub(super) const EXTENSIONS: &[(&str, FileFormat)] = &[
                #(#extension_entries,)*
            ];

            #[cfg(test)]
            pub(super) const FILE_NAMES: &[(&str, FileFormat)] = &[
                #(#name_entries,)*
            ];
        })
    }
}

#[cfg(test)]
mod tests {
    use proc_macro2::TokenStream;
    use quote::quote;

    use super::Catalog;

    fn assert_invalid(input: TokenStream, expected: &str) {
        let declaration = input.to_string();
        let error = syn::parse2::<Catalog>(input)
            .and_then(|catalog| catalog.expand())
            .expect_err("invalid declaration must be rejected");
        assert!(
            error.to_string().contains(expected),
            "{declaration}: {error}"
        );
    }

    fn expand(input: TokenStream) -> String {
        syn::parse2::<Catalog>(input)
            .and_then(|catalog| catalog.expand())
            .expect("valid declaration")
            .to_string()
    }

    #[test]
    fn rejects_duplicate_or_reserved_definitions() {
        let cases = [
            (
                quote! { Alpha => (1, "alpha", [Code, Code], [], []) },
                "duplicate category in the same format",
            ),
            (
                quote! { Alpha => (0, "alpha", [Unknown], [], []) },
                "0 is reserved",
            ),
            (
                quote! { Unknown => (1, "alpha", [Unknown], [], []) },
                "Unknown is reserved",
            ),
            (
                quote! { Alpha => (1, "unknown", [Unknown], [], []) },
                "unknown is reserved",
            ),
            (
                quote! { Alpha => (7, "alpha", [Unknown], [], []), Beta => (7, "beta", [Unknown], [], []) },
                "duplicate format ID",
            ),
            (
                quote! { Alpha => (1, "alpha", [Unknown], [], []), Alpha => (2, "beta", [Unknown], [], []) },
                "duplicate format variant",
            ),
            (
                quote! { Alpha => (1, "alpha", [Unknown], [], []), Beta => (2, "alpha", [Unknown], [], []) },
                "duplicate format name",
            ),
            (
                quote! { Alpha => (1, "alpha", [Unknown], ["a", "a"], []) },
                "duplicate extension in the same format",
            ),
            (
                quote! { Alpha => (1, "alpha", [Unknown], [], ["KnownFile", "KnownFile"]) },
                "duplicate file name in the same format",
            ),
        ];
        for (input, reason) in cases {
            assert_invalid(input, reason);
        }
    }

    #[test]
    fn groups_shared_extensions_and_file_names() {
        let output = expand(quote! {
            Alpha => (7, "alpha", [Unknown], ["a", "shared"], ["KnownFile", "SharedFile"]),
            Beta => (3, "beta", [Unknown], ["b", "shared"], ["SharedFile"]),
        });
        let extensions = quote! {
            match extension {
                "a" => &[FileFormat::Alpha],
                "b" => &[FileFormat::Beta],
                "shared" => &[FileFormat::Alpha, FileFormat::Beta],
                _ => &[],
            }
        }
        .to_string();
        let names = quote! {
            match name {
                "KnownFile" => &[FileFormat::Alpha],
                "SharedFile" => &[FileFormat::Alpha, FileFormat::Beta],
                _ => &[],
            }
        }
        .to_string();
        assert!(output.contains(&extensions), "{output}");
        assert!(output.contains(&names), "{output}");
    }

    #[test]
    fn allows_definitions_without_aliases() {
        let output = expand(quote! { Alpha => (1, "alpha", [Unknown], [], []) });
        let extensions = quote! { match extension { _ => &[], } }.to_string();
        let names = quote! { match name { _ => &[], } }.to_string();
        let maximum = quote! { const MAX_EXTENSION_LEN: usize = 0usize; }.to_string();
        assert!(output.contains(&extensions), "{output}");
        assert!(output.contains(&names), "{output}");
        assert!(output.contains(&maximum), "{output}");

        expand(quote! {
            Alpha => (1, "alpha", [Unknown], ["a"], []),
            Beta => (2, "beta", [Unknown], [], ["KnownFile"]),
        });
    }

    #[test]
    fn validates_declaration_syntax_and_characters() {
        assert_invalid(
            quote! { Alpha => (1, "alpha", [], [], []) },
            "formats must have at least one category",
        );
        assert_invalid(
            quote! { Alpha => (1, "alpha", [Unknown, Code], [], []) },
            "Unknown cannot be combined with specific categories",
        );
        for name in [
            "",
            "Alpha",
            " alpha",
            "alpha ",
            "alpha beta",
            "a\t",
            "a\0",
            "α",
            "a/b",
            "a\\b",
        ] {
            assert_invalid(
                quote! { Alpha => (1, #name, [Unknown], [], []) },
                "format names must be",
            );
        }
        for extension in [
            "", ".a", "a.", "a..b", "A", "a b", "a\t", "a\0", "α", "a/b", "a\\b",
        ] {
            assert_invalid(
                quote! { Alpha => (1, "alpha", [Unknown], [#extension], []) },
                "extensions must contain",
            );
        }
        for name in ["", ".", "..", "dir/File", "dir\\File", "File\0", "File\n"] {
            assert_invalid(
                quote! { Alpha => (1, "alpha", [Unknown], [], [#name]) },
                "exact file names must be",
            );
        }
        assert_invalid(
            quote! { Alpha => (65536, "alpha", [Unknown], [], []) },
            "number too large",
        );
        assert_invalid(
            quote! { Alpha => (1, "alpha", [Unknown], [], [], "extra") },
            "unexpected format fields",
        );
        assert_invalid(
            quote! { Alpha => (1 "alpha", [Unknown], [], []) },
            "expected `,`",
        );
        expand(quote! {
            Alpha => (7, "alpha-family", [Unknown], ["a++", "a-b", "a_b", "a.long", "7a",], ["Known File", "文件", ".settings",],),
        });
    }
}
