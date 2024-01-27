mod utils;

use std::path::PathBuf;

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use wayrs_proto_parser::*;

use crate::utils::*;

#[proc_macro]
pub fn generate(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let path = syn::parse_macro_input!(input as syn::LitStr).value();
    let path = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(manifest) => {
            let mut full = PathBuf::from(manifest);
            full.push(path);
            full
        }
        None => PathBuf::from(path),
    };

    let file = std::fs::read_to_string(path).expect("could not read the file");
    let protocol = parse_protocol(&file);

    let modules = protocol.interfaces.into_iter().map(|i| gen_interface(i));

    let x = quote! { #(#modules)* };
    // {
    //     let mut file = std::fs::File::create("/tmp/test.rs").unwrap();
    //     std::io::Write::write_all(&mut file, x.to_string().as_bytes()).unwrap();
    // }
    x.into()
}

fn make_ident(name: impl AsRef<str>) -> syn::Ident {
    syn::Ident::new_raw(name.as_ref(), Span::call_site())
}

fn make_pascal_case_ident(name: impl AsRef<str>) -> syn::Ident {
    let name = name.as_ref();
    if name.chars().next().unwrap().is_ascii_digit() {
        syn::Ident::new_raw(&format!("_{name}"), Span::call_site())
    } else {
        syn::Ident::new_raw(&snake_to_pascal(name), Span::call_site())
    }
}

fn make_proxy_path(iface: impl AsRef<str>) -> TokenStream {
    let proxy_name = make_pascal_case_ident(iface);
    quote! { super::#proxy_name }
}

fn gen_interface(mut iface: Interface) -> TokenStream {
    for req in &mut iface.requests {
        req.args = req
            .args
            .iter()
            .cloned()
            .flat_map(|arg| {
                if arg.arg_type == "new_id" && arg.interface.is_none() {
                    vec![
                        Argument {
                            name: format!("{}_interface", arg.name),
                            arg_type: "string".into(),
                            allow_null: false,
                            enum_type: None,
                            interface: None,
                            summary: None,
                        },
                        Argument {
                            name: format!("{}_version", arg.name),
                            arg_type: "uint".into(),
                            allow_null: false,
                            enum_type: None,
                            interface: None,
                            summary: None,
                        },
                        Argument {
                            name: format!("{}_id", arg.name),
                            arg_type: "uint".into(),
                            allow_null: false,
                            enum_type: None,
                            interface: None,
                            summary: None,
                        },
                    ]
                } else {
                    vec![arg]
                }
            })
            .collect();
    }

    let mod_doc = gen_doc(iface.description.as_ref(), None);
    let mod_name = syn::Ident::new(&iface.name, Span::call_site());

    let proxy_name = make_pascal_case_ident(&iface.name);
    let proxy_name_str = snake_to_pascal(&iface.name);

    let raw_iface_name = &iface.name;
    let iface_version = iface.version;

    let gen_msg_gesc = |msg: &Message| {
        let args = msg.args.iter().map(map_arg_to_argtype);
        let name = &msg.name;
        let is_destructor = msg.kind.as_deref() == Some("destructor");
        quote! {
            crate::wayland_core::MessageDesc {
                name: #name,
                is_destructor: #is_destructor,
                signature: &[ #( crate::wayland_core::ArgType::#args, )* ]
            }
        }
    };
    let events_desc = iface.events.iter().map(gen_msg_gesc);
    let requests_desc = iface.requests.iter().map(gen_msg_gesc);

    let request_args_structs = iface
        .requests
        .iter()
        .filter(|request| request.args.len() > 1)
        .map(|request| {
            let struct_name = format_ident!("{}Args", make_pascal_case_ident(&request.name));
            let arg_name = request.args.iter().map(|arg| make_ident(&arg.name));
            let arg_ty = request.args.iter().map(|arg| arg.as_request_ty());
            let summary = request
                .args
                .iter()
                .map(|arg| arg.summary.as_ref().map(|s| quote!(#[doc = #s])));
            quote! {
                #[derive(Debug)]
                pub struct #struct_name { #( #summary pub #arg_name: #arg_ty, )* }
            }
        });

    let request_enum_options = iface.requests.iter().map(|request| {
        let request_name = make_pascal_case_ident(&request.name);
        let doc = gen_doc(request.description.as_ref(), Some(request.since));
        match request.args.as_slice() {
            [] => quote! { #doc #request_name },
            [_, _, ..] => {
                let struct_name = format_ident!("{request_name}Args");
                quote! { #doc #request_name(#struct_name) }
            }
            [arg] => {
                let event_ty = arg.as_request_ty();
                let arg_name = &arg.name;
                let name_doc = quote!(#[doc = #arg_name]);
                let summary = arg
                    .summary
                    .as_ref()
                    .map(|s| quote!(#[doc = "\n"] #[doc = #s]));
                quote! { #doc #request_name(#name_doc #summary #event_ty) }
            }
        }
    });

    let request_decoding = iface.requests.iter().enumerate().map(|(opcode, request)| {
        let request_name = make_pascal_case_ident(&request.name);
        let opcode = opcode as u16;
        let arg_ty = request.args.iter().map(map_arg_to_argval);
        let arg_names = request.args.iter().map(|arg| make_ident(&arg.name));
        let arg_decode = request.args.iter().map(|arg| {
            let arg_name = make_ident(&arg.name);
            match (arg.arg_type.as_str(), arg.enum_type.is_some(), arg.interface.as_deref()) {
                ("int" | "uint", true, _) => quote! {
                    match #arg_name.try_into() {
                        Ok(val) => val,
                        Err(_) => return Err(crate::wayland_core::BadMessage),
                    }
                },
                ("new_id" | "object", _, Some(iface)) => {
                    let proxy_path = make_proxy_path(iface);
                    let map = quote!{
                        match conn.get_object(#arg_name) {
                            None => return Err(crate::wayland_core::BadMessage),
                            Some(object) => match #proxy_path::try_from(object) {
                                Err(_) => return Err(crate::wayland_core::BadMessage),
                                Ok(val) => val,
                            }
                        }
                    };
                    if arg.allow_null {
                        quote!{
                            match #arg_name {
                                Some(#arg_name) => Some(#map),
                                None => None,
                            }
                        }
                    } else {
                        map
                    }
                }
                _ => quote!(#arg_name),
            }
        });
        let args_len = request.args.len();
        let retval = match args_len {
            0 => quote!(Request::#request_name),
            1 => quote!(Request::#request_name(#( #arg_decode )*)),
            _ => {
                let struct_name = format_ident!("{request_name}Args");
                let arg_names = arg_names.clone();
                quote!(Request::#request_name(#struct_name { #( #arg_names: #arg_decode, )* }))
            }
        };
        quote! {
            #opcode => {
                if msg.args.len() != #args_len {
                    return Err(crate::wayland_core::BadMessage);
                }
                let mut args = msg.args.into_iter();
                #( let Some(crate::wayland_core::ArgValue::#arg_ty(#arg_names)) = args.next() else { return Err(crate::wayland_core::BadMessage) }; )*
                Ok(#retval)
            }
        }
    });

    let events = iface
        .events
        .iter()
        .enumerate()
        .map(|(opcode, request)| gen_event_fn(opcode as u16, request));

    let enums = iface.enums.iter().map(|en| {
        let name = make_pascal_case_ident(&en.name);
        let items = en
            .items
            .iter()
            .map(|item| make_pascal_case_ident(&item.name));
        let values = en.items.iter().map(|item| item.value);
        let items2 = items.clone();
        let values2 = values.clone();
        let doc = gen_doc(en.description.as_ref(), None);
        let item_docs = en
            .items
            .iter()
            .map(|i| gen_doc(i.description.as_ref(), Some(i.since)));
        if en.is_bitfield {
            quote! {
                #doc
                #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
                pub struct #name(u32);
                impl From<#name> for u32 {
                    fn from(val: #name) -> Self {
                        val.0
                    }
                }
                impl From<u32> for #name {
                    fn from(val: u32) -> Self {
                        Self(val)
                    }
                }
                impl #name {
                    #(
                        #item_docs
                        #[allow(non_upper_case_globals)]
                        pub const #items: Self = Self(#values);
                    )*

                    pub fn empty() -> Self {
                        Self(0)
                    }
                    pub fn contains(self, item: Self) -> bool {
                        self.0 & item.0 != 0
                    }
                }
                impl ::std::ops::BitOr for #name {
                    type Output = Self;
                    fn bitor(self, rhs: Self) -> Self {
                        Self(self.0 | rhs.0)
                    }
                }
                impl ::std::ops::BitOrAssign for #name {
                    fn bitor_assign(&mut self, rhs: Self) {
                        self.0 |= rhs.0;
                    }
                }
            }
        } else {
            quote! {
                #doc
                #[repr(u32)]
                #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
                #[non_exhaustive]
                pub enum #name { #( #item_docs #items = #values, )* }
                impl From<#name> for u32 {
                    fn from(val: #name) -> u32 {
                        val as u32
                    }
                }
                impl TryFrom<u32> for #name {
                    type Error = ();
                    fn try_from(val: u32) -> ::std::result::Result<Self, ()> {
                        match val {
                            #( #values2 => Ok(Self::#items2), )*
                            _ => Err(()),
                        }
                    }
                }
            }
        }
    });

    quote! {
        #mod_doc
        pub mod #mod_name {
            use crate::wayland_core::{Proxy, ObjectId, Object};
            use crate::client::{Connection, ClientId};

            #mod_doc
            #[doc = "See [`Request`] for the list of possible requests."]
            #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
            pub struct #proxy_name {
                inner: Object
            }

            impl Proxy for #proxy_name {
                type Request = Request;

                const INTERFACE: &'static crate::wayland_core::Interface
                    = &crate::wayland_core::Interface {
                        name: match ::std::ffi::CStr::from_bytes_with_nul(concat!(#raw_iface_name, "\0").as_bytes()) {
                            Ok(name) => name,
                            Err(_) => panic!(),
                        },
                        version: #iface_version,
                        events: &[ #(#events_desc,)* ],
                        requests: &[ #(#requests_desc,)* ],
                    };

                fn as_object(&self) -> &Object {
                    &self.inner
                }

                fn parse_request(conn: &::std::rc::Rc<Connection>, msg: crate::wayland_core::Message) ->
                    ::std::result::Result<Self::Request, crate::wayland_core::BadMessage>
                {
                    match msg.header.opcode {
                        #( #request_decoding )*
                        _ => Err(crate::wayland_core::BadMessage),
                    }
                }
            }

            impl TryFrom<Object> for #proxy_name {
                type Error = crate::wayland_core::WrongObject;

                fn try_from(object: Object) -> Result<Self, Self::Error> {
                    if object.interface() == Self::INTERFACE {
                        Ok(Self{ inner: object })
                    } else {
                        Err(crate::wayland_core::WrongObject)
                    }
                }
            }

            impl ::std::fmt::Debug for #proxy_name {
                fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    write!(
                        f,
                        "{}@{}v{}",
                        #raw_iface_name,
                        self.inner.id().as_u32(),
                        self.inner.version()
                    )
                }
            }

            #( #request_args_structs )*
            #( #enums )*

            #[doc = "The request enum for [`"]
            #[doc = #proxy_name_str]
            #[doc = "`]"]
            #[derive(Debug)]
            pub enum Request {
                #( #request_enum_options, )*
            }

            impl #proxy_name {
                #( #events )*
            }
        }

        pub use #mod_name::#proxy_name;
    }
}

fn gen_pub_fn(
    attrs: &TokenStream,
    name: &str,
    generics: &[TokenStream],
    args: &[TokenStream],
    ret_ty: TokenStream,
    where_: Option<TokenStream>,
    body: TokenStream,
) -> TokenStream {
    let name = make_ident(name);
    quote! {
        #attrs
        #[allow(clippy::too_many_arguments)]
        pub fn #name<#(#generics),*>(#(#args),*) -> #ret_ty #where_ {
            #body
        }
    }
}

fn gen_event_fn(opcode: u16, event: &Message) -> TokenStream {
    let mut fn_args = vec![quote!(&self)];
    fn_args.extend(event.args.iter().map(|arg| arg.as_event_fn_arg()));

    let msg_args = event.args.iter().map(|arg| {
        let arg_name = make_ident(&arg.name);
        let arg_ty = map_arg_to_argval(arg);
        match arg.arg_type.as_str() {
            "object" | "new_id" if arg.interface.is_some() => {
                if arg.allow_null {
                    quote! { crate::wayland_core::ArgValue::#arg_ty(#arg_name.map(Proxy::id)) }
                } else {
                    quote! { crate::wayland_core::ArgValue::#arg_ty(#arg_name.id()) }
                }
            }
            _ => quote! { crate::wayland_core::ArgValue::#arg_ty(#arg_name.into()) },
        }
    });

    let extra = if event.kind.as_deref() == Some("destructor") {
        quote! { self.inner.destroy(); }
    } else {
        quote!()
    };

    let send_message = quote! {
        let conn = self.inner.conn();
        conn.send_event(
            crate::wayland_core::Message {
                header: crate::wayland_core::MessageHeader {
                    object_id: self.inner.id(),
                    size: 0,
                    opcode: #opcode,
                },
                args: vec![ #( #msg_args, )* ],
            }
        );
        #extra
    };

    let doc = gen_doc(event.description.as_ref(), Some(event.since));

    gen_pub_fn(
        &doc,
        &event.name,
        &[],
        &fn_args,
        quote!(()),
        None,
        send_message,
    )
}

fn map_arg_to_argtype(arg: &Argument) -> TokenStream {
    match arg.arg_type.as_str() {
        "int" if arg.enum_type.is_some() => quote!(Uint),
        "int" => quote!(Int),
        "uint" => quote!(Uint),
        "fixed" => quote!(Fixed),
        "object" => match arg.allow_null {
            false => quote!(Object),
            true => quote!(OptObject),
        },
        "new_id" => match &arg.interface {
            Some(iface) => {
                let proxy_name = make_proxy_path(iface);
                quote!(NewId(#proxy_name::INTERFACE))
            }
            None => quote!(AnyNewId),
        },
        "string" => match arg.allow_null {
            false => quote!(String),
            true => quote!(OptString),
        },
        "array" => quote!(Array),
        "fd" => quote!(Fd),
        _ => unreachable!(),
    }
}

fn map_arg_to_argval(arg: &Argument) -> TokenStream {
    match arg.arg_type.as_str() {
        "int" if arg.enum_type.is_some() => quote!(Uint),
        "int" => quote!(Int),
        "uint" => quote!(Uint),
        "fixed" => quote!(Fixed),
        "object" => match arg.allow_null {
            false => quote!(Object),
            true => quote!(OptObject),
        },
        "new_id" => match arg.interface.as_deref() {
            Some(_) => quote!(NewId),
            None => quote!(AnyNewId),
        },
        "string" => match arg.allow_null {
            false => quote!(String),
            true => quote!(OptString),
        },
        "array" => quote!(Array),
        "fd" => quote!(Fd),
        _ => unreachable!(),
    }
}

fn gen_doc(desc: Option<&Description>, since: Option<u32>) -> TokenStream {
    let since = since
        .map(|ver| format!("**Since version {ver}**.\n"))
        .map(|ver| quote!(#[doc = #ver]));

    let summary = desc
        .and_then(|d| d.summary.as_deref())
        .map(|s| format!("{}\n", s.trim()))
        .map(|s| quote!(#[doc = #s]));

    let text = desc
        .and_then(|d| d.text.as_deref())
        .into_iter()
        .flat_map(str::lines)
        .map(|s| format!("{}\n", s.trim()))
        .map(|s| quote!(#[doc = #s]));

    quote! {
        #summary
        #[doc = "\n"]
        #(#text)*
        #[doc = "\n"]
        #since
        #[doc = "\n"]
    }
}

trait ArgExt {
    fn as_event_fn_arg(&self) -> TokenStream;
    fn as_request_ty(&self) -> TokenStream;
}

impl ArgExt for Argument {
    fn as_event_fn_arg(&self) -> TokenStream {
        let arg_name = make_ident(&self.name);
        let retval = match (
            self.arg_type.as_str(),
            self.interface.as_deref(),
            self.enum_type.as_deref(),
            self.allow_null,
        ) {
            ("int", None, None, false) => quote!(#arg_name: i32),
            ("uint", None, None, false) => quote!(#arg_name: u32),
            ("int" | "uint", None, Some(enum_ty), false) => {
                if let Some((iface, name)) = enum_ty.split_once('.') {
                    let iface_name = syn::Ident::new(iface, Span::call_site());
                    let enum_name = make_pascal_case_ident(name);
                    quote!(#arg_name: super::#iface_name::#enum_name)
                } else {
                    let enum_name = make_pascal_case_ident(enum_ty);
                    quote!(#arg_name: #enum_name)
                }
            }
            ("fixed", None, None, false) => quote!(#arg_name: crate::wayland_core::Fixed),
            ("object", None, None, allow_null) => match allow_null {
                false => quote!(#arg_name: ObjectId),
                true => quote!(#arg_name: ::std::option::Option<ObjectId>),
            },
            ("object" | "new_id", Some(iface), None, allow_null) => {
                let proxy_path = make_proxy_path(iface);
                match allow_null {
                    false => quote!(#arg_name: &#proxy_path),
                    true => quote!(#arg_name: ::std::option::Option<&#proxy_path>),
                }
            }
            ("new_id", None, None, false) => unimplemented!(),
            ("string", None, None, allow_null) => match allow_null {
                false => quote!(#arg_name: ::std::ffi::CString),
                true => quote!(#arg_name: ::std::option::Option<::std::ffi::CString>),
            },
            ("array", None, None, false) => quote!(#arg_name: ::std::vec::Vec<u8>),
            ("fd", None, None, false) => quote!(#arg_name: ::std::os::fd::OwnedFd),
            _ => unreachable!(),
        };
        retval
    }

    fn as_request_ty(&self) -> TokenStream {
        match self.arg_type.as_str() {
            "int" | "uint" if self.enum_type.is_some() => {
                let enum_type = self.enum_type.as_deref().unwrap();
                if let Some((iface, name)) = enum_type.split_once('.') {
                    let iface_name = syn::Ident::new(iface, Span::call_site());
                    let enum_name = make_pascal_case_ident(name);
                    quote!(super::#iface_name::#enum_name)
                } else {
                    let enum_name = make_pascal_case_ident(enum_type);
                    quote!(#enum_name)
                }
            }
            "int" => quote!(i32),
            "uint" => quote!(u32),
            "fixed" => quote!(crate::wayland_core::Fixed),
            "new_id" | "object" => match (&self.interface, self.allow_null) {
                (None, false) => quote!(Object),
                (None, true) => quote!(Option<Object>),
                (Some(iface), false) => make_proxy_path(iface),
                (Some(iface), true) => {
                    let proxy = make_proxy_path(iface);
                    quote!(Option<#proxy>)
                }
            },
            "string" => match self.allow_null {
                false => quote!(::std::ffi::CString),
                true => quote!(::std::option::Option<::std::ffi::CString>),
            },
            "array" => quote!(::std::vec::Vec<u8>),
            "fd" => quote!(::std::os::fd::OwnedFd),
            _ => unreachable!(),
        }
    }
}
