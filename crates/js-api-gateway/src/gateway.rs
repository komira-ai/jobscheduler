// Generated from the google.api.http options of jobscheduler.v1.JobService by src/codegen.rs. The
// build script and the test generated_source_is_current fail when this file differs
// from what src/codegen.rs writes; do not edit it by hand.

/// A method of `jobscheduler.v1.JobService` that has a route: one per annotated method, in the
/// order of the service definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rpc {
    /// `SubmitJob`, served at `POST /v1/jobs`.
    SubmitJob,
    /// `GetJob`, served at `GET /v1/jobs/{job_id}`.
    GetJob,
    /// `ListJobs`, served at `GET /v1/jobs`.
    ListJobs,
    /// `CancelJob`, served at `POST /v1/jobs/{job_id}:cancel`.
    CancelJob,
    /// `GetResult`, served at `GET /v1/jobs/{job_id}/result`.
    GetResult,
    /// `GetLogs`, served at `GET /v1/jobs/{job_id}/logs`.
    GetLogs,
    /// `ListHosts`, served at `GET /v1/hosts`.
    ListHosts,
    /// `CordonHost`, served at `POST /v1/hosts/{host_id}:cordon`.
    CordonHost,
    /// `UncordonHost`, served at `POST /v1/hosts/{host_id}:uncordon`.
    UncordonHost,
    /// `ListPools`, served at `GET /v1/pools`.
    ListPools,
    /// `ListProviders`, served at `GET /v1/providers`.
    ListProviders,
    /// `GetUsage`, served at `GET /v1/usage`.
    GetUsage,
}

impl Rpc {
    /// The method's name in the service definition, such as `GetJob`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SubmitJob => "SubmitJob",
            Self::GetJob => "GetJob",
            Self::ListJobs => "ListJobs",
            Self::CancelJob => "CancelJob",
            Self::GetResult => "GetResult",
            Self::GetLogs => "GetLogs",
            Self::ListHosts => "ListHosts",
            Self::CordonHost => "CordonHost",
            Self::UncordonHost => "UncordonHost",
            Self::ListPools => "ListPools",
            Self::ListProviders => "ListProviders",
            Self::GetUsage => "GetUsage",
        }
    }
}

/// The routes of the gateway, one per annotated method of `jobscheduler.v1.JobService`, in the
/// order of the service definition.
pub const ROUTES: &[Route] = &[
    Route {
        verb: Verb::Post,
        template: "/v1/jobs",
        rpc: Rpc::SubmitJob,
        body: true,
        status: 201,
        input: "jobscheduler.v1.SubmitJobRequest",
        output: "jobscheduler.v1.SubmitJobResponse",
    },
    Route {
        verb: Verb::Get,
        template: "/v1/jobs/{job_id}",
        rpc: Rpc::GetJob,
        body: false,
        status: 200,
        input: "jobscheduler.v1.GetJobRequest",
        output: "jobscheduler.v1.GetJobResponse",
    },
    Route {
        verb: Verb::Get,
        template: "/v1/jobs",
        rpc: Rpc::ListJobs,
        body: false,
        status: 200,
        input: "jobscheduler.v1.ListJobsRequest",
        output: "jobscheduler.v1.ListJobsResponse",
    },
    Route {
        verb: Verb::Post,
        template: "/v1/jobs/{job_id}:cancel",
        rpc: Rpc::CancelJob,
        body: true,
        status: 200,
        input: "jobscheduler.v1.CancelJobRequest",
        output: "jobscheduler.v1.CancelJobResponse",
    },
    Route {
        verb: Verb::Get,
        template: "/v1/jobs/{job_id}/result",
        rpc: Rpc::GetResult,
        body: false,
        status: 200,
        input: "jobscheduler.v1.GetResultRequest",
        output: "jobscheduler.v1.GetResultResponse",
    },
    Route {
        verb: Verb::Get,
        template: "/v1/jobs/{job_id}/logs",
        rpc: Rpc::GetLogs,
        body: false,
        status: 200,
        input: "jobscheduler.v1.GetLogsRequest",
        output: "jobscheduler.v1.GetLogsResponse",
    },
    Route {
        verb: Verb::Get,
        template: "/v1/hosts",
        rpc: Rpc::ListHosts,
        body: false,
        status: 200,
        input: "jobscheduler.v1.ListHostsRequest",
        output: "jobscheduler.v1.ListHostsResponse",
    },
    Route {
        verb: Verb::Post,
        template: "/v1/hosts/{host_id}:cordon",
        rpc: Rpc::CordonHost,
        body: true,
        status: 200,
        input: "jobscheduler.v1.CordonHostRequest",
        output: "jobscheduler.v1.CordonHostResponse",
    },
    Route {
        verb: Verb::Post,
        template: "/v1/hosts/{host_id}:uncordon",
        rpc: Rpc::UncordonHost,
        body: true,
        status: 200,
        input: "jobscheduler.v1.UncordonHostRequest",
        output: "jobscheduler.v1.UncordonHostResponse",
    },
    Route {
        verb: Verb::Get,
        template: "/v1/pools",
        rpc: Rpc::ListPools,
        body: false,
        status: 200,
        input: "jobscheduler.v1.ListPoolsRequest",
        output: "jobscheduler.v1.ListPoolsResponse",
    },
    Route {
        verb: Verb::Get,
        template: "/v1/providers",
        rpc: Rpc::ListProviders,
        body: false,
        status: 200,
        input: "jobscheduler.v1.ListProvidersRequest",
        output: "jobscheduler.v1.ListProvidersResponse",
    },
    Route {
        verb: Verb::Get,
        template: "/v1/usage",
        rpc: Rpc::GetUsage,
        body: false,
        status: 200,
        input: "jobscheduler.v1.GetUsageRequest",
        output: "jobscheduler.v1.GetUsageResponse",
    },
];

/// Calls the method `rpc` of `service` with `input`, a message of its request type,
/// carrying `extensions` (those of the HTTP request, which the mounting server's
/// layers may have set), and answers its response as a message of `output` (see
/// [`decode_as`] and [`answer_of`]).
async fn call<T: ::js_proto::v1::job_service_server::JobService>(
    service: &T,
    rpc: Rpc,
    input: ::prost_reflect::DynamicMessage,
    output: &::prost_reflect::MessageDescriptor,
    extensions: ::tonic::Extensions,
) -> ::core::result::Result<::prost_reflect::DynamicMessage, Answer> {
    match rpc {
        Rpc::SubmitJob => {
            let request = decode_as::<::js_proto::v1::SubmitJobRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.submit_job(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::GetJob => {
            let request = decode_as::<::js_proto::v1::GetJobRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.get_job(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::ListJobs => {
            let request = decode_as::<::js_proto::v1::ListJobsRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.list_jobs(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::CancelJob => {
            let request = decode_as::<::js_proto::v1::CancelJobRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.cancel_job(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::GetResult => {
            let request = decode_as::<::js_proto::v1::GetResultRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.get_result(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::GetLogs => {
            let request = decode_as::<::js_proto::v1::GetLogsRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.get_logs(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::ListHosts => {
            let request = decode_as::<::js_proto::v1::ListHostsRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.list_hosts(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::CordonHost => {
            let request = decode_as::<::js_proto::v1::CordonHostRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.cordon_host(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::UncordonHost => {
            let request = decode_as::<::js_proto::v1::UncordonHostRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.uncordon_host(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::ListPools => {
            let request = decode_as::<::js_proto::v1::ListPoolsRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.list_pools(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::ListProviders => {
            let request = decode_as::<::js_proto::v1::ListProvidersRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.list_providers(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
        Rpc::GetUsage => {
            let request = decode_as::<::js_proto::v1::GetUsageRequest>(&input)?;
            let request = ::tonic::Request::from_parts(::tonic::metadata::MetadataMap::new(), extensions, request);
            let response = service.get_usage(request).await;
            answer_of(output, response.map(|response| ::prost::Message::encode_to_vec(response.get_ref())))
        }
    }
}
