//! `#[tool(...)]` 属性宏 —— 让 Rust 的工具定义长得像 Python 版的 `@tool(...)` 装饰器。
//!
//! ```ignore
//! #[tool(
//!     name = "read",
//!     description = "读取文件内容…",
//!     parameters = r#"{"path": {"type": "string"}}"#   // 可选：properties 的 JSON
//! )]
//! fn read(path: String, _max_lines: Option<i64>) -> ToolResult { ... }
//! ```
//!
//! 宏做四件事（对齐 Python 版 `tools.tool()` 装饰器）：
//!   1. 生成 `<fn>_tool() -> ToolDef`：把 name/description/schema 打包成工具定义；
//!   2. 从**函数签名**推导哪些参数必填（非 `Option`）—— 对应 Python 的「无默认值即必填」；
//!   3. 下划线开头的参数视为**私有参数**：不进 schema，由配置按工具名注入；
//!   4. 生成参数解析 + 调用的分发函数（`Value` → 具名参数），省掉手写 `match 工具名`。
//!
//! 约定：类型写作 `Events<'_>` 的参数是**上下文参数**（工具运行时的进度回调），
//! 同样不进 schema、不从 JSON 读取，由分发层直接注入。

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse::Parser, parse_macro_input, punctuated::Punctuated, FnArg, Ident, ItemFn, Lit, Meta,
    Pat, Token, Type,
};

#[proc_macro_attribute]
pub fn tool(attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match expand(attr.into(), func) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[derive(Default)]
struct ToolArgs {
    name: Option<String>,
    description: Option<String>,
    /// `properties` 的 JSON（不含 `type`/`required` 外壳），与 Python 的 `parameters={...}` 同形。
    parameters: Option<String>,
}

fn expand(attr: TokenStream2, func: ItemFn) -> syn::Result<TokenStream2> {
    let parsed = Punctuated::<Meta, Token![,]>::parse_terminated.parse2(attr)?;
    let mut tool_args = ToolArgs::default();
    for meta in parsed {
        let Meta::NameValue(nv) = meta else {
            continue;
        };
        let key = nv
            .path
            .get_ident()
            .map(Ident::to_string)
            .unwrap_or_default();
        let syn::Expr::Lit(syn::ExprLit {
            lit: Lit::Str(value),
            ..
        }) = &nv.value
        else {
            return Err(syn::Error::new_spanned(
                &nv.value,
                "#[tool(...)] 的值必须是字符串字面量（parameters 请写成 JSON 的 raw string）",
            ));
        };
        match key.as_str() {
            "name" => tool_args.name = Some(value.value()),
            "description" => tool_args.description = Some(value.value()),
            "parameters" => tool_args.parameters = Some(value.value()),
            other => {
                return Err(syn::Error::new_spanned(
                    nv.path,
                    format!("不认识的 #[tool] 参数: {other}（可用：name / description / parameters）"),
                ))
            }
        }
    }

    let fn_ident = func.sig.ident.clone();
    let tool_name = tool_args
        .name
        .clone()
        .unwrap_or_else(|| fn_ident.to_string());
    let description = tool_args
        .description
        .clone()
        .or_else(|| doc_summary(&func).ok().flatten())
        .unwrap_or_else(|| tool_name.clone());
    let is_async = func.sig.asyncness.is_some();

    // ---- 收集参数 ----
    let mut named: Vec<Param> = Vec::new();
    for input in &func.sig.inputs {
        let FnArg::Typed(pat_type) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "#[tool] 不支持 self 参数（工具是自由函数）",
            ));
        };
        let Pat::Ident(pat_ident) = &*pat_type.pat else {
            return Err(syn::Error::new_spanned(
                &pat_type.pat,
                "#[tool] 的参数必须是普通标识符（不支持模式解构）",
            ));
        };
        let ident = pat_ident.ident.clone();
        let name = ident.to_string();
        let ty = (*pat_type.ty).clone();
        named.push(Param {
            is_events: is_events_type(&ty),
            private: name.starts_with('_'),
            optional: is_option_type(&ty),
            ident,
            ty,
        });
    }

    // ---- schema ----
    let properties_json = match &tool_args.parameters {
        Some(json) => json.clone(),
        None => auto_properties(&named),
    };
    let required: Vec<String> = named
        .iter()
        .filter(|p| !p.private && !p.is_events && !p.optional)
        .map(|p| p.ident.to_string())
        .collect();
    let private_params: Vec<String> = named
        .iter()
        .filter(|p| p.private && !p.is_events)
        .map(|p| p.ident.to_string())
        .collect();

    // ---- 参数结构体（跳过上下文参数）----
    let fields = named.iter().filter(|p| !p.is_events).map(|p| {
        let ident = &p.ident;
        let ty = &p.ty;
        if p.optional || p.private {
            quote!(#[serde(default)] pub #ident: #ty,)
        } else {
            quote!(pub #ident: #ty,)
        }
    });
    let has_fields = named.iter().any(|p| !p.is_events);

    // ---- 调用 ----
    let call_args = named.iter().map(|p| {
        let ident = &p.ident;
        if p.is_events {
            quote!(crate::tools::Events(on_event))
        } else {
            quote!(__args.#ident)
        }
    });
    let call = if is_async {
        quote!(#fn_ident(#(#call_args),*).await)
    } else {
        quote!(#fn_ident(#(#call_args),*))
    };

    let uses_events = named.iter().any(|p| p.is_events);
    let on_event_ident = if uses_events {
        format_ident!("on_event")
    } else {
        format_ident!("_on_event")
    };
    let bind_args = if has_fields {
        quote! {
            let __args: __Args = match ::serde_json::from_value(args) {
                Ok(v) => v,
                Err(e) => {
                    return ::std::result::Result::Err(crate::tools::ToolError(format!(
                        "参数解析失败（{}）: {e}",
                        #tool_name
                    )))
                }
            };
        }
    } else {
        quote!(let _ = &args;)
    };

    let call_ident = format_ident!("__{}_call", fn_ident);
    let tool_ident = format_ident!("{}_tool", fn_ident);

    Ok(quote! {
        #func

        #[doc(hidden)]
        fn #call_ident<'a>(
            args: ::serde_json::Value,
            #on_event_ident: crate::tools::EventFn<'a>,
        ) -> crate::tools::ToolFuture<'a> {
            ::std::boxed::Box::pin(async move {
                #[derive(::serde::Deserialize)]
                #[allow(non_snake_case)]
                struct __Args {
                    #(#fields)*
                }
                #bind_args
                #call
            })
        }

        /// 由 `#[tool]` 生成的工具定义（name / description / schema / 分发函数）。
        pub fn #tool_ident() -> crate::tools::ToolDef {
            crate::tools::ToolDef {
                name: #tool_name,
                description: #description,
                properties_json: #properties_json,
                required: &[#(#required),*],
                private_params: &[#(#private_params),*],
                call: #call_ident,
            }
        }
    })
}

struct Param {
    ident: Ident,
    ty: Type,
    optional: bool,
    private: bool,
    is_events: bool,
}

/// 类型是否写作 `Events<'_>`（上下文参数：进度回调，不进 schema、不从 JSON 读）。
fn is_events_type(ty: &Type) -> bool {
    last_segment(ty).as_deref() == Some("Events")
}

fn is_option_type(ty: &Type) -> bool {
    last_segment(ty).as_deref() == Some("Option")
}

fn last_segment(ty: &Type) -> Option<String> {
    let Type::Path(p) = ty else { return None };
    p.path.segments.last().map(|s| s.ident.to_string())
}

/// 没给 `parameters` 时按签名推导 properties（类型 → JSON Schema 的粗略映射）。
fn auto_properties(params: &[Param]) -> String {
    let mut map = serde_json::Map::new();
    for p in params {
        if p.private || p.is_events {
            continue;
        }
        map.insert(p.ident.to_string(), type_schema(&p.ty));
    }
    serde_json::Value::Object(map).to_string()
}

fn type_schema(ty: &Type) -> serde_json::Value {
    use serde_json::json;
    match last_segment(ty).as_deref() {
        Some("Option") | Some("Vec") => match inner_type(ty) {
            Some(inner) => type_schema(&inner),
            None => json!({}),
        },
        Some("String") | Some("str") => json!({"type": "string"}),
        Some("bool") => json!({"type": "boolean"}),
        Some("f32") | Some("f64") => json!({"type": "number"}),
        Some("i8") | Some("i16") | Some("i32") | Some("i64") | Some("isize") | Some("u8")
        | Some("u16") | Some("u32") | Some("u64") | Some("usize") => json!({"type": "integer"}),
        _ => json!({}),
    }
}

fn inner_type(ty: &Type) -> Option<Type> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    })
}

/// 从文档注释里取第一行非空内容（对应 Python 的 `fn.__doc__.splitlines()[0]`）。
fn doc_summary(func: &ItemFn) -> syn::Result<Option<String>> {
    for attr in &func.attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: Lit::Str(text),
                ..
            }) = &nv.value
            {
                if let Some(line) = text.value().lines().map(str::trim).find(|l| !l.is_empty()) {
                    return Ok(Some(line.to_string()));
                }
            }
        }
    }
    Ok(None)
}
