// Copyright 2024 RisingWave Labs
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

//! Generate code for the functions.

use itertools::Itertools;
use proc_macro2::{Ident, Span};
use quote::{format_ident, quote};

use super::*;

impl FunctionAttr {
    /// Expands the wildcard in function arguments or return type.
    pub fn expand(&self) -> Vec<Self> {
        let args = self.args.iter().map(|ty| types::expand_type_wildcard(ty));
        let ret = types::expand_type_wildcard(&self.ret);
        // multi_cartesian_product should emit an empty set if the input is empty.
        let args_cartesian_product =
            args.multi_cartesian_product()
                .chain(match self.args.is_empty() {
                    true => vec![vec![]],
                    false => vec![],
                });
        let mut attrs = Vec::new();
        for (args, ret) in args_cartesian_product.cartesian_product(ret) {
            let attr = FunctionAttr {
                args: args.iter().map(|s| s.to_string()).collect(),
                ret: ret.to_string(),
                ..self.clone()
            };
            attrs.push(attr);
        }
        attrs
    }

    /// Generate the type infer function.
    #[allow(dead_code)]
    fn generate_type_infer_fn(&self) -> Result<TokenStream2> {
        if let Some(func) = &self.type_infer {
            if func == "panic" {
                return Ok(quote! { |_| panic!("type inference function is not implemented") });
            }
            // use the user defined type inference function
            return Ok(func.parse().unwrap());
        } else if self.ret == "any" {
            // TODO: if there are multiple "any", they should be the same type
            if let Some(i) = self.args.iter().position(|t| t == "any") {
                // infer as the type of "any" argument
                return Ok(quote! { |args| Ok(args[#i].clone()) });
            }
            if let Some(i) = self.args.iter().position(|t| t == "anyarray") {
                // infer as the element type of "anyarray" argument
                return Ok(quote! { |args| Ok(args[#i].as_list().clone()) });
            }
        } else if self.ret == "anyarray" {
            if let Some(i) = self.args.iter().position(|t| t == "anyarray") {
                // infer as the type of "anyarray" argument
                return Ok(quote! { |args| Ok(args[#i].clone()) });
            }
            if let Some(i) = self.args.iter().position(|t| t == "any") {
                // infer as the array type of "any" argument
                return Ok(quote! { |args| Ok(DataType::List(Box::new(args[#i].clone()))) });
            }
        } else if self.ret == "struct" {
            if let Some(i) = self.args.iter().position(|t| t == "struct") {
                // infer as the type of "struct" argument
                return Ok(quote! { |args| Ok(args[#i].clone()) });
            }
        } else {
            // the return type is fixed
            let ty = data_type(&self.ret);
            return Ok(quote! { |_| Ok(#ty) });
        }
        Err(Error::new(
            Span::call_site(),
            "type inference function is required",
        ))
    }

    /// Generate a descriptor of the scalar or table function.
    ///
    /// The types of arguments and return value should not contain wildcard.
    pub fn generate_function_descriptor(&self, user_fn: &UserFunctionAttr) -> Result<TokenStream2> {
        let name = self.name.clone();
        let variadic = matches!(self.args.last(), Some(t) if t == "...");
        let args = match variadic {
            true => &self.args[..self.args.len() - 1],
            false => &self.args[..],
        }
        .iter()
        .map(|ty| sig_data_type(ty))
        .collect_vec();
        let ret = sig_data_type(&self.ret);

        let eval_name = match &self.output {
            Some(output) => format_ident!("{}", output),
            None => format_ident!("{}_eval", self.ident_name()),
        };
        let sig_name = format_ident!("{}_sig", self.ident_name());
        let ffi_name = format_ident!("{}_ffi", self.ident_name());
        let export_name = format!("arrowudf_{}", base64_encode(&self.normalize_signature()));
        let eval_function = self.generate_function(user_fn, &eval_name)?;
        let kind = match self.is_table_function {
            true => quote! { Table },
            false => quote! { Scalar },
        };
        let ffi = match self.is_table_function {
            true => quote! {}, // TODO: add ffi for table functions
            false => quote! {
                #[export_name = #export_name]
                unsafe extern "C" fn #ffi_name(ptr: *const u8, len: usize, out: *mut arrow_udf::ffi::CSlice) -> i32 {
                    arrow_udf::ffi::scalar_wrapper(#eval_name, ptr, len, out)
                }
            },
        };

        Ok(quote! {
            #eval_function

            #ffi

            #[cfg(feature = "global_registry")]
            #[::arrow_udf::codegen::linkme::distributed_slice(::arrow_udf::sig::SIGNATURES)]
            fn #sig_name() -> ::arrow_udf::sig::FunctionSignature {
                use ::arrow_udf::sig::{FunctionSignature, FunctionKind, SigDataType};
                use ::arrow_udf::codegen::arrow_schema::{self, TimeUnit, IntervalUnit};

                FunctionSignature {
                    name: #name.into(),
                    arg_types: vec![#(#args),*],
                    variadic: #variadic,
                    return_type: #ret,
                    function: FunctionKind::#kind(#eval_name),
                }
            }
        })
    }

    /// Generate a scalar or table function.
    fn generate_function(
        &self,
        user_fn: &UserFunctionAttr,
        eval_fn_name: &Ident,
    ) -> Result<TokenStream2> {
        let variadic = matches!(self.args.last(), Some(t) if t == "...");
        let num_args = self.args.len() - if variadic { 1 } else { 0 };
        let user_fn_name = format_ident!("{}", user_fn.name);
        let fn_name = &self.name;

        let children_indices = (0..num_args).collect_vec();

        /// Return a list of identifiers with the given prefix and indices.
        fn idents(prefix: &str, indices: &[usize]) -> Vec<Ident> {
            indices
                .iter()
                .map(|i| format_ident!("{prefix}{i}"))
                .collect()
        }
        let inputs = idents("i", &children_indices);
        let arrays = idents("a", &children_indices);
        let arg_arrays = children_indices
            .iter()
            .map(|i| format_ident!("{}", types::array_type(&self.args[*i])));
        let ret_array_type = format_ident!("{}", types::array_type(&self.ret));
        let ret_data_type = data_type(&self.ret);

        let variadic_args = variadic.then(|| quote! { variadic_row, });
        let context = user_fn.context.then(|| quote! { &self.context, });
        let writer = user_fn.write.then(|| quote! { &mut writer, });
        let await_ = user_fn.async_.then(|| quote! { .await });
        // transform inputs for array arguments
        // e.g. for `int[]`, transform `ArrayRef` -> `&[T]`
        let transformed_inputs = inputs
            .iter()
            .zip(&self.args)
            .map(|(input, ty)| transform_input(input, ty));
        // call the user defined function
        let mut output = quote! { #user_fn_name(
            #(#transformed_inputs,)*
            #variadic_args
            #context
            #writer
        ) #await_ };
        // handle error if the function returns `Result`
        // wrap a `Some` if the function doesn't return `Option`
        output = if self.is_table_function {
            match user_fn.return_type_kind {
                ReturnTypeKind::T => quote! { Some(#output) },
                ReturnTypeKind::Option => output,
                ReturnTypeKind::Result => {
                    quote! { match #output {
                        Ok(x) => Some(x),
                        Err(e) => {
                            index_builder.append_value(i as i32);
                            builder.append_null();
                            error_builder.append_value(e.to_string());
                            None
                        }
                    } }
                }
                ReturnTypeKind::ResultOption => {
                    quote! { match #output {
                        Ok(x) => x,
                        Err(e) => {
                            index_builder.append_value(i as i32);
                            builder.append_null();
                            error_builder.append_value(e.to_string());
                            None
                        }
                    } }
                }
            }
        } else {
            match user_fn.return_type_kind {
                ReturnTypeKind::T => quote! { Some(#output) },
                ReturnTypeKind::Option => output,
                ReturnTypeKind::Result => {
                    quote! { match #output {
                        Ok(x)  => { error_builder.append_null(); Some(x) },
                        Err(e) => { error_builder.append_value(e.to_string()); None }
                    } }
                }
                ReturnTypeKind::ResultOption => {
                    quote! { match #output {
                        Ok(x)  => { error_builder.append_null(); x },
                        Err(e) => { error_builder.append_value(e.to_string()); None }
                    } }
                }
            }
        };
        // if user function accepts non-option arguments, we assume the function
        // returns null on null input, so we need to unwrap the inputs before calling.
        let some_inputs = inputs
            .iter()
            .zip(user_fn.args_option.iter())
            .map(|(input, opt)| {
                if *opt {
                    quote! { #input }
                } else {
                    quote! { Some(#input) }
                }
            });
        output = quote! {
            match (#(#inputs,)*) {
                (#(#some_inputs,)*) => #output,
                _ => None,
            }
        };

        let eval = if self.is_table_function {
            let array_zip = match children_indices.len() {
                0 => quote! { std::iter::repeat(()).take(input.num_rows()) },
                _ => quote! { itertools::multizip((#(#arrays.iter(),)*)) },
            };
            let builder = builder(&self.ret);
            let append_output = gen_append(&self.ret);
            let error_append_null = user_fn
                .has_error()
                .then(|| quote! { error_builder.append_null(); });
            let element = match user_fn.iterator_item_kind.clone().unwrap() {
                ReturnTypeKind::T => quote! {{ #error_append_null; Some(v) }},
                ReturnTypeKind::Option => quote! {{ #error_append_null; v }},
                ReturnTypeKind::Result => {
                    quote! { match v {
                        Ok(x) => { error_builder.append_null(); Some(x) },
                        Err(e) => { error_builder.append_value(e.to_string()); None }
                    } }
                }
                ReturnTypeKind::ResultOption => {
                    quote! { match v {
                        Ok(x) => { error_builder.append_null(); x },
                        Err(e) => { error_builder.append_value(e.to_string()); None }
                    } }
                }
            };

            let error_field = user_fn.has_error().then(|| {
                quote! { Field::new("error", DataType::Utf8, true), }
            });
            let let_error_builder = user_fn.has_error().then(|| {
                quote! { let mut error_builder = StringBuilder::with_capacity(input.num_rows(), input.num_rows() * 16); }
            });
            let error_array = user_fn.has_error().then(|| {
                quote! { Arc::new(error_builder.finish()) }
            });
            let yield_batch = quote! {
                let index_array = Arc::new(index_builder.finish());
                let value_array = Arc::new(builder.finish());
                yield_!(RecordBatch::try_new(SCHEMA.clone(), vec![index_array, value_array, #error_array]).unwrap());
            };
            quote! {{
                lazy_static! {
                    static ref SCHEMA: SchemaRef = Arc::new(Schema::new(vec![
                        Field::new("row", DataType::Int32, true),
                        Field::new(#fn_name, #ret_data_type, true),
                        #error_field
                    ]));
                }
                let mut index_builder = Int32Builder::with_capacity(input.num_rows());
                let mut builder = #builder;
                #let_error_builder
                for (i, (#(#inputs,)*)) in #array_zip.enumerate() {
                    let Some(iter) = (#output) else {
                        continue;
                    };
                    for v in iter {
                        index_builder.append_value(i as i32);
                        let v = #element;
                        #append_output
                        if index_builder.len() == BATCH_SIZE {
                            #yield_batch
                        }
                    }
                }
                if index_builder.len() > 0 {
                    #yield_batch
                }
            }}
        } else if let Some(batch_fn) = &self.batch_fn {
            if variadic {
                return Err(Error::new(
                    Span::call_site(),
                    "customized batch function is not supported for variadic functions",
                ));
            }
            // user defined batch function
            let fn_name = format_ident!("{}", batch_fn);
            quote! {
                let c = #fn_name(#(#arrays),*);
                let array = Arc::new(c);
            }
        } else if types::is_primitive(&self.ret)
            && self.args.iter().all(|ty| types::is_primitive(ty))
            && self.args.len() <= 2
            && user_fn.is_pure()
            && !variadic
        {
            // SIMD optimization for primitive types
            match self.args.len() {
                0 => quote! {
                    let c = #ret_array_type::from_iter_values(
                        std::iter::repeat_with(|| #user_fn_name()).take(input.num_rows())
                    );
                    let array = Arc::new(c);
                },
                1 => quote! {
                    let c: #ret_array_type = arrow_arith::arity::unary(a0, #user_fn_name);
                    let array = Arc::new(c);
                },
                2 => quote! {
                    let c: #ret_array_type = arrow_arith::arity::binary(a0, a1, #user_fn_name)?;
                    let array = Arc::new(c);
                },
                n => todo!("SIMD optimization for {n} arguments"),
            }
        } else {
            // no optimization
            let array_zip = match children_indices.len() {
                0 => quote! { std::iter::repeat(()).take(input.num_rows()) },
                _ => quote! { itertools::multizip((#(#arrays.iter(),)*)) },
            };
            let builder = builder(&self.ret);
            // append the `output` to the `builder`
            let append_output = if user_fn.write {
                if self.ret != "varchar" && self.ret != "bytea" {
                    return Err(Error::new(
                        Span::call_site(),
                        "`&mut Write` can only be used for functions that return `varchar` or `bytea`",
                    ));
                }
                quote! {{
                    let mut writer = builder.writer();
                    if #output.is_some() {
                        writer.finish();
                    } else {
                        drop(writer);
                        builder.append_null();
                    }
                }}
            } else {
                let append = gen_append(&self.ret);
                quote! {{
                    let v = #output;
                    #append
                }}
            };
            quote! {
                let mut builder = #builder;
                for (i, (#(#inputs,)*)) in #array_zip.enumerate() {
                    #append_output
                }
                let array = Arc::new(builder.finish());
            }
        };

        let eval_and_return = if self.is_table_function {
            quote! {
                #eval
            }
        } else {
            let error_field = user_fn.has_error().then(|| {
                quote! { Field::new("error", DataType::Utf8, true), }
            });
            let let_error_builder = user_fn.has_error().then(|| {
                quote! { let mut error_builder = StringBuilder::with_capacity(input.num_rows(), input.num_rows() * 16); }
            });
            let error_array = user_fn.has_error().then(|| {
                quote! { Arc::new(error_builder.finish()) }
            });
            quote! {
                #let_error_builder
                #eval

                lazy_static! {
                    static ref SCHEMA: SchemaRef = Arc::new(Schema::new(vec![
                        Field::new(#fn_name, #ret_data_type, true),
                        #error_field
                    ]));
                }
                let opts = RecordBatchOptions::new().with_row_count(Some(input.num_rows()));
                Ok(RecordBatch::try_new_with_options(SCHEMA.clone(), vec![array, #error_array], &opts).unwrap())
            }
        };

        // downcast input arrays
        let downcast_arrays = quote! {
            #(
                let #arrays: &#arg_arrays = input.column(#children_indices).as_any().downcast_ref()
                    .ok_or_else(|| ::arrow_udf::codegen::arrow_schema::ArrowError::CastError(
                        format!("expect {} for the {}-th argument", stringify!(#arg_arrays), #children_indices)
                    ))?;
            )*
        };

        // the function body
        let body = quote! {
            use ::std::sync::Arc;
            use ::arrow_udf::{Result, Error};
            use ::arrow_udf::codegen::arrow_array::{RecordBatch, RecordBatchOptions};
            use ::arrow_udf::codegen::arrow_array::array::*;
            use ::arrow_udf::codegen::arrow_array::builder::*;
            use ::arrow_udf::codegen::arrow_schema::{Schema, SchemaRef, Field, DataType, IntervalUnit, TimeUnit};
            use ::arrow_udf::codegen::arrow_arith;
            use ::arrow_udf::codegen::arrow_schema;
            use ::arrow_udf::codegen::chrono;
            use ::arrow_udf::codegen::lazy_static::lazy_static;
            use ::arrow_udf::codegen::itertools;
            use ::arrow_udf::codegen::rust_decimal;
            use ::arrow_udf::codegen::serde_json;

            #eval_and_return
        };

        Ok(if self.is_table_function {
            quote! {
                fn #eval_fn_name<'a>(input: &'a ::arrow_udf::codegen::arrow_array::RecordBatch)
                    -> ::arrow_udf::Result<Box<dyn Iterator<Item = ::arrow_udf::codegen::arrow_array::RecordBatch> + 'a>>
                {
                    const BATCH_SIZE: usize = 1024;
                    use ::arrow_udf::codegen::genawaiter::{rc::gen, yield_};
                    #downcast_arrays
                    Ok(Box::new(gen!({ #body }).into_iter()))
                }
            }
        } else {
            quote! {
                fn #eval_fn_name(input: &::arrow_udf::codegen::arrow_array::RecordBatch)
                    -> ::arrow_udf::Result<::arrow_udf::codegen::arrow_array::RecordBatch>
                {
                    #downcast_arrays
                    #body
                }
            }
        })
    }
}

fn sig_data_type(ty: &str) -> TokenStream2 {
    match ty {
        "any" => quote! { SigDataType::Any },
        "anyarray" => quote! { SigDataType::AnyArray },
        "struct" => quote! { SigDataType::AnyStruct },
        _ => {
            let datatype = data_type(ty);
            quote! { SigDataType::Exact(#datatype) }
        }
    }
}

/// Returns a `DataType` from type name.
fn data_type(ty: &str) -> TokenStream2 {
    if let Some(ty) = ty.strip_suffix("[]") {
        let inner_type = data_type(ty);
        return quote! { arrow_schema::DataType::List(Arc::new(arrow_schema::Field::new("item", #inner_type, true))) };
    }
    if ty.starts_with("struct<") && ty.ends_with('>') {
        let fields = fields(ty);
        return quote! { arrow_schema::DataType::Struct(#fields) };
    }
    let variant: TokenStream2 = types::data_type(ty).parse().unwrap();
    quote! { arrow_schema::DataType::#variant }
}

/// Returns a `Fields` from struct type name.
fn fields(ty: &str) -> TokenStream2 {
    let fields = types::iter_fields(ty).map(|(name, ty)| {
        let ty = data_type(ty);
        quote! { arrow_schema::Field::new(#name, #ty, true) }
    });
    quote! { arrow_schema::Fields::from(vec![#(#fields,)*]) }
}

/// Generate a builder for the given type.
fn builder(ty: &str) -> TokenStream2 {
    match ty {
        "varchar" => {
            quote! { arrow_udf::codegen::StringBuilder::with_capacity(input.num_rows(), 1024) }
        }
        "bytea" => {
            quote! { arrow_udf::codegen::BinaryBuilder::with_capacity(input.num_rows(), 1024) }
        }
        "decimal" => {
            quote! { arrow_udf::codegen::LargeBinaryBuilder::with_capacity(input.num_rows(), input.num_rows() * 8) }
        }
        "json" => {
            quote! { arrow_udf::codegen::LargeStringBuilder::with_capacity(input.num_rows(), input.num_rows() * 8) }
        }
        s if s.ends_with("[]") => {
            let values_builder = builder(ty.strip_suffix("[]").unwrap());
            quote! { ListBuilder::with_capacity(#values_builder, input.num_rows()) }
        }
        s if s.starts_with("struct") => {
            let fields = fields(s);
            quote! { StructBuilder::from_fields(#fields, input.num_rows()) }
        }
        _ => {
            let builder_type = format_ident!("{}", types::array_builder_type(ty));
            quote! { #builder_type::with_capacity(input.num_rows()) }
        }
    }
}

/// Generate code to append the `v: Option<T>` to the `builder`.
fn gen_append(ty: &str) -> TokenStream2 {
    let mut append_value = gen_append_value(ty);
    if ty.ends_with("[]") {
        // the user function returns an iterator of `T`
        // we need to wrap the item with `Some`
        append_value = quote! {{
            let v = v.map(Some);
            #append_value
        }};
    }
    let append_null = gen_append_null(ty);
    quote! {
        match v {
            Some(v) => #append_value,
            None => #append_null,
        }
    }
}

/// Generate code to append the `v: T` to the `builder`.
fn gen_append_value(ty: &str) -> TokenStream2 {
    if ty.starts_with("struct") {
        let append_fields = types::iter_fields(ty).enumerate().map(|(i, (_, ty))| {
            let index = syn::Index::from(i);
            let builder_type = format_ident!("{}", types::array_builder_type(ty));
            let append = gen_append_value(ty);
            quote! {{
                let builder = builder.field_builder::<#builder_type>(#i).unwrap();
                let v = v.#index;
                #append
            }}
        });
        quote! {{
            #(#append_fields)*
            builder.append(true);
        }}
    } else if ty == "decimal" || ty == "json" {
        quote! {{
            use std::fmt::Write;
            let mut writer = builder.writer();
            write!(&mut writer, "{}", v).unwrap();
            writer.finish();
        }}
    } else if ty == "date" {
        quote! { builder.append_value(arrow_array::types::Date32Type::from_naive_date(v)) }
    } else if ty == "time" {
        quote! { builder.append_value(arrow_array::temporal_conversions::time_to_time64us(v)) }
    } else if ty == "timestamp" {
        quote! { builder.append_value(v.timestamp_micros()) }
    } else if ty == "interval" {
        quote! { builder.append_value({
            let v: arrow_udf::types::Interval = v.into();
            arrow_array::types::IntervalMonthDayNanoType::make_value(v.months, v.days, v.nanos)
        }) }
    } else if ty == "void" {
        quote! { builder.append_empty_value() }
    } else {
        quote! { builder.append_value(v) }
    }
}

/// Generate code to append null to the `builder`.
fn gen_append_null(ty: &str) -> TokenStream2 {
    if ty.starts_with("struct") {
        let append_fields = types::iter_fields(ty).enumerate().map(|(i, (_, ty))| {
            let append = gen_append_null(ty);
            let builder_type = format_ident!("{}", types::array_builder_type(ty));
            quote! {{
                let builder = builder.field_builder::<#builder_type>(#i).unwrap();
                #append
            }}
        });
        quote! {{
            #(#append_fields)*
            builder.append(false);
        }}
    } else {
        quote! { builder.append_null() }
    }
}

/// Generate code to transform the input from the type got from arrow array to the type in the user function.
///
/// | Data Type   | Arrow Value Type | User Function Type           |
/// | ----------- | ---------------- | ---------------------------- |
/// | `date`      | `i32`            | `chrono::NaiveDate`          |
/// | `time`      | `i64`            | `chrono::NaiveTime`          |
/// | `timestamp` | `i64`            | `chrono::NaiveDateTime`      |
/// | `interval`  | `i128`           | `arrow_udf::types::Interval` |
/// | `decimal`   | `&str`           | `rust_decimal::Decimal`      |
/// | `json`      | `&str`           | `serde_json::Value`          |
/// | `smallint[]`| `ArrayRef`       | `&[i16]`                     |
/// | `int[]`     | `ArrayRef`       | `&[i32]`                     |
/// | `bigint[]`  | `ArrayRef`       | `&[i64]`                     |
/// | `real[]`    | `ArrayRef`       | `&[f32]`                     |
/// | `float[]`   | `ArrayRef`       | `&[f64]`                     |
fn transform_input(input: &Ident, ty: &str) -> TokenStream2 {
    if ty == "decimal" {
        return quote! { std::str::from_utf8(#input).unwrap().parse::<rust_decimal::Decimal>().unwrap() };
    } else if ty == "date" {
        return quote! { arrow_array::types::Date32Type::to_naive_date(#input) };
    } else if ty == "time" {
        return quote! { arrow_array::temporal_conversions::as_time::<arrow_array::types::Time64MicrosecondType>(#input).unwrap() };
    } else if ty == "timestamp" {
        return quote! { arrow_array::temporal_conversions::as_datetime::<arrow_array::types::TimestampMicrosecondType>(#input).unwrap() };
    } else if ty == "interval" {
        return quote! {{
            let (months, days, nanos) = arrow_array::types::IntervalMonthDayNanoType::to_parts(#input);
            arrow_udf::types::Interval { months, days, nanos }
        }};
    } else if ty == "json" {
        return quote! { #input.parse::<serde_json::Value>().unwrap() };
    } else if let Some(elem_type) = ty.strip_suffix("[]") {
        if types::is_primitive(elem_type) {
            let array_type = format_ident!("{}", types::array_type(elem_type));
            return quote! {{
                let primitive_array: &#array_type = #input.as_primitive();
                primitive_array.values().as_ref()
            }};
        }
    }
    quote! { #input }
}

/// Encode a string to a symbol name using customized base64.
fn base64_encode(input: &str) -> String {
    use base64::{
        alphabet::Alphabet,
        engine::{general_purpose::NO_PAD, GeneralPurpose},
        Engine,
    };
    // standard base64 uses '+' and '/', which is not a valid symbol name.
    // we use '$' and '_' instead.
    let alphabet =
        Alphabet::new("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789$_").unwrap();
    let engine = GeneralPurpose::new(&alphabet, NO_PAD);
    engine.encode(input)
}
