//! The checks the build script runs on the `google.api.http` options
//! (`js_api_gateway::annotation::routes`), over the test descriptors under `tests/protos` and
//! over the job service itself.

mod common;

use js_api_gateway::Verb;
use js_api_gateway::annotation::{AnnotationError, Fault, RouteSpec, Segment, Template, routes};

use common::fixture;

/// The route of a method of `gatewaytest.Good`.
fn good(rpc: &str, verb: Verb, template: &str, body: bool, status: u16) -> RouteSpec {
    RouteSpec {
        rpc: rpc.to_owned(),
        verb,
        template: template.to_owned(),
        body,
        status,
        input: "gatewaytest.Req".to_owned(),
        output: "gatewaytest.Resp".to_owned(),
    }
}

/// Asserts that the routes of a service are its annotated methods, in order, each as its
/// option declares it, and that the unannotated method `Hidden` has none. Catches a method
/// without the option that is given a default route.
#[test]
fn routes_are_the_annotated_methods() {
    let pool = fixture("gatewaytest/good.proto");
    assert_eq!(
        routes(&pool, "gatewaytest.Good"),
        Ok(vec![
            good("Read", Verb::Get, "/v1/things/{id}", false, 200),
            good("List", Verb::Get, "/v1/things", false, 200),
            good("Act", Verb::Post, "/v1/things/{id}:act", true, 201),
            good(
                "Other",
                Verb::Post,
                "/v1/things/{other_id}:other",
                false,
                200
            ),
        ])
    );
    assert_eq!(
        routes(&pool, "gatewaytest.Missing"),
        Err(AnnotationError::NoService("gatewaytest.Missing".to_owned()))
    );
}

/// Asserts that each option the gateway does not serve fails with the method's name and
/// its own fault, whose whole text is given: a path variable that names no field (or a
/// field that is not a singular string, or appears twice), two methods on one method and
/// path, a path outside `/v1/`, a route under `/v1/leases` or `/v1/agent` (or with a
/// variable as its resource), a method other than GET and POST, no method, additional
/// bindings, a response body, a body on a GET or a named body, a template outside the
/// grammar, a success status other than 200 and 201, a streaming method, a request or
/// response that is not a top-level message of the service's package (another package's,
/// a nested one), and an option that is not an `HttpRule`; and that a service with no
/// method at all, or with methods none of which is annotated, fails with the service's
/// name. Catches any of them accepted.
#[test]
fn build_rejects_a_bad_annotation() {
    let pool = fixture("gatewaytest/bad.proto");
    let template = "its path is not literal segments of [a-z0-9._-] and {field} variables, \
                    with an optional :verb of lower-case letters after a final variable";
    let reserved =
        "its path names no resource of its own after /v1/ (a literal other than leases and agent)";
    let cases: [(&str, &str, String); 28] = [
        (
            "Put",
            "Call",
            "its route is PUT /v1/things; the gateway serves GET and POST only".to_owned(),
        ),
        (
            "Delete",
            "Call",
            "its route is DELETE /v1/things/{id}; the gateway serves GET and POST only".to_owned(),
        ),
        (
            "Patch",
            "Call",
            "its route is PATCH /v1/things; the gateway serves GET and POST only".to_owned(),
        ),
        (
            "Custom",
            "Call",
            "its route is HEAD /v1/things; the gateway serves GET and POST only".to_owned(),
        ),
        (
            "NoMethod",
            "Call",
            "its route names no HTTP method".to_owned(),
        ),
        (
            "Additional",
            "Call",
            "it has additional_bindings; the gateway serves one route per method".to_owned(),
        ),
        (
            "ResponseBody",
            "Call",
            "it has a response_body; the gateway answers the whole response".to_owned(),
        ),
        (
            "BodyOnGet",
            "Call",
            "it is a GET route with a body".to_owned(),
        ),
        (
            "NamedBody",
            "Call",
            "its body is `inner`; the gateway takes the whole request (`*`) or no body".to_owned(),
        ),
        ("OutsideV1", "Call", "its path is not under /v1/".to_owned()),
        ("EmptySegment", "Call", template.to_owned()),
        ("BadVariable", "Call", template.to_owned()),
        ("BadLiteral", "Call", template.to_owned()),
        ("VerbAfterLiteral", "Call", template.to_owned()),
        ("BadVerb", "Call", template.to_owned()),
        ("Leases", "Call", reserved.to_owned()),
        ("Agent", "Call", reserved.to_owned()),
        ("VariableResource", "Call", reserved.to_owned()),
        (
            "UnknownVariable",
            "Call",
            "its path variable nope names no field of the request".to_owned(),
        ),
        (
            "NotAString",
            "Call",
            "its path variable count is not a singular string field".to_owned(),
        ),
        (
            "RepeatedString",
            "Call",
            "its path variable tags is not a singular string field".to_owned(),
        ),
        (
            "RepeatedVariable",
            "Call",
            "its path variable id appears twice".to_owned(),
        ),
        (
            "BadStatus",
            "Call",
            "its http_success_status is 204; the gateway answers a success 200 or 201".to_owned(),
        ),
        (
            "ClientStream",
            "Call",
            "it streams; the gateway serves unary methods only".to_owned(),
        ),
        (
            "ServerStream",
            "Call",
            "it streams; the gateway serves unary methods only".to_owned(),
        ),
        (
            "Foreign",
            "Call",
            "its message google.protobuf.Empty is not a top-level message of the service's \
             package"
                .to_owned(),
        ),
        (
            "Nested",
            "Call",
            "its message gatewaytest.Holder.Inside is not a top-level message of the \
             service's package"
                .to_owned(),
        ),
        (
            "Duplicate",
            "Second",
            "its method and path are those of First".to_owned(),
        ),
    ];
    for (service, rpc, text) in cases {
        let err =
            routes(&pool, &format!("gatewaytest.{service}")).expect_err("the option is refused");
        assert_eq!(err.to_string(), format!("{rpc}: {text}"), "{service}");
        assert!(matches!(err, AnnotationError::Rpc { .. }), "{service}");
    }

    for service in ["gatewaytest.Empty", "gatewaytest.Unannotated"] {
        let err = routes(&pool, service).expect_err("a service without routes is refused");
        assert_eq!(err, AnnotationError::NoRoutes(service.to_owned()));
        assert_eq!(
            err.to_string(),
            format!("no method of {service} carries a google.api.http option")
        );
    }

    let garbled = fixture("gatewaytest/garbled.proto");
    assert_eq!(
        routes(&garbled, "garbled.Garbled"),
        Err(AnnotationError::Rpc {
            rpc: "Call".to_owned(),
            fault: Fault::Undecodable,
        })
    );
    assert_eq!(
        Fault::Undecodable.to_string(),
        "its google.api.http option does not decode as a google.api.HttpRule"
    );
    assert_eq!(
        AnnotationError::NoService("x.Y".to_owned()).to_string(),
        "the descriptor set has no service x.Y"
    );
}

/// Asserts how a template parses: its segments and verb, its variables and its shape (the
/// form two templates are compared in).
#[test]
fn a_template_parses_into_segments_and_a_verb() {
    let parsed = Template::parse("/v1/jobs/{job_id}:cancel").expect("the template parses");
    assert_eq!(
        parsed,
        Template {
            segments: vec![
                Segment::Literal("v1".to_owned()),
                Segment::Literal("jobs".to_owned()),
                Segment::Variable("job_id".to_owned()),
            ],
            verb: Some("cancel".to_owned()),
        }
    );
    assert_eq!(parsed.variables().collect::<Vec<_>>(), ["job_id"]);
    assert_eq!(parsed.shape(), "/v1/jobs/{}:cancel");
    let plain = Template::parse("/v1/a.b/c_d-1/{x}").expect("the template parses");
    assert_eq!(plain.shape(), "/v1/a.b/c_d-1/{}");
    assert_eq!(Template::parse("/v1/jobs/{}"), Err(Fault::Template));
}

/// Asserts that the job service's own options pass the checks and give its twelve routes,
/// with the verbs the crate documents (a change is a POST, a read a GET) and the success
/// statuses (201 for a submit, 200 for the rest).
#[test]
fn the_job_service_routes_pass_the_checks() {
    let pool = prost_reflect::DescriptorPool::decode(js_proto::FILE_DESCRIPTOR_SET)
        .expect("the descriptor set decodes");
    let routes = routes(&pool, "jobscheduler.v1.JobService").expect("the options pass");
    let table: Vec<(&str, &str, &str, bool, u16)> = routes
        .iter()
        .map(|route| {
            (
                route.rpc.as_str(),
                route.verb.as_str(),
                route.template.as_str(),
                route.body,
                route.status,
            )
        })
        .collect();
    assert_eq!(
        table,
        [
            ("SubmitJob", "POST", "/v1/jobs", true, 201),
            ("GetJob", "GET", "/v1/jobs/{job_id}", false, 200),
            ("ListJobs", "GET", "/v1/jobs", false, 200),
            ("CancelJob", "POST", "/v1/jobs/{job_id}:cancel", true, 200),
            ("GetResult", "GET", "/v1/jobs/{job_id}/result", false, 200),
            ("GetLogs", "GET", "/v1/jobs/{job_id}/logs", false, 200),
            ("ListHosts", "GET", "/v1/hosts", false, 200),
            (
                "CordonHost",
                "POST",
                "/v1/hosts/{host_id}:cordon",
                true,
                200
            ),
            (
                "UncordonHost",
                "POST",
                "/v1/hosts/{host_id}:uncordon",
                true,
                200
            ),
            ("ListPools", "GET", "/v1/pools", false, 200),
            ("ListProviders", "GET", "/v1/providers", false, 200),
            ("GetUsage", "GET", "/v1/usage", false, 200),
        ]
    );
}

/// Asserts that the route table of `docs/http-gateway.md` is `ROUTES`, row for row, so the
/// page cannot fall behind the options.
#[test]
fn the_docs_list_every_route() {
    let page = include_str!("../../../docs/http-gateway.md");
    let rows: Vec<String> = js_api_gateway::ROUTES
        .iter()
        .map(|route| {
            format!(
                "| `{} {}` | `{}` | {} |",
                route.verb.as_str(),
                route.template,
                route.rpc.name(),
                route.status
            )
        })
        .collect();
    let documented: Vec<&str> = page
        .lines()
        .filter(|line| line.starts_with("| `GET ") || line.starts_with("| `POST "))
        .collect();
    assert_eq!(documented, rows);
}
