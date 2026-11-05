//! The Rust the gateway is generated as: [`render`] writes the enum `Rpc`, the table
//! `ROUTES` and the function `call` for the routes [`crate::annotation::routes`] read. The
//! output is checked in as `src/gateway.rs`, which `src/lib.rs` includes, so the coverage
//! report measures it as part of the crate; the build script and the crate's tests fail when
//! the file differs from what this module writes for the job service's options.
//!
//! Names follow the generated code of js-proto: a method of the server trait is the method's
//! name in snake case and a message type its name in upper camel case, both by `heck` as
//! prost and tonic write them (`GetHTTPUsage` is `get_http_usage`, `HTTPRequest` is
//! `HttpRequest`). The build script includes this file as a module of its own.

use heck::{ToSnakeCase, ToUpperCamelCase};

use crate::annotation::{RouteSpec, Verb};

/// The Rust module of the job service's messages and server trait.
const MODULE: &str = "::js_proto::v1";

/// The name the generated server trait gives the method `rpc`: `GetJob` is `get_job`.
#[must_use]
pub fn method_name(rpc: &str) -> String {
    rpc.to_snake_case()
}

/// The Rust type of the top-level message `full_name`: its last component in upper camel
/// case, in [`MODULE`].
#[must_use]
pub fn message_type(full_name: &str) -> String {
    let name = full_name.rsplit('.').next().unwrap_or(full_name);
    format!("{MODULE}::{}", name.to_upper_camel_case())
}

/// The Rust spelling of a verb in the generated table.
fn verb(verb: Verb) -> &'static str {
    match verb {
        Verb::Get => "Verb::Get",
        Verb::Post => "Verb::Post",
    }
}

/// The text of `src/gateway.rs` for the routes of `service` (a full name).
#[must_use]
pub fn render(service: &str, routes: &[RouteSpec]) -> String {
    let mut code = String::new();
    code.push_str(&format!(
        "// Generated from the google.api.http options of {service} by src/codegen.rs. The\n\
         // build script and the test generated_source_is_current fail when this file differs\n\
         // from what src/codegen.rs writes; do not edit it by hand.\n\n"
    ));
    code.push_str(&format!(
        "/// A method of `{service}` that has a route: one per annotated method, in the\n\
         /// order of the service definition.\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
         pub enum Rpc {{\n"
    ));
    for route in routes {
        code.push_str(&format!(
            "    /// `{}`, served at `{} {}`.\n    {},\n",
            route.rpc,
            route.verb.as_str(),
            route.template,
            route.rpc.to_upper_camel_case()
        ));
    }
    code.push_str(
        "}\n\n\
         impl Rpc {\n    \
         /// The method's name in the service definition, such as `GetJob`.\n    \
         #[must_use]\n    \
         pub const fn name(self) -> &'static str {\n        \
         match self {\n",
    );
    for route in routes {
        code.push_str(&format!(
            "            Self::{} => {:?},\n",
            route.rpc.to_upper_camel_case(),
            route.rpc
        ));
    }
    code.push_str(&format!(
        "        }}\n    }}\n}}\n\n\
         /// The routes of the gateway, one per annotated method of `{service}`, in the\n\
         /// order of the service definition.\n\
         pub const ROUTES: &[Route] = &[\n"
    ));
    for route in routes {
        code.push_str(&format!(
            "    Route {{\n        \
             verb: {},\n        \
             template: {:?},\n        \
             rpc: Rpc::{},\n        \
             body: {},\n        \
             status: {},\n        \
             input: {:?},\n        \
             output: {:?},\n    \
             }},\n",
            verb(route.verb),
            route.template,
            route.rpc.to_upper_camel_case(),
            route.body,
            route.status,
            route.input,
            route.output
        ));
    }
    code.push_str(&format!(
        "];\n\n\
         /// Calls the method `rpc` of `service` with `input`, a message of its request type,\n\
         /// and answers its response as a message of `output` (see [`decode_as`] and\n\
         /// [`answer_of`]).\n\
         async fn call<T: {MODULE}::job_service_server::JobService>(\n    \
         service: &T,\n    \
         rpc: Rpc,\n    \
         input: ::prost_reflect::DynamicMessage,\n    \
         output: &::prost_reflect::MessageDescriptor,\n\
         ) -> ::core::result::Result<::prost_reflect::DynamicMessage, Answer> {{\n    \
         match rpc {{\n"
    ));
    for route in routes {
        code.push_str(&format!(
            "        Rpc::{} => {{\n            \
             let request = decode_as::<{}>(&input)?;\n            \
             let response = service.{}(::tonic::Request::new(request)).await;\n            \
             answer_of(output, response.map(|response| \
             ::prost::Message::encode_to_vec(response.get_ref())))\n        \
             }}\n",
            route.rpc.to_upper_camel_case(),
            message_type(&route.input),
            method_name(&route.rpc)
        ));
    }
    code.push_str("    }\n}\n");
    code
}
