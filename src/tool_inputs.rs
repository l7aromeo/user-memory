//! The tools' inputs as the source declares them, for the tests that hold
//! a list of every tool input complete: the option floors
//! (src/setup/versions.rs) and the read-only scope (src/remote/scope.rs).
//! Read from the source with syn, so inputs hidden from the schema count.

use std::collections::{BTreeMap, BTreeSet};

/// The source of user-memory's own files, parsed.
pub(crate) fn parsed(path: &str) -> syn::File {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let text = std::fs::read_to_string(&path).unwrap();
    syn::parse_file(&text).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

/// Every struct in src/memory.rs and src/memory/, by name.
pub(crate) fn memory_structs() -> BTreeMap<String, syn::ItemStruct> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/memory");
    let mut files = vec!["src/memory.rs".to_owned()];
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.ends_with(".rs") {
            files.push(format!("src/memory/{name}"));
        }
    }
    files
        .iter()
        .flat_map(|file| parsed(file).items)
        .filter_map(|item| match item {
            syn::Item::Struct(item) => Some((item.ident.to_string(), item)),
            _ => None,
        })
        .collect()
}

/// The name of the last segment of `ty`'s path: `memory::Read` is `Read`.
pub(crate) fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(path) => path.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

/// Each `#[tool]` in src/main.rs with the struct of its `Parameters<…>`.
pub(crate) fn tool_structs() -> BTreeMap<String, String> {
    let mut tools = BTreeMap::new();
    for item in parsed("src/main.rs").items {
        let syn::Item::Impl(block) = item else {
            continue;
        };
        for item in block.items {
            let syn::ImplItem::Fn(function) = item else {
                continue;
            };
            if !function
                .attrs
                .iter()
                .any(|attr| attr.path().is_ident("tool"))
            {
                continue;
            }
            for input in &function.sig.inputs {
                let syn::FnArg::Typed(input) = input else {
                    continue;
                };
                let syn::Type::Path(path) = &*input.ty else {
                    continue;
                };
                let Some(last) = path.path.segments.last() else {
                    continue;
                };
                if last.ident != "Parameters" {
                    continue;
                }
                let syn::PathArguments::AngleBracketed(arguments) = &last.arguments else {
                    continue;
                };
                if let Some(syn::GenericArgument::Type(ty)) = arguments.args.first()
                    && let Some(name) = type_name(ty)
                {
                    tools.insert(function.sig.ident.to_string(), name);
                }
            }
        }
    }
    tools
}

/// The input names a server reads for `name`'s fields: each field's
/// serde name and aliases, with flattened structs' fields in its place
/// and skipped fields left out. Fields hidden from the schema count.
pub(crate) fn inputs(structs: &BTreeMap<String, syn::ItemStruct>, name: &str) -> BTreeSet<String> {
    use syn::ext::IdentExt;
    let item = structs
        .get(name)
        .unwrap_or_else(|| panic!("no struct {name} in src/memory"));
    let mut names = BTreeSet::new();
    for field in &item.fields {
        let (mut renamed, mut aliases, mut flatten, mut skip) = (None, Vec::new(), false, false);
        for attr in field
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("serde"))
        {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("rename") {
                    renamed = Some(meta.value()?.parse::<syn::LitStr>()?.value());
                } else if meta.path.is_ident("alias") {
                    aliases.push(meta.value()?.parse::<syn::LitStr>()?.value());
                } else if meta.path.is_ident("flatten") {
                    flatten = true;
                } else if meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing") {
                    skip = true;
                } else if meta.input.peek(syn::Token![=]) {
                    meta.value()?.parse::<syn::Expr>()?;
                }
                Ok(())
            })
            .unwrap();
        }
        if skip {
            continue;
        }
        if flatten {
            let inner = type_name(&field.ty).expect("a flattened field names its struct");
            names.extend(inputs(structs, &inner));
            continue;
        }
        let ident = field.ident.as_ref().expect("tool inputs have named fields");
        names.insert(renamed.unwrap_or_else(|| ident.unraw().to_string()));
        names.extend(aliases);
    }
    names
}
