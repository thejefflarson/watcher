//! OTLP/gRPC server (:4317). Thin wrappers over the shared `otlp::store_*` functions.

use std::net::SocketAddr;
use std::sync::Arc;

use opentelemetry_proto::tonic::collector::{
    logs::v1::{
        logs_service_server::{LogsService, LogsServiceServer},
        ExportLogsServiceRequest, ExportLogsServiceResponse,
    },
    metrics::v1::{
        metrics_service_server::{MetricsService, MetricsServiceServer},
        ExportMetricsServiceRequest, ExportMetricsServiceResponse,
    },
    trace::v1::{
        trace_service_server::{TraceService, TraceServiceServer},
        ExportTraceServiceRequest, ExportTraceServiceResponse,
    },
};
use sqlx::PgPool;
use tonic::{transport::Server, Request, Response, Status};

use crate::otlp;

#[derive(Clone)]
struct OtlpGrpc {
    pool: PgPool,
}

#[tonic::async_trait]
impl TraceService for OtlpGrpc {
    async fn export(
        &self,
        request: Request<ExportTraceServiceRequest>,
    ) -> Result<Response<ExportTraceServiceResponse>, Status> {
        otlp::store_traces(&self.pool, request.into_inner()).await;
        Ok(Response::new(ExportTraceServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl LogsService for OtlpGrpc {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        otlp::store_logs(&self.pool, request.into_inner()).await;
        Ok(Response::new(ExportLogsServiceResponse::default()))
    }
}

#[tonic::async_trait]
impl MetricsService for OtlpGrpc {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        otlp::store_metrics(&self.pool, request.into_inner()).await;
        Ok(Response::new(ExportMetricsServiceResponse::default()))
    }
}

pub async fn serve(
    pool: PgPool,
    addr: SocketAddr,
    ingest_token: Option<Arc<str>>,
) -> Result<(), tonic::transport::Error> {
    let svc = OtlpGrpc { pool };
    let check = move |req: Request<()>| -> Result<Request<()>, Status> {
        if let Some(token) = &ingest_token {
            let ok = req
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(|t| t == &**token)
                .unwrap_or(false);
            if !ok {
                return Err(Status::unauthenticated("invalid or missing token"));
            }
        }
        Ok(req)
    };

    Server::builder()
        .add_service(TraceServiceServer::with_interceptor(
            svc.clone(),
            check.clone(),
        ))
        .add_service(LogsServiceServer::with_interceptor(
            svc.clone(),
            check.clone(),
        ))
        .add_service(MetricsServiceServer::with_interceptor(svc, check))
        .serve(addr)
        .await
}
