use crate::dispatcher::{DispatchMethod, Dispatcher};
use crate::target::Target;
use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
};
use syn::{
    parse::Parser, parse_quote, punctuated::Punctuated, spanned::Spanned, token::Comma, Error,
    ItemFn, Lit, LitStr, Meta, NestedMeta, Path, ReturnType, Type,
};

fn meta_path_string(meta: &Meta) -> Result<String, Error> {
    meta.path()
        .get_ident()
        .ok_or_else(|| Error::new(meta.path().span(), "expected identifier, got path"))
        .map(ToString::to_string)
}

fn meta_kv_value(meta: Meta) -> Result<Lit, Error> {
    if let Meta::NameValue(nv) = meta {
        Ok(nv.lit)
    } else {
        Err(Error::new(meta.span(), "expected name-value pair"))
    }
}

fn lit_str(lit: Lit) -> Result<LitStr, Error> {
    if let Lit::Str(s) = lit {
        Ok(s)
    } else {
        Err(Error::new(lit.span(), "expected string"))
    }
}

struct MetaMap {
    map: HashMap<String, Meta>,
    span: Span,
}

impl TryFrom<Punctuated<NestedMeta, Comma>> for MetaMap {
    type Error = Error;

    fn try_from(meta: Punctuated<NestedMeta, Comma>) -> Result<Self, Self::Error> {
        let mut map = HashMap::new();
        let span = meta.span();
        for meta in meta.into_iter() {
            let meta = if let NestedMeta::Meta(m) = meta {
                Ok(m)
            } else {
                Err(Error::new(meta.span(), "expected meta, got literal"))
            }?;

            let key = meta_path_string(&meta)?;
            if map.contains_key(&key) {
                return Err(Error::new(meta.path().span(), "key already provided"));
            }
            map.insert(key, meta);
        }
        Ok(Self { map, span })
    }
}

impl MetaMap {
    fn try_remove(&mut self, key: &str) -> Option<Meta> {
        self.map.remove(key)
    }

    fn span(&self) -> Span {
        self.span
    }

    fn finish(self) -> Result<(), Error> {
        if let Some((_, v)) = self.map.into_iter().next() {
            Err(Error::new(v.span(), "unexpected key"))
        } else {
            Ok(())
        }
    }
}

enum Specialization {
    Clone { target: Target },
}

impl TryFrom<Meta> for Specialization {
    type Error = Error;

    fn try_from(meta: Meta) -> Result<Self, Self::Error> {
        match meta_path_string(&meta)?.as_str() {
            "clone" => Ok(Self::Clone {
                target: Target::parse(&lit_str(meta_kv_value(meta)?)?)?,
            }),
            _ => Err(Error::new(meta.span(), "expected `clone` or `alternative`")),
        }
    }
}

struct Function {
    specializations: Vec<Specialization>,
    func: ItemFn,
    crate_path: Path,
    dispatcher: DispatchMethod,
}

impl Function {
    fn new(attr: Punctuated<NestedMeta, Comma>, func: ItemFn) -> Result<Self, Error> {
        let mut map = MetaMap::try_from(attr)?;

        let specializations = if let Some(clones) = map.try_remove("clones") {
            if let Meta::List(list) = clones {
                list.nested
                    .into_iter()
                    .map(|x| {
                        if let NestedMeta::Lit(lit) = x {
                            let target = Target::parse(&lit_str(lit)?)?;
                            Ok(Specialization::Clone { target })
                        } else {
                            Err(Error::new(x.span(), "expected target string"))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()
            } else {
                Err(Error::new(
                    clones.span(),
                    "expected list of function clone targets",
                ))
            }
        } else if let Some(versions) = map.try_remove("versions") {
            if let Meta::List(list) = versions {
                list.nested
                    .into_iter()
                    .map(|x| {
                        if let NestedMeta::Meta(meta) = x {
                            meta.try_into()
                        } else {
                            Err(Error::new(x.span(), "unexpected value"))
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()
            } else {
                Err(Error::new(
                    versions.span(),
                    "expected list of function versions",
                ))
            }
        } else {
            Err(Error::new(map.span(), "expected `clones` or `versions`"))
        }?;

        let dispatcher = map
            .try_remove("dispatcher")
            .map(|x| {
                let s = lit_str(meta_kv_value(x)?)?;
                match s.value().as_str() {
                    "default" => Ok(DispatchMethod::Default),
                    "static" => Ok(DispatchMethod::Static),
                    "direct" => Ok(DispatchMethod::Direct),
                    "indirect" => Ok(DispatchMethod::Indirect),
                    _ => Err(Error::new(
                        s.span(),
                        "expected `default`, `static`, `direct`, or `indirect`",
                    )),
                }
            })
            .unwrap_or_else(|| Ok(DispatchMethod::Default))?;
        let crate_path = map
            .try_remove("crate_path")
            .map(|x| lit_str(meta_kv_value(x)?)?.parse())
            .unwrap_or_else(|| Ok(parse_quote!(multiversion)))?;
        map.finish()?;
        Ok(Self {
            specializations,
            crate_path,
            dispatcher,
            func,
        })
    }
}

impl TryFrom<Function> for Dispatcher {
    type Error = Error;

    fn try_from(item: Function) -> Result<Self, Self::Error> {
        Ok(Self {
            specializations: item
                .specializations
                .iter()
                .map(|specialization| match specialization {
                    Specialization::Clone { target, .. } => crate::dispatcher::Specialization {
                        target: target.clone(),
                        block: item.func.block.as_ref().clone(),
                        normalize: false,
                    },
                })
                .collect(),
            attrs: item.func.attrs,
            vis: item.func.vis,
            sig: item.func.sig,
            default: *item.func.block,
            crate_path: item.crate_path,
            dispatcher: item.dispatcher,
        })
    }
}

pub(crate) fn make_multiversioned_fn(
    attr: TokenStream,
    func: ItemFn,
) -> Result<TokenStream, syn::Error> {
    if let ReturnType::Type(_, ty) = &func.sig.output {
        if let Type::ImplTrait(_) = **ty {
            return Err(Error::new(
                ty.span(),
                "cannot multiversion function with `impl Trait` return type",
            ));
        }
    }

    let parser = Punctuated::parse_terminated;
    let attr = parser.parse2(attr)?;
    let function = Function::new(attr, func)?;
    let dispatcher: Dispatcher = function.try_into()?;
    Ok(dispatcher.to_token_stream())
}
