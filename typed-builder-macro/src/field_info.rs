use proc_macro2::{Ident, Span, TokenStream};
use quote::quote_spanned;
use syn::{parse::Error, spanned::Spanned};

use crate::util::{
    expr_to_lit_string, ident_to_type, path_to_single_string, strip_raw_ident_prefix, ApplyMeta, AttrArg, KeyValue, Mutator,
};

#[derive(Debug)]
pub struct FieldInfo<'a> {
    pub ordinal: usize,
    pub name: &'a syn::Ident,
    pub generic_ident: syn::Ident,
    pub ty: &'a syn::Type,
    pub builder_attr: FieldBuilderAttr<'a>,
}

impl<'a> FieldInfo<'a> {
    pub fn new(ordinal: usize, field: &'a syn::Field, field_defaults: FieldBuilderAttr<'a>) -> Result<FieldInfo<'a>, Error> {
        if let Some(ref name) = field.ident {
            FieldInfo {
                ordinal,
                name,
                generic_ident: syn::Ident::new(&format!("__{}", strip_raw_ident_prefix(name.to_string())), Span::call_site()),
                ty: &field.ty,
                builder_attr: field_defaults.with(&field.attrs, name)?,
            }
            .post_process()
        } else {
            Err(Error::new(field.span(), "Nameless field in struct"))
        }
    }

    pub fn generic_ty_param(&self) -> syn::GenericParam {
        syn::GenericParam::Type(self.generic_ident.clone().into())
    }

    pub fn type_ident(&self) -> syn::Type {
        ident_to_type(self.generic_ident.clone())
    }

    pub fn tuplized_type_ty_param(&self) -> syn::Type {
        let mut types = syn::punctuated::Punctuated::default();
        types.push(self.ty.clone());
        types.push_punct(Default::default());
        syn::TypeTuple {
            paren_token: Default::default(),
            elems: types,
        }
        .into()
    }

    pub fn type_from_inside_option(&self) -> Option<&syn::Type> {
        let path = if let syn::Type::Path(type_path) = self.ty {
            if type_path.qself.is_some() {
                return None;
            }
            &type_path.path
        } else {
            return None;
        };
        let segment = path.segments.last()?;
        if segment.ident != "Option" {
            return None;
        }
        let generic_params = if let syn::PathArguments::AngleBracketed(generic_params) = &segment.arguments {
            generic_params
        } else {
            return None;
        };
        if let syn::GenericArgument::Type(ty) = generic_params.args.first()? {
            Some(ty)
        } else {
            None
        }
    }

    pub fn setter_method_name(&self) -> Ident {
        let name = strip_raw_ident_prefix(self.name.to_string());

        if let (Some(prefix), Some(suffix)) = (&self.builder_attr.setter.prefix, &self.builder_attr.setter.suffix) {
            Ident::new(&format!("{}{}{}", prefix, name, suffix), Span::call_site())
        } else if let Some(prefix) = &self.builder_attr.setter.prefix {
            Ident::new(&format!("{}{}", prefix, name), Span::call_site())
        } else if let Some(suffix) = &self.builder_attr.setter.suffix {
            Ident::new(&format!("{}{}", name, suffix), Span::call_site())
        } else {
            self.name.clone()
        }
    }

    fn post_process(mut self) -> Result<Self, Error> {
        if let Some(ref strip_bool_span) = self.builder_attr.setter.strip_bool {
            if let Some(default_span) = self.builder_attr.default.as_ref().map(Spanned::span) {
                let mut error = Error::new(
                    *strip_bool_span,
                    "cannot set both strip_bool and default - default is assumed to be false",
                );
                error.combine(Error::new(default_span, "default set here"));
                return Err(error);
            }
            self.builder_attr.default = Some(syn::Expr::Lit(syn::ExprLit {
                attrs: Default::default(),
                lit: syn::Lit::Bool(syn::LitBool {
                    value: false,
                    span: *strip_bool_span,
                }),
            }));
        }
        Ok(self)
    }
}

#[derive(Debug, Default, Clone)]
pub struct FieldBuilderAttr<'a> {
    pub default: Option<syn::Expr>,
    pub via_mutators: Option<syn::Expr>,
    pub deprecated: Option<&'a syn::Attribute>,
    pub setter: SetterSettings,
    /// Functions that are able to mutate fields in the builder that are already set
    pub mutators: Vec<Mutator>,
    pub mutable_during_default_resolution: Option<Span>,
}

#[derive(Debug, Default, Clone)]
pub struct SetterSettings {
    pub doc: Option<syn::Expr>,
    pub skip: Option<Span>,
    pub auto_into: Option<Span>,
    pub strip_option: Option<Span>,
    pub strip_bool: Option<Span>,
    pub transform: Option<Transform>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

impl<'a> FieldBuilderAttr<'a> {
    pub fn with(mut self, attrs: &'a [syn::Attribute], name: &Ident) -> Result<Self, Error> {
        for attr in attrs {
            let list = match &attr.meta {
                syn::Meta::List(list) => {
                    let Some(path) = path_to_single_string(&list.path) else {
                        continue;
                    };

                    if path == "deprecated" {
                        self.deprecated = Some(attr);
                        continue;
                    }

                    if path != "builder" {
                        continue;
                    }

                    list
                }
                syn::Meta::Path(path) | syn::Meta::NameValue(syn::MetaNameValue { path, .. }) => {
                    if path_to_single_string(path).as_deref() == Some("deprecated") {
                        self.deprecated = Some(attr);
                    };

                    continue;
                }
            };

            self.apply_subsections(list)?;
        }

        for mutator in self.mutators.iter_mut() {
            mutator.required_fields.insert(name.clone());
        }

        self.inter_fields_conflicts()?;

        Ok(self)
    }

    fn inter_fields_conflicts(&self) -> Result<(), Error> {
        if let (Some(skip), None) = (&self.setter.skip, &self.default) {
            return Err(Error::new(
                *skip,
                "#[builder(skip)] must be accompanied by default or default_code",
            ));
        }

        let conflicting_transformations = [
            ("transform", self.setter.transform.as_ref().map(|t| &t.span)),
            ("strip_option", self.setter.strip_option.as_ref()),
            ("strip_bool", self.setter.strip_bool.as_ref()),
        ];
        let mut conflicting_transformations = conflicting_transformations
            .iter()
            .filter_map(|(caption, span)| span.map(|span| (caption, span)))
            .collect::<Vec<_>>();

        if 1 < conflicting_transformations.len() {
            let (first_caption, first_span) = conflicting_transformations.pop().unwrap();
            let conflicting_captions = conflicting_transformations
                .iter()
                .map(|(caption, _)| **caption)
                .collect::<Vec<_>>();
            let mut error = Error::new(
                *first_span,
                format_args!("{} conflicts with {}", first_caption, conflicting_captions.join(", ")),
            );
            for (caption, span) in conflicting_transformations {
                error.combine(Error::new(*span, format_args!("{} set here", caption)));
            }
            return Err(error);
        }
        Ok(())
    }
}

impl ApplyMeta for FieldBuilderAttr<'_> {
    fn apply_meta(&mut self, expr: AttrArg) -> Result<(), Error> {
        match expr.name().to_string().as_str() {
            "default" => match expr {
                AttrArg::Flag(ident) => {
                    self.default =
                        Some(syn::parse2(quote_spanned!(ident.span() => ::core::default::Default::default())).unwrap());
                    Ok(())
                }
                AttrArg::KeyValue(key_value) => {
                    self.default = Some(key_value.parse_value()?);
                    Ok(())
                }
                AttrArg::Not { .. } => {
                    self.default = None;
                    Ok(())
                }
                AttrArg::Sub(_) => Err(expr.incorrect_type()),
            },
            "default_code" => {
                use std::str::FromStr;

                let code = expr.key_value()?.parse_value::<syn::LitStr>()?;
                let tokenized_code = TokenStream::from_str(&code.value())?;
                self.default = Some(syn::parse2(tokenized_code).map_err(|e| Error::new_spanned(code, format!("{}", e)))?);

                Ok(())
            }
            "setter" => self.setter.apply_sub_attr(expr),
            "mutable_during_default_resolution" => expr.apply_flag_to_field(
                &mut self.mutable_during_default_resolution,
                "made mutable during default resolution",
            ),
            "via_mutators" => {
                match expr {
                    AttrArg::Flag(ident) => {
                        self.via_mutators =
                            Some(syn::parse2(quote_spanned!(ident.span() => ::core::default::Default::default())).unwrap());
                    }
                    AttrArg::KeyValue(key_value) => {
                        self.via_mutators = Some(key_value.value);
                    }
                    AttrArg::Not { .. } => {
                        self.via_mutators = None;
                    }
                    AttrArg::Sub(sub) => {
                        let paren_span = sub.paren.span.span();
                        let mut args = sub.args()?.into_iter();
                        let Some(KeyValue { name, value, .. }) = args.next() else {
                            return Err(Error::new(paren_span, "Expected `init = ...`"));
                        };
                        if name != "init" {
                            return Err(Error::new_spanned(name, "Expected `init`"));
                        }
                        if let Some(remaining) = args.next() {
                            return Err(Error::new_spanned(remaining, "Expected only one argument (`init = ...`)"));
                        }
                        self.via_mutators = Some(value);
                    }
                }
                Ok(())
            }
            "mutators" => expr.sub_attr()?.undelimited().map(|fns| self.mutators.extend(fns)),
            _ => Err(Error::new_spanned(
                expr.name(),
                format!("Unknown parameter {:?}", expr.name().to_string()),
            )),
        }
    }
}

impl ApplyMeta for SetterSettings {
    fn apply_meta(&mut self, expr: AttrArg) -> Result<(), Error> {
        match expr.name().to_string().as_str() {
            "doc" => {
                self.doc = expr.key_value_or_not()?.map(|kv| kv.parse_value()).transpose()?;
                Ok(())
            }
            "transform" => {
                self.transform = if let Some(key_value) = expr.key_value_or_not()? {
                    Some(parse_transform_closure(key_value.name.span(), key_value.parse_value()?)?)
                } else {
                    None
                };
                Ok(())
            }
            "prefix" => {
                self.prefix = if let Some(key_value) = expr.key_value_or_not()? {
                    Some(expr_to_lit_string(&key_value.parse_value()?)?)
                } else {
                    None
                };
                Ok(())
            }
            "suffix" => {
                self.suffix = if let Some(key_value) = expr.key_value_or_not()? {
                    Some(expr_to_lit_string(&key_value.parse_value()?)?)
                } else {
                    None
                };
                Ok(())
            }
            "skip" => expr.apply_flag_to_field(&mut self.skip, "skipped"),
            "into" => expr.apply_flag_to_field(&mut self.auto_into, "calling into() on the argument"),
            "strip_option" => expr.apply_flag_to_field(&mut self.strip_option, "putting the argument in Some(...)"),
            "strip_bool" => expr.apply_flag_to_field(&mut self.strip_bool, "zero arguments setter, sets the field to true"),
            _ => Err(Error::new_spanned(
                expr.name(),
                format!("Unknown parameter {:?}", expr.name().to_string()),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Transform {
    pub params: Vec<(syn::Pat, syn::Type)>,
    pub body: syn::Expr,
    span: Span,
}

fn parse_transform_closure(span: Span, expr: syn::Expr) -> Result<Transform, Error> {
    let closure = match expr {
        syn::Expr::Closure(closure) => closure,
        _ => return Err(Error::new_spanned(expr, "Expected closure")),
    };
    if let Some(kw) = &closure.asyncness {
        return Err(Error::new(kw.span, "Transform closure cannot be async"));
    }
    if let Some(kw) = &closure.capture {
        return Err(Error::new(kw.span, "Transform closure cannot be move"));
    }

    let params = closure
        .inputs
        .into_iter()
        .map(|input| match input {
            syn::Pat::Type(pat_type) => Ok((*pat_type.pat, *pat_type.ty)),
            _ => Err(Error::new_spanned(input, "Transform closure must explicitly declare types")),
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Transform {
        params,
        body: *closure.body,
        span,
    })
}
