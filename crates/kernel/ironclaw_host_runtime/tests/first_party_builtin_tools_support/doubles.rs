use super::*;

#[derive(Debug, Default)]
pub(crate) struct RecordingProcessPort {
    pub(crate) requests: std::sync::Mutex<Vec<CommandExecutionRequest>>,
}

#[async_trait]
impl RuntimeProcessPort for RecordingProcessPort {
    async fn run_command(
        &self,
        request: CommandExecutionRequest,
    ) -> Result<CommandExecutionOutput, RuntimeProcessError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(CommandExecutionOutput {
            output: format!("process port: {}", request.command),
            saved_output: None,
            exit_code: 0,
            sandboxed: true,
            duration: Duration::from_millis(7),
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct RecordingSandboxTransport {
    pub(crate) requests: std::sync::Mutex<Vec<CommandExecutionRequest>>,
}

#[async_trait]
impl SandboxCommandTransport for RecordingSandboxTransport {
    async fn run_command(
        &self,
        request: CommandExecutionRequest,
    ) -> Result<CommandExecutionOutput, RuntimeProcessError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(CommandExecutionOutput {
            output: format!("process port: {}", request.command),
            saved_output: None,
            exit_code: 0,
            sandboxed: false,
            duration: Duration::from_millis(7),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RecordingRuntimeHttpEgress {
    pub(crate) requests: Arc<std::sync::Mutex<Vec<RuntimeHttpEgressRequest>>>,
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
    pub(crate) saved_body: Option<RuntimeHttpSavedBody>,
    pub(crate) error: Option<RuntimeHttpEgressError>,
    pub(crate) responses: Option<BTreeMap<String, (u16, Vec<u8>)>>,
    pub(crate) headers: Option<Vec<(String, String)>>,
    pub(crate) redaction_applied: bool,
    pub(crate) response_bytes_override: Option<u64>,
}

impl RecordingRuntimeHttpEgress {
    pub(crate) fn with_body(body: Vec<u8>) -> Self {
        Self {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            status: 200,
            body,
            saved_body: None,
            error: None,
            responses: None,
            headers: None,
            redaction_applied: false,
            response_bytes_override: None,
        }
    }

    pub(crate) fn with_status_and_body(status: u16, body: Vec<u8>) -> Self {
        Self {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            status,
            body,
            saved_body: None,
            error: None,
            responses: None,
            headers: None,
            redaction_applied: false,
            response_bytes_override: None,
        }
    }

    pub(crate) fn with_error(error: RuntimeHttpEgressError) -> Self {
        Self {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            status: 200,
            body: Vec::new(),
            saved_body: None,
            error: Some(error),
            responses: None,
            headers: None,
            redaction_applied: false,
            response_bytes_override: None,
        }
    }

    pub(crate) fn with_saved_body(mut self, path: &str, bytes_written: u64) -> Self {
        self.saved_body = Some(RuntimeHttpSavedBody {
            path: ScopedPath::new(path).unwrap(),
            bytes_written,
        });
        self
    }

    pub(crate) fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = Some(headers);
        self
    }

    pub(crate) fn with_redaction_applied(mut self, redaction_applied: bool) -> Self {
        self.redaction_applied = redaction_applied;
        self
    }

    pub(crate) fn with_response_bytes(mut self, response_bytes: u64) -> Self {
        self.response_bytes_override = Some(response_bytes);
        self
    }

    pub(crate) fn with_url_bodies(responses: BTreeMap<String, (u16, Vec<u8>)>) -> Self {
        Self {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            status: 200,
            body: Vec::new(),
            saved_body: None,
            error: None,
            responses: Some(responses),
            headers: None,
            redaction_applied: false,
            response_bytes_override: None,
        }
    }

    pub(crate) fn requests(&self) -> Vec<RuntimeHttpEgressRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl RuntimeHttpEgress for RecordingRuntimeHttpEgress {
    async fn execute(
        &self,
        request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        self.requests.lock().unwrap().push(request.clone());
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let (status, body) = self
            .responses
            .as_ref()
            .and_then(|responses| responses.get(&request.url).cloned())
            .unwrap_or_else(|| {
                (
                    if self.status == 0 { 200 } else { self.status },
                    self.body.clone(),
                )
            });
        Ok(RuntimeHttpEgressResponse {
            status,
            headers: self.headers.clone().unwrap_or_else(|| {
                vec![("content-type".to_string(), "application/json".to_string())]
            }),
            body: body.clone(),
            saved_body: self.saved_body.clone(),
            request_bytes: request.body.len() as u64,
            response_bytes: self.response_bytes_override.unwrap_or(body.len() as u64),
            redaction_applied: self.redaction_applied,
        })
    }
}

#[async_trait::async_trait]
impl ToolCallHttpEgress for RecordingRuntimeHttpEgress {
    async fn execute_for_model_visible_output(
        &self,
        request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        self.execute(request).await
    }
}

pub(crate) fn serialized_json_len(value: &Value) -> usize {
    serde_json::to_vec(value).unwrap().len()
}

#[derive(Debug, Clone)]
pub(crate) struct SleepingRuntimeHttpEgress {
    pub(crate) delay: Duration,
    pub(crate) body: Vec<u8>,
}

#[async_trait::async_trait]
impl RuntimeHttpEgress for SleepingRuntimeHttpEgress {
    async fn execute(
        &self,
        request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        tokio::time::sleep(self.delay).await;
        let body = if self.body.is_empty() {
            b"ok".to_vec()
        } else {
            self.body.clone()
        };
        Ok(RuntimeHttpEgressResponse {
            status: 200,
            headers: Vec::new(),
            response_bytes: body.len() as u64,
            body,
            saved_body: None,
            request_bytes: request.body.len() as u64,
            redaction_applied: false,
        })
    }
}

#[async_trait::async_trait]
impl ToolCallHttpEgress for SleepingRuntimeHttpEgress {
    async fn execute_for_model_visible_output(
        &self,
        request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        self.execute(request).await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PanickingRuntimeHttpEgress;

#[async_trait::async_trait]
impl RuntimeHttpEgress for PanickingRuntimeHttpEgress {
    async fn execute(
        &self,
        _request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        panic!("runtime HTTP egress panic")
    }
}

#[async_trait::async_trait]
impl ToolCallHttpEgress for PanickingRuntimeHttpEgress {
    async fn execute_for_model_visible_output(
        &self,
        request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        self.execute(request).await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PanickingToolCallHttpEgress;

#[async_trait::async_trait]
impl ToolCallHttpEgress for PanickingToolCallHttpEgress {
    async fn execute_for_model_visible_output(
        &self,
        _request: RuntimeHttpEgressRequest,
    ) -> Result<RuntimeHttpEgressResponse, RuntimeHttpEgressError> {
        panic!("tool-call HTTP egress must not be used")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RecordingTransport {
    pub(crate) response: Result<NetworkHttpResponse, NetworkHttpError>,
    pub(crate) requests: Arc<std::sync::Mutex<Vec<NetworkTransportRequest>>>,
}

impl RecordingTransport {
    pub(crate) fn ok(response: NetworkHttpResponse) -> Self {
        Self {
            response: Ok(response),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn err(error: NetworkHttpError) -> Self {
        Self {
            response: Err(error),
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl NetworkHttpTransport for RecordingTransport {
    async fn execute(
        &self,
        request: NetworkTransportRequest,
    ) -> Result<NetworkHttpResponse, NetworkHttpError> {
        self.requests.lock().unwrap().push(request);
        self.response.clone()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StaticResolver {
    pub(crate) ips: Vec<IpAddr>,
}

impl StaticResolver {
    pub(crate) fn new(ips: Vec<IpAddr>) -> Self {
        Self { ips }
    }
}

impl NetworkResolver for StaticResolver {
    fn resolve_ips(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, NetworkHttpError> {
        Ok(self.ips.clone())
    }
}
