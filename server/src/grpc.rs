//! OTLP/gRPC server (:4317). Thin wrappers over the shared `otlp::store_*` functions.

use std::net::SocketAddr;

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

pub async fn serve(pool: PgPool, addr: SocketAddr) -> Result<(), tonic::transport::Error> {
    // Unauthenticated by design — the gRPC ingest port is only reachable
    // in-cluster and is never exposed publicly (see ADR 0013).
    let svc = OtlpGrpc { pool };
    Server::builder()
        .add_service(TraceServiceServer::new(svc.clone()))
        .add_service(LogsServiceServer::new(svc.clone()))
        .add_service(MetricsServiceServer::new(svc))
        .serve(addr)
        .await
}
