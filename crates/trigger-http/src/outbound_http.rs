use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use http::{uri::Scheme, HeaderName, HeaderValue};
use spin_core::async_trait;
use spin_factor_outbound_http::intercept::{self, InterceptOutcome, InterceptRequest};
use spin_factor_outbound_networking::config::allowed_hosts::{
    parse_service_chaining_target, OutboundAllowedHosts,
};
use spin_factors::RuntimeFactors;
use spin_http::routes::RouteMatch;
use wasmtime_wasi_http::{bindings::http::outgoing_handler::ErrorCode, HttpError, HttpResult};

use crate::HttpServer;

/// An outbound HTTP interceptor that handles service chaining requests.
pub struct OutboundHttpInterceptor<F: RuntimeFactors> {
    server: Arc<HttpServer<F>>,
    allowed_hosts: OutboundAllowedHosts,
}

impl<F: RuntimeFactors> OutboundHttpInterceptor<F> {
    pub fn new(server: Arc<HttpServer<F>>, allowed_hosts: OutboundAllowedHosts) -> Self {
        Self {
            server,
            allowed_hosts,
        }
    }
}

const CHAINED_CLIENT_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0);

#[async_trait]
impl<F: RuntimeFactors> intercept::OutboundHttpInterceptor for OutboundHttpInterceptor<F> {
    async fn intercept(&self, mut request: InterceptRequest) -> HttpResult<InterceptOutcome> {
        // Handle service chaining requests
        if let Some(component_id) = parse_service_chaining_target(request.uri()) {
            let req = request.into_hyper_request();
            let path = req.uri().path().to_owned();
            let route_match = RouteMatch::synthetic(component_id, path);
            let resp = self
                .server
                .handle_trigger_route(req, route_match, Scheme::HTTP, CHAINED_CLIENT_ADDR)
                .await
                .map_err(HttpError::trap)?;
            Ok(InterceptOutcome::Complete(resp))
        } else {
            // For demo purposes we need a way to send a request to an IP that doesn't match
            // its 'host' header. This allows the guest to set
            // 'fermyon-override-host' which will override 'host' here after
            // wasi-http is done forbidding that.
            // The 'check_override_host' makes this not an allowed_outbound_hosts bypass.
            const FERMYON_OVERRIDE_HOST: HeaderName =
                HeaderName::from_static("fermyon-override-host");
            if let Some(override_host) = request.headers_mut().remove(FERMYON_OVERRIDE_HOST) {
                self.check_override_host(&override_host)
                    .await
                    .map_err(|err| ErrorCode::InternalError(Some(err.to_string())))?;
                request
                    .headers_mut()
                    .insert(hyper::header::HOST, override_host);
            }
            Ok(InterceptOutcome::Continue(request))
        }
    }
}

impl<F: RuntimeFactors> OutboundHttpInterceptor<F> {
    /// Check fermyon-override-host
    /// See https://github.com/fermyon/lhc/issues/741
    async fn check_override_host(&self, host_value: &HeaderValue) -> anyhow::Result<()> {
        let host_str = host_value.to_str()?;
        let allowed = self
            .allowed_hosts
            .check_url(&format!("https://{host_str}"), "")
            .await?;
        anyhow::ensure!(allowed, "not allowed by allowed_outbound_hosts");
        Ok(())
    }
}
