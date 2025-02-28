/*
 * Copyright (c) godot-rust; Bromeon and contributors.
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use crate::util;
use crate::util::ident;
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};

/// Information used for registering a Rust function with Godot.
pub struct FuncDefinition {
    /// Raw information about the Rust function.
    pub func: venial::Function,
    /// The function's non-gdext attributes (all except #[func]).
    pub external_attributes: Vec<venial::Attribute>,
    /// The name the function will be exposed as in Godot. If `None`, the Rust function name is used.
    pub rename: Option<String>,
    pub has_gd_self: bool,
}

/// Returns a C function which acts as the callback when a virtual method of this instance is invoked.
//
// There are currently no virtual static methods. Additionally, virtual static methods dont really make a lot
// of sense. Therefore there is no need to support them.
pub fn make_virtual_callback(
    class_name: &Ident,
    signature_info: SignatureInfo,
    before_kind: BeforeKind,
) -> TokenStream {
    let method_name = &signature_info.method_name;

    let wrapped_method = make_forwarding_closure(class_name, &signature_info, before_kind);
    let sig_tuple =
        util::make_signature_tuple_type(&signature_info.ret_type, &signature_info.param_types);

    let invocation = make_ptrcall_invocation(method_name, &sig_tuple, &wrapped_method, true);

    quote! {
        {
            use ::godot::sys;

            unsafe extern "C" fn function(
                instance_ptr: sys::GDExtensionClassInstancePtr,
                args_ptr: *const sys::GDExtensionConstTypePtr,
                ret: sys::GDExtensionTypePtr,
            ) {
                #invocation;
            }
            Some(function)
        }
    }
}

/// Generates code that registers the specified method for the given class.
pub fn make_method_registration(
    class_name: &Ident,
    func_definition: FuncDefinition,
) -> TokenStream {
    let signature_info = get_signature_info(&func_definition.func, func_definition.has_gd_self);
    let sig_tuple =
        util::make_signature_tuple_type(&signature_info.ret_type, &signature_info.param_types);

    let method_name = &signature_info.method_name;
    let param_idents = &signature_info.param_idents;

    let method_flags = make_method_flags(signature_info.receiver_type);

    let forwarding_closure =
        make_forwarding_closure(class_name, &signature_info, BeforeKind::Without);

    let varcall_func = make_varcall_func(method_name, &sig_tuple, &forwarding_closure);
    let ptrcall_func = make_ptrcall_func(method_name, &sig_tuple, &forwarding_closure);

    // String literals
    let class_name_str = class_name.to_string();
    let method_name_str = if let Some(rename) = func_definition.rename {
        rename
    } else {
        method_name.to_string()
    };
    let param_ident_strs = param_idents.iter().map(|ident| ident.to_string());

    // Transport #[cfg] attrs to the FFI glue to ensure functions which were conditionally
    // removed from compilation don't cause errors.
    let cfg_attrs = util::extract_cfg_attrs(&func_definition.external_attributes)
        .into_iter()
        .collect::<Vec<_>>();

    quote! {
        #(#cfg_attrs)*
        {
            use ::godot::obj::GodotClass;
            use ::godot::builtin::meta::registration::method::ClassMethodInfo;
            use ::godot::builtin::{StringName, Variant};
            use ::godot::sys;

            type Sig = #sig_tuple;

            let method_name = StringName::from(#method_name_str);

            let varcall_func = #varcall_func;
            let ptrcall_func = #ptrcall_func;

            // SAFETY:
            // `get_varcall_func` upholds all the requirements for `call_func`.
            // `get_ptrcall_func` upholds all the requirements for `ptrcall_func`
            let method_info = unsafe {
                ClassMethodInfo::from_signature::<Sig>(
                #class_name::class_name(),
                method_name,
                Some(varcall_func),
                Some(ptrcall_func),
                #method_flags,
                &[
                    #( #param_ident_strs ),*
                ],
                Vec::new()
                )
            };

            ::godot::private::out!(
                "   Register fn:   {}::{}",
                #class_name_str,
                #method_name_str
            );


            method_info.register_extension_class_method();
        };
    }
}

// ----------------------------------------------------------------------------------------------------------------------------------------------
// Implementation

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum ReceiverType {
    Ref,
    Mut,
    GdSelf,
    Static,
}

pub struct SignatureInfo {
    pub method_name: Ident,
    pub receiver_type: ReceiverType,
    pub param_idents: Vec<Ident>,
    pub param_types: Vec<venial::TyExpr>,
    pub ret_type: TokenStream,
}

impl SignatureInfo {
    pub fn fn_ready() -> Self {
        Self {
            method_name: ident("ready"),
            receiver_type: ReceiverType::Mut,
            param_idents: vec![],
            param_types: vec![],
            ret_type: quote! { () },
        }
    }
}

pub enum BeforeKind {
    /// Default: just call the method.
    Without,

    /// Call `before_{method}` before calling the method itself.
    WithBefore,

    /// Call **only** `before_{method}`, not the method itself.
    OnlyBefore,
}

/// Returns a closure expression that forwards the parameters to the Rust instance.
fn make_forwarding_closure(
    class_name: &Ident,
    signature_info: &SignatureInfo,
    before_kind: BeforeKind,
) -> TokenStream {
    let method_name = &signature_info.method_name;
    let params = &signature_info.param_idents;

    let instance_decl = match &signature_info.receiver_type {
        ReceiverType::Ref => quote! {
            let instance = ::godot::private::Storage::get(storage);
        },
        ReceiverType::Mut => quote! {
            let mut instance = ::godot::private::Storage::get_mut(storage);
        },
        _ => quote! {},
    };

    let before_method_call = match before_kind {
        BeforeKind::WithBefore | BeforeKind::OnlyBefore => {
            let before_method = format_ident!("__before_{}", method_name);
            quote! { instance.#before_method(); }
        }
        BeforeKind::Without => TokenStream::new(),
    };

    match signature_info.receiver_type {
        ReceiverType::Ref | ReceiverType::Mut => {
            // Generated default virtual methods (e.g. for ready) may not have an actual implementation (user code), so
            // all they need to do is call the __before_ready() method. This means the actual method call may be optional.
            let method_call = if matches!(before_kind, BeforeKind::OnlyBefore) {
                TokenStream::new()
            } else {
                quote! { instance.#method_name(#(#params),*) }
            };

            quote! {
                |instance_ptr, params| {
                    let ( #(#params,)* ) = params;

                    let storage =
                        unsafe { ::godot::private::as_storage::<#class_name>(instance_ptr) };

                    #instance_decl
                    #before_method_call
                    #method_call
                }
            }
        }
        ReceiverType::GdSelf => {
            // Method call is always present, since GdSelf implies that the user declares the method.
            // (Absent method is only used in the case of a generated default virtual method, e.g. for ready()).
            quote! {
                |instance_ptr, params| {
                    let ( #(#params,)* ) = params;

                    let storage =
                        unsafe { ::godot::private::as_storage::<#class_name>(instance_ptr) };

                    #before_method_call
                    <#class_name>::#method_name(::godot::private::Storage::get_gd(storage), #(#params),*)
                }
            }
        }
        ReceiverType::Static => {
            // No before-call needed, since static methods are not virtual.
            quote! {
                |_, params| {
                    let ( #(#params,)* ) = params;
                    <#class_name>::#method_name(#(#params),*)
                }
            }
        }
    }
}

pub(crate) fn get_signature_info(signature: &venial::Function, has_gd_self: bool) -> SignatureInfo {
    let method_name = signature.name.clone();
    let mut receiver_type = if has_gd_self {
        ReceiverType::GdSelf
    } else {
        ReceiverType::Static
    };
    let mut param_idents: Vec<Ident> = Vec::new();
    let mut param_types = Vec::new();
    let ret_type = match &signature.return_ty {
        None => quote! { () },
        Some(ty) => quote! { #ty },
    };

    let mut next_unnamed_index = 0;
    for (arg, _) in &signature.params.inner {
        match arg {
            venial::FnParam::Receiver(recv) => {
                if receiver_type == ReceiverType::GdSelf {
                    // This shouldn't happen, as when has_gd_self is true the first function parameter should have been removed.
                    // And the first parameter should be the only one that can be a Receiver.
                    panic!("has_gd_self is true for a signature starting with a Receiver param.");
                }
                receiver_type = if recv.tk_mut.is_some() {
                    ReceiverType::Mut
                } else if recv.tk_ref.is_some() {
                    ReceiverType::Ref
                } else {
                    panic!("Receiver not supported");
                };
            }
            venial::FnParam::Typed(arg) => {
                // Parameter will be forwarded as an argument to the instance, so we need to give `_` a name.
                let ident = if arg.name == "_" {
                    let ident = format_ident!("__unnamed_{next_unnamed_index}");
                    next_unnamed_index += 1;
                    ident
                } else {
                    arg.name.clone()
                };
                let ty = arg.ty.clone();

                param_types.push(ty);
                param_idents.push(ident);
            }
        }
    }

    SignatureInfo {
        method_name,
        receiver_type,
        param_idents,
        param_types,
        ret_type,
    }
}

fn make_method_flags(method_type: ReceiverType) -> TokenStream {
    match method_type {
        ReceiverType::Ref | ReceiverType::Mut | ReceiverType::GdSelf => {
            quote! { ::godot::engine::global::MethodFlags::DEFAULT }
        }
        ReceiverType::Static => {
            quote! { ::godot::engine::global::MethodFlags::STATIC }
        }
    }
}

/// Generate code for a C FFI function that performs a varcall.
fn make_varcall_func(
    method_name: &Ident,
    sig_tuple: &TokenStream,
    wrapped_method: &TokenStream,
) -> TokenStream {
    let invocation = make_varcall_invocation(method_name, sig_tuple, wrapped_method);
    let method_name_str = method_name.to_string();

    quote! {
        {
            unsafe extern "C" fn function(
                _method_data: *mut std::ffi::c_void,
                instance_ptr: sys::GDExtensionClassInstancePtr,
                args_ptr: *const sys::GDExtensionConstVariantPtr,
                _arg_count: sys::GDExtensionInt,
                ret: sys::GDExtensionVariantPtr,
                err: *mut sys::GDExtensionCallError,
            ) {
                let success = ::godot::private::handle_panic(
                    || #method_name_str,
                    || #invocation
                );

                if success.is_none() {
                    // Signal error and set return type to Nil
                    (*err).error = sys::GDEXTENSION_CALL_ERROR_INVALID_METHOD; // no better fitting enum?

                    // TODO(uninit)
                    sys::interface_fn!(variant_new_nil)(sys::AsUninit::as_uninit(ret));
                }
            }

            function
        }
    }
}

/// Generate code for a C FFI function that performs a ptrcall.
fn make_ptrcall_func(
    method_name: &Ident,
    sig_tuple: &TokenStream,
    wrapped_method: &TokenStream,
) -> TokenStream {
    let invocation = make_ptrcall_invocation(method_name, sig_tuple, wrapped_method, false);
    let method_name_str = method_name.to_string();

    quote! {
        {
            unsafe extern "C" fn function(
                _method_data: *mut std::ffi::c_void,
                instance_ptr: sys::GDExtensionClassInstancePtr,
                args_ptr: *const sys::GDExtensionConstTypePtr,
                ret: sys::GDExtensionTypePtr,
            ) {
                let success = ::godot::private::handle_panic(
                    || #method_name_str,
                    || #invocation
                );

                if success.is_none() {
                    // TODO set return value to T::default()?
                }
            }

            function
        }
    }
}

/// Generate code for a `ptrcall` call expression.
fn make_ptrcall_invocation(
    method_name: &Ident,
    sig_tuple: &TokenStream,
    wrapped_method: &TokenStream,
    is_virtual: bool,
) -> TokenStream {
    let method_name_str = method_name.to_string();

    let ptrcall_type = if is_virtual {
        quote! { sys::PtrcallType::Virtual }
    } else {
        quote! { sys::PtrcallType::Standard }
    };

    quote! {
         <#sig_tuple as ::godot::builtin::meta::PtrcallSignatureTuple>::in_ptrcall(
            instance_ptr,
            #method_name_str,
            args_ptr,
            ret,
            #wrapped_method,
            #ptrcall_type,
        )
    }
}

/// Generate code for a `varcall()` call expression.
fn make_varcall_invocation(
    method_name: &Ident,
    sig_tuple: &TokenStream,
    wrapped_method: &TokenStream,
) -> TokenStream {
    let method_name_str = method_name.to_string();

    quote! {
        <#sig_tuple as ::godot::builtin::meta::VarcallSignatureTuple>::in_varcall(
            instance_ptr,
            #method_name_str,
            args_ptr,
            ret,
            err,
            #wrapped_method,
        )
    }
}
