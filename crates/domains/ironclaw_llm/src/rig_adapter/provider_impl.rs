use super::*;

#[async_trait]
impl<M> LlmProvider for RigAdapter<M>
where
    M: CompletionModel + Send + Sync + 'static,
    M::Response: Send + Sync + Serialize + DeserializeOwned,
{
    fn provider_id(&self) -> String {
        self.provider_id.clone()
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        (self.input_cost, self.output_cost)
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        let Some(endpoint) = self.models_endpoint.as_ref() else {
            // No discovery endpoint wired (e.g. Anthropic/Ollama paths); preserve
            // the trait default rather than guessing a URL.
            return Ok(Vec::new());
        };
        endpoint.fetch_models().await
    }

    fn cache_write_multiplier(&self) -> Decimal {
        match self.cache_retention {
            CacheRetention::None => Decimal::ONE,
            CacheRetention::Short => Decimal::new(125, 2), // 1.25× (125% of input rate)
            CacheRetention::Long => Decimal::TWO,          // 2.0×  (200% of input rate)
        }
    }

    fn cache_read_discount(&self) -> Decimal {
        if self.cache_retention != CacheRetention::None {
            dec!(10) // Anthropic: 90% discount (cost = input_rate / 10)
        } else {
            Decimal::ONE
        }
    }

    async fn complete(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse, LlmError> {
        self.ensure_structured_output_supported(request.response_format.as_ref())?;
        let model_override = request.take_model_override();
        let response_format = request.response_format.take();

        self.strip_unsupported_completion_params(&mut request);

        let mut messages = request.messages;
        crate::provider::sanitize_tool_messages(&mut messages);
        let (preamble, history) = convert_messages(&messages);

        let mut rig_req = build_rig_request(
            preamble,
            history,
            Vec::new(),
            None,
            request.temperature,
            self.max_tokens_or_default(request.max_tokens),
            self.cache_retention,
        )?;
        set_output_schema(
            &mut rig_req,
            response_format,
            &self.provider_id,
            self.json_object_supported,
        )?;

        merge_additional_params(&mut rig_req, self.default_additional_params.as_ref());
        if let Some(cache_key) = self.prompt_cache_key(&request.metadata) {
            merge_additional_params(
                &mut rig_req,
                Some(&serde_json::json!({
                    crate::provider::PROMPT_CACHE_KEY_METADATA: cache_key
                })),
            );
        }
        self.apply_reasoning_effort(&mut rig_req, &request.metadata, model_override.as_deref());
        inject_model_override(&mut rig_req, model_override.as_deref());

        let response = self
            .model
            .completion(rig_req)
            .await
            .map_err(|e| map_rig_error_for(&self.provider_id, &self.model_name, e))?;

        let raw_response = serialize_raw_response(&response.raw_response);
        let provider_finish = extract_finish_reason(raw_response.as_ref());
        let (text, _tool_calls, finish, reasoning, _reasoning_details) =
            extract_response(&response.choice, &response.usage, provider_finish);

        let resp = CompletionResponse {
            content: text.unwrap_or_default(),
            input_tokens: saturate_u32(response.usage.input_tokens),
            output_tokens: saturate_u32(response.usage.output_tokens),
            finish_reason: finish,
            reasoning,
            cache_read_input_tokens: saturate_u32(response.usage.cached_input_tokens),
            cache_creation_input_tokens: extract_cache_creation(raw_response.as_ref()),
        };

        if resp.cache_read_input_tokens > 0 {
            tracing::debug!(
                model = %self.model_name,
                input = resp.input_tokens,
                output = resp.output_tokens,
                cache_read = resp.cache_read_input_tokens,
                "prompt cache hit",
            );
        }

        Ok(resp)
    }

    async fn complete_streaming(
        &self,
        mut request: CompletionRequest,
        sink: std::sync::Arc<dyn CompletionStreamSink>,
    ) -> Result<CompletionResponse, LlmError> {
        self.ensure_structured_output_supported(request.response_format.as_ref())?;
        if !self.native_streaming {
            return self.complete(request).await;
        }

        let model_override = request.take_model_override();
        let response_format = request.response_format.take();

        self.strip_unsupported_completion_params(&mut request);

        let mut messages = request.messages;
        crate::provider::sanitize_tool_messages(&mut messages);
        let (preamble, history) = convert_messages(&messages);

        let mut rig_req = build_rig_request(
            preamble,
            history,
            Vec::new(),
            None,
            request.temperature,
            self.max_tokens_or_default(request.max_tokens),
            self.cache_retention,
        )?;
        set_output_schema(
            &mut rig_req,
            response_format,
            &self.provider_id,
            self.json_object_supported,
        )?;

        merge_additional_params(&mut rig_req, self.default_additional_params.as_ref());
        if let Some(cache_key) = self.prompt_cache_key(&request.metadata) {
            merge_additional_params(
                &mut rig_req,
                Some(&serde_json::json!({
                    crate::provider::PROMPT_CACHE_KEY_METADATA: cache_key
                })),
            );
        }
        self.apply_reasoning_effort(&mut rig_req, &request.metadata, model_override.as_deref());
        inject_model_override(&mut rig_req, model_override.as_deref());

        let stream = self
            .model
            .stream(rig_req)
            .await
            .map_err(|e| map_rig_error_for(&self.provider_id, &self.model_name, e))?;
        let drained = self.drain_streaming_response(stream, sink).await?;

        let resp = CompletionResponse {
            content: drained.text.unwrap_or_default(),
            input_tokens: saturate_u32(drained.usage.input_tokens),
            output_tokens: saturate_u32(drained.usage.output_tokens),
            finish_reason: drained.finish,
            reasoning: drained.reasoning,
            cache_read_input_tokens: saturate_u32(drained.usage.cached_input_tokens),
            cache_creation_input_tokens: drained.cache_creation_input_tokens,
        };

        if resp.cache_read_input_tokens > 0 {
            tracing::debug!(
                model = %self.model_name,
                input = resp.input_tokens,
                output = resp.output_tokens,
                cache_read = resp.cache_read_input_tokens,
                "prompt cache hit",
            );
        }

        Ok(resp)
    }

    async fn complete_with_tools(
        &self,
        mut request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        self.ensure_structured_output_supported(request.response_format.as_ref())?;
        let model_override = request.take_model_override();
        let response_format = request.response_format.take();

        self.strip_unsupported_tool_params(&mut request);

        let known_tool_names: HashSet<String> =
            request.tools.iter().map(|t| t.name.clone()).collect();

        let mut messages = request.messages;
        crate::provider::sanitize_tool_messages(&mut messages);
        let (preamble, history) = convert_messages(&messages);
        let tools = convert_tools(&request.tools);
        let tool_choice = convert_tool_choice(request.tool_choice.as_deref());

        let mut rig_req = build_rig_request(
            preamble,
            history,
            tools,
            tool_choice,
            request.temperature,
            self.max_tokens_or_default(request.max_tokens),
            self.cache_retention,
        )?;
        set_output_schema(
            &mut rig_req,
            response_format,
            &self.provider_id,
            self.json_object_supported,
        )?;

        merge_additional_params(&mut rig_req, self.default_additional_params.as_ref());
        if let Some(cache_key) = self.prompt_cache_key(&request.metadata) {
            merge_additional_params(
                &mut rig_req,
                Some(&serde_json::json!({
                    crate::provider::PROMPT_CACHE_KEY_METADATA: cache_key
                })),
            );
        }
        self.apply_reasoning_effort(&mut rig_req, &request.metadata, model_override.as_deref());
        inject_model_override(&mut rig_req, model_override.as_deref());

        let response = self
            .model
            .completion(rig_req)
            .await
            .map_err(|e| map_rig_error_for(&self.provider_id, &self.model_name, e))?;

        let raw_response = serialize_raw_response(&response.raw_response);
        let provider_finish = extract_finish_reason(raw_response.as_ref());
        let (text, mut tool_calls, finish, reasoning, reasoning_details) =
            extract_response(&response.choice, &response.usage, provider_finish);

        // Normalize tool call names: some proxies prepend "proxy_" prefixes.
        for tc in &mut tool_calls {
            let normalized = normalize_tool_name(&tc.name, &known_tool_names);
            if normalized != tc.name {
                tracing::debug!(
                    original = %tc.name,
                    normalized = %normalized,
                    "Normalized tool call name from provider",
                );
                tc.name = normalized;
            }
        }

        // Strict-mode tool schemas advertise every optional as required+nullable,
        // so the model fills unset optionals with `null`. Strip those placeholders
        // against each tool's original schema so only provided values reach the
        // tool.
        crate::tool_schema::strip_unset_optional_fields(
            &mut tool_calls,
            &request.tools,
            crate::tool_schema::PlaceholderStrippingMode::NullOnly,
        );

        let resp = ToolCompletionResponse {
            content: text,
            tool_calls,
            input_tokens: saturate_u32(response.usage.input_tokens),
            output_tokens: saturate_u32(response.usage.output_tokens),
            finish_reason: finish,
            cache_read_input_tokens: saturate_u32(response.usage.cached_input_tokens),
            cache_creation_input_tokens: extract_cache_creation(raw_response.as_ref()),
            reasoning,
            reasoning_details,
        };

        if resp.cache_read_input_tokens > 0 {
            tracing::debug!(
                model = %self.model_name,
                input = resp.input_tokens,
                output = resp.output_tokens,
                cache_read = resp.cache_read_input_tokens,
                "prompt cache hit",
            );
        }

        Ok(resp)
    }

    async fn complete_with_tools_streaming(
        &self,
        mut request: ToolCompletionRequest,
        sink: std::sync::Arc<dyn CompletionStreamSink>,
    ) -> Result<ToolCompletionResponse, LlmError> {
        self.ensure_structured_output_supported(request.response_format.as_ref())?;
        if !self.native_streaming {
            return self.complete_with_tools(request).await;
        }

        let model_override = request.take_model_override();
        let response_format = request.response_format.take();

        self.strip_unsupported_tool_params(&mut request);

        let known_tool_names: HashSet<String> =
            request.tools.iter().map(|t| t.name.clone()).collect();

        let mut messages = request.messages;
        crate::provider::sanitize_tool_messages(&mut messages);
        let (preamble, history) = convert_messages(&messages);
        let tools = convert_tools(&request.tools);
        let tool_choice = convert_tool_choice(request.tool_choice.as_deref());

        let mut rig_req = build_rig_request(
            preamble,
            history,
            tools,
            tool_choice,
            request.temperature,
            self.max_tokens_or_default(request.max_tokens),
            self.cache_retention,
        )?;
        set_output_schema(
            &mut rig_req,
            response_format,
            &self.provider_id,
            self.json_object_supported,
        )?;

        merge_additional_params(&mut rig_req, self.default_additional_params.as_ref());
        if let Some(cache_key) = self.prompt_cache_key(&request.metadata) {
            merge_additional_params(
                &mut rig_req,
                Some(&serde_json::json!({
                    crate::provider::PROMPT_CACHE_KEY_METADATA: cache_key
                })),
            );
        }
        self.apply_reasoning_effort(&mut rig_req, &request.metadata, model_override.as_deref());
        inject_model_override(&mut rig_req, model_override.as_deref());

        let stream = self
            .model
            .stream(rig_req)
            .await
            .map_err(|e| map_rig_error_for(&self.provider_id, &self.model_name, e))?;
        let mut drained = self.drain_streaming_response(stream, sink).await?;

        // Normalize tool call names: some proxies prepend "proxy_" prefixes.
        for tc in &mut drained.tool_calls {
            let normalized = normalize_tool_name(&tc.name, &known_tool_names);
            if normalized != tc.name {
                tracing::debug!(
                    original = %tc.name,
                    normalized = %normalized,
                    "Normalized tool call name from provider",
                );
                tc.name = normalized;
            }
        }

        crate::tool_schema::strip_unset_optional_fields(
            &mut drained.tool_calls,
            &request.tools,
            crate::tool_schema::PlaceholderStrippingMode::NullOnly,
        );

        let resp = ToolCompletionResponse {
            content: drained.text,
            tool_calls: drained.tool_calls,
            input_tokens: saturate_u32(drained.usage.input_tokens),
            output_tokens: saturate_u32(drained.usage.output_tokens),
            finish_reason: drained.finish,
            cache_read_input_tokens: saturate_u32(drained.usage.cached_input_tokens),
            cache_creation_input_tokens: drained.cache_creation_input_tokens,
            reasoning: drained.reasoning,
            reasoning_details: drained.reasoning_details,
        };

        if resp.cache_read_input_tokens > 0 {
            tracing::debug!(
                model = %self.model_name,
                input = resp.input_tokens,
                output = resp.output_tokens,
                cache_read = resp.cache_read_input_tokens,
                "prompt cache hit",
            );
        }

        Ok(resp)
    }

    fn active_model_name(&self) -> String {
        self.model_name.clone()
    }

    fn effective_model_name(&self, requested_model: Option<&str>) -> String {
        normalized_model_override(requested_model)
            .map(str::to_owned)
            .unwrap_or_else(|| self.active_model_name())
    }

    fn set_model(&self, _model: &str) -> Result<(), LlmError> {
        // rig-core models are baked at construction time.
        // Switching requires creating a new adapter.
        Err(LlmError::RequestFailed {
            provider: self.model_name.clone(),
            reason: "Runtime model switching not supported for rig-core providers. \
                     Restart with a different model configured."
                .to_string(),
        })
    }
}
