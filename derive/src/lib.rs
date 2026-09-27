//! Pek.RCode 派生宏：`#[derive(Entity)]`（对应 DH.NCode 的实体基类映射）。
//!
//! 为手写实体结构体生成 [`pek_rcode::Entity`] 的映射实现（表名/列/主键/取值/装载），
//! 增删改查由 `Entity` 的默认方法提供：
//!
//! ```ignore
//! use pek_rcode::Entity;
//! use pek_rcode_derive::Entity;
//!
//! #[derive(Entity)]
//! #[entity(table = "DH_Order")]
//! pub struct Order {
//!     #[entity(identity, primary_key)]
//!     pub id: i32,
//!     pub code: Option<String>,
//!     #[entity(column = "Status")]
//!     pub status: i32,
//! }
//! ```
//!
//! 支持的类型：`bool/u8/i16/i32/i64/f32/f64/String/Vec<u8>/NaiveDateTime/rust_decimal::Decimal`
//! 及其 `Option<...>`；成员枚举（SexKinds/MenuTypes/RoleTypes/TenantTypes/DepartmentTypes/
//! ParameterKinds/DataScope）按 `i32` 底层存取，未知成员取默认值。

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Type, parse_macro_input};

/// 派生 `pek_rcode::Entity` 实现。
#[proc_macro_derive(Entity, attributes(entity))]
pub fn derive_entity(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// 值转换类别。
enum ValueKind {
    /// 标量（DbValue 转换方法名）
    Scalar(&'static str),
    /// 文本
    Text,
    /// 二进制
    Blob,
    /// 时间
    DateTime,
    /// 十进制
    Decimal,
    /// 成员枚举（按 i32 底层存储）
    Enum,
}

/// 字段信息。
struct FieldInfo {
    /// 字段名
    ident: syn::Ident,
    /// 字段类型（去 Option 后的内部类型）
    inner_ty: Type,
    /// 列名
    column: String,
    /// 是否可空（Option<...>）
    nullable: bool,
    /// 是否主键
    primary_key: bool,
    /// 是否自增
    identity: bool,
    /// 值类别
    kind: ValueKind,
}

/// 展开派生实现。
fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;

    // 容器属性：#[entity(table = "...")]
    let mut table: Option<String> = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("entity") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("table") {
                let value = meta.value()?;
                let lit: syn::LitStr = value.parse()?;
                table = Some(lit.value());
                Ok(())
            } else {
                Err(meta.error("未知的 entity 容器属性（支持 table = \"...\"）"))
            }
        })?;
    }
    let table = table.unwrap_or_else(|| struct_name.to_string());

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            struct_name,
            "#[derive(Entity)] 仅支持具名字段结构体",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new_spanned(
            struct_name,
            "#[derive(Entity)] 需要具名字段",
        ));
    };

    let mut infos: Vec<FieldInfo> = Vec::new();
    for field in &fields.named {
        let ident = field.ident.clone().expect("具名字段");

        // 字段属性：#[entity(column = "...", primary_key, identity, skip)]
        let mut column: Option<String> = None;
        let mut primary_key = false;
        let mut identity = false;
        let mut skip = false;
        for attr in &field.attrs {
            if !attr.path().is_ident("entity") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("column") {
                    let value = meta.value()?;
                    let lit: syn::LitStr = value.parse()?;
                    column = Some(lit.value());
                    Ok(())
                } else if meta.path.is_ident("primary_key") {
                    primary_key = true;
                    Ok(())
                } else if meta.path.is_ident("identity") {
                    identity = true;
                    Ok(())
                } else if meta.path.is_ident("skip") {
                    skip = true;
                    Ok(())
                } else {
                    Err(meta.error(
                        "未知的 entity 字段属性（支持 column/primary_key/identity/skip）",
                    ))
                }
            })?;
        }
        if skip {
            continue;
        }

        // Option<T> → 可空
        let (inner_ty, nullable) = unwrap_option(&field.ty);
        let kind = classify(&inner_ty).ok_or_else(|| {
            syn::Error::new_spanned(
                &field.ty,
                "#[derive(Entity)] 不支持该类型（支持 bool/u8/i16/i32/i64/f32/f64/String/Vec<u8>/NaiveDateTime/Decimal/成员枚举 及 Option<...>）",
            )
        })?;

        infos.push(FieldInfo {
            ident,
            inner_ty,
            column: column.unwrap_or_else(|| field.ident.as_ref().expect("具名字段").to_string()),
            nullable,
            primary_key,
            identity,
            kind,
        });
    }

    let columns: Vec<&str> = infos.iter().map(|f| f.column.as_str()).collect();
    let pk_columns: Vec<&str> = infos
        .iter()
        .filter(|f| f.primary_key)
        .map(|f| f.column.as_str())
        .collect();

    let identity_field = infos.iter().find(|f| f.identity);
    let identity_expr = match identity_field {
        Some(f) => {
            let column = &f.column;
            quote! { Some(#column) }
        }
        None => quote! { None },
    };

    // to_fields
    let mut to_exprs: Vec<TokenStream2> = Vec::new();
    for f in &infos {
        let ident = &f.ident;
        let column = &f.column;
        let expr = match f.kind {
            ValueKind::Enum => {
                if f.nullable {
                    quote! { (self.#ident.map(|v| v as i32)).into() }
                } else {
                    quote! { ((self.#ident as i32)).into() }
                }
            }
            _ => quote! { self.#ident.clone().into() },
        };
        to_exprs.push(quote! { (#column, #expr) });
    }

    // from_row
    let mut from_exprs: Vec<TokenStream2> = Vec::new();
    for f in &infos {
        let ident = &f.ident;
        let column = &f.column;
        let nullable = f.nullable;
        let expr = match &f.kind {
            ValueKind::Text => {
                if nullable {
                    quote! { row.get_by_name(#column).and_then(|v| (!v.is_null()).then(|| v.to_text())) }
                } else {
                    quote! { row.get_by_name(#column).map(pek_rcode::DbValue::to_text).unwrap_or_default() }
                }
            }
            ValueKind::Blob => {
                if nullable {
                    quote! { row.get_by_name(#column).and_then(|v| v.as_blob().map(<[u8]>::to_vec)) }
                } else {
                    quote! { row.get_by_name(#column).and_then(|v| v.as_blob().map(<[u8]>::to_vec)).unwrap_or_default() }
                }
            }
            ValueKind::DateTime => {
                if nullable {
                    quote! { row.get_by_name(#column).and_then(pek_rcode::DbValue::as_datetime) }
                } else {
                    quote! { row.get_by_name(#column).and_then(pek_rcode::DbValue::as_datetime).unwrap_or_else(|| chrono::DateTime::UNIX_EPOCH.naive_utc()) }
                }
            }
            ValueKind::Decimal => {
                if nullable {
                    quote! { row.get_by_name(#column).and_then(pek_rcode::DbValue::as_decimal) }
                } else {
                    quote! { row.get_by_name(#column).and_then(pek_rcode::DbValue::as_decimal).unwrap_or_default() }
                }
            }
            ValueKind::Scalar(method) => {
                let method = syn::Ident::new(method, proc_macro2::Span::call_site());
                if nullable {
                    quote! { row.get_by_name(#column).and_then(pek_rcode::DbValue::#method) }
                } else {
                    quote! { row.get_by_name(#column).and_then(pek_rcode::DbValue::#method).unwrap_or_default() }
                }
            }
            ValueKind::Enum => {
                let ty = &f.inner_ty;
                if nullable {
                    quote! { row.get_by_name(#column).and_then(pek_rcode::DbValue::as_i32).and_then(<#ty>::from_i32) }
                } else {
                    quote! { row.get_by_name(#column).and_then(pek_rcode::DbValue::as_i32).and_then(<#ty>::from_i32).unwrap_or_default() }
                }
            }
        };
        from_exprs.push(quote! { #ident: #expr });
    }

    // set_identity（自增列必须为整数标量）
    let set_identity = match identity_field {
        Some(f) => {
            let ident = &f.ident;
            let cast = match f.kind {
                ValueKind::Scalar("as_i64") => quote! { value },
                ValueKind::Scalar(_) => {
                    let ty = &f.inner_ty;
                    quote! { value as #ty }
                }
                _ => {
                    return Err(syn::Error::new_spanned(
                        &f.ident,
                        "自增（identity）字段必须是整数类型",
                    ));
                }
            };
            quote! {
                fn set_identity(&mut self, value: i64) -> pek_rcode::Result<()> {
                    self.#ident = #cast;
                    Ok(())
                }
            }
        }
        None => quote! {},
    };

    Ok(quote! {
        impl pek_rcode::Entity for #struct_name {
            fn table() -> &'static str {
                #table
            }

            fn columns() -> &'static [&'static str] {
                &[#(#columns),*]
            }

            fn primary_keys() -> &'static [&'static str] {
                &[#(#pk_columns),*]
            }

            fn identity_column() -> Option<&'static str> {
                #identity_expr
            }

            fn to_fields(&self) -> Vec<(&'static str, pek_rcode::DbValue)> {
                vec![#(#to_exprs),*]
            }

            fn from_row(row: &pek_rcode::DbRow) -> pek_rcode::Result<Self> {
                Ok(Self {
                    #(#from_exprs),*
                })
            }

            #set_identity
        }
    })
}

/// 解包 `Option<T>` → `(T, true)`；非 Option 原样返回。
fn unwrap_option(ty: &Type) -> (Type, bool) {
    if let Type::Path(path) = ty
        && let Some(segment) = path.path.segments.last()
        && segment.ident == "Option"
        && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
        && let Some(syn::GenericArgument::Type(inner)) = args.args.first()
    {
        return (inner.clone(), true);
    }
    (ty.clone(), false)
}

/// 类型 → 值类别。
fn classify(ty: &Type) -> Option<ValueKind> {
    let Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    let name = segment.ident.to_string();
    // Vec<u8> 特判
    if name == "Vec"
        && let syn::PathArguments::AngleBracketed(args) = &segment.arguments
        && let Some(syn::GenericArgument::Type(Type::Path(inner))) = args.args.first()
        && inner.path.is_ident("u8")
    {
        return Some(ValueKind::Blob);
    }
    match name.as_str() {
        "bool" => Some(ValueKind::Scalar("as_bool")),
        "u8" => Some(ValueKind::Scalar("as_u8")),
        "i16" => Some(ValueKind::Scalar("as_i16")),
        "i32" => Some(ValueKind::Scalar("as_i32")),
        "i64" => Some(ValueKind::Scalar("as_i64")),
        "f32" => Some(ValueKind::Scalar("as_f32")),
        "f64" => Some(ValueKind::Scalar("as_f64")),
        "String" | "str" => Some(ValueKind::Text),
        "NaiveDateTime" => Some(ValueKind::DateTime),
        "Decimal" => Some(ValueKind::Decimal),
        "SexKinds" | "MenuTypes" | "RoleTypes" | "TenantTypes" | "DepartmentTypes"
        | "ParameterKinds" | "DataScope" | "DataScopes" => Some(ValueKind::Enum),
        _ => None,
    }
}
