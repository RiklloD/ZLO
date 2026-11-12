//! Product-surface event subscription, stream request/response codecs, and the
//! first-event and access-revalidation timing (`projections` charter row).

use super::*;

pub(super) const PRODUCT_STREAM_FIRST_EVENT_WAIT: Duration = Duration::from_secs(1);

pub(super) const PRODUCT_STREAM_ACCESS_REVALIDATION_INTERVAL: Duration = Duration::from_secs(1);

pub(super) async fn open_product_surface_event_subscription<I, V>(
    services: &RebornServices<I, V>,
    caller: ProductSurfaceCaller,
    request: RebornStreamEventsRequest,
) -> Result<ironclaw_product_contracts::surface::ProductSurfaceEventSubscription, ProductSurfaceError>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    let (access, subscription_request) = services
        .resolve_projection_subscription_request(caller.clone(), request)
        .await?;
    let thread_id = subscription_request.scope.thread_id.clone();
    let actor = caller.actor();
    let Some(event_stream) = &services.event_stream else {
        return Err(ProductSurfaceError::from_status_kind(
            ProductSurfaceErrorCode::Unavailable,
            ProductSurfaceErrorKind::ReplayUnavailable,
            503,
            false,
        ));
    };
    if !event_stream.supports_subscription() {
        return Err(ProductSurfaceError::service_unavailable(false));
    }

    let subscribed_actor = access.run_actor.clone();
    let subscribed_scope = access.scope.clone();
    let mut subscription = event_stream
        .subscribe(subscription_request)
        .await
        .map_err(map_projection_error)?;
    // A single-slot handoff applies backpressure without creating another
    // burst queue. The underlying projection subscription remains alive
    // continuously, so live milestones cannot fall between resubscriptions.
    let (sender, receiver) = mpsc::channel(1);
    let (access_error_sender, mut access_error_receiver) = mpsc::channel(1);
    let services = services.clone();
    let access_caller = caller.clone();
    let access_thread_id = thread_id.clone();
    let access_actor = actor.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(PRODUCT_STREAM_ACCESS_REVALIDATION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The subscription-open probe is the initial validation. The first
        // periodic probe should run one interval later, not immediately.
        interval.tick().await;
        loop {
            tokio::select! {
                _ = access_error_sender.closed() => return,
                _ = interval.tick() => {}
            }
            let revalidated = services
                .resolve_thread_access_for_caller(
                    access_caller.clone(),
                    access_caller.turn_scope(access_thread_id.clone()),
                    &access_actor,
                )
                .await;
            let error = match revalidated {
                Ok(access)
                    if access.run_actor == subscribed_actor && access.scope == subscribed_scope =>
                {
                    continue;
                }
                Ok(_) => ProductSurfaceError::not_found(),
                Err(error) => error,
            };
            let _ = access_error_sender.send(error).await;
            return;
        }
    });
    tokio::spawn(async move {
        loop {
            // Reserve the one output slot before reading another source
            // event. A slow browser therefore backpressures this bridge
            // instead of accumulating another burst queue.
            let permit = tokio::select! {
                _ = sender.closed() => return,
                permit = sender.reserve() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return,
                },
            };
            let item = tokio::select! {
                biased;
                _ = sender.closed() => return,
                error = access_error_receiver.recv() => {
                    permit.send(Err(error.unwrap_or_else(ProductSurfaceError::internal)));
                    return;
                }
                item = subscription.next() => item,
            };
            let Some(item) = item else {
                return;
            };
            let response = match item {
                Ok(event) => encode_product_surface_stream_response(RebornStreamEventsResponse {
                    events: vec![event],
                }),
                Err(error) => Err(map_projection_error(error)),
            };
            let stop = response.is_err();
            permit.send(response);
            if stop {
                return;
            }
        }
    });
    Ok(ironclaw_product_contracts::surface::ProductSurfaceEventSubscription::new(receiver))
}

pub(super) fn decode_product_surface_stream_request(
    request: ironclaw_product_contracts::surface::ProductSurfaceStreamRequest,
) -> Result<RebornStreamEventsRequest, ProductSurfaceError> {
    let thread_id = request.stream_id.ok_or_else(|| {
        ProductSurfaceError::from_status(ProductSurfaceErrorCode::InvalidRequest, 400, false)
    })?;
    let after_cursor = match request.after_cursor {
        Some(cursor) => Some(ProjectionCursor::new(cursor).map_err(|_| {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::InvalidRequest, 400, false)
        })?),
        None => None,
    };
    Ok(RebornStreamEventsRequest {
        thread_id,
        after_cursor,
    })
}

pub(super) fn encode_product_surface_stream_response(
    response: RebornStreamEventsResponse,
) -> Result<ironclaw_product_contracts::surface::ProductSurfaceStreamResponse, ProductSurfaceError>
{
    let events = response
        .events
        .into_iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            tracing::error!(%error, "failed to encode product surface stream response");
            ProductSurfaceError::internal()
        })?;
    Ok(
        ironclaw_product_contracts::surface::ProductSurfaceStreamResponse {
            events,
            next_cursor: None,
            subscription: None,
        },
    )
}

pub(super) fn map_projection_error(error: ProductAdapterError) -> ProductSurfaceError {
    match error {
        ProductAdapterError::SurfaceRejected {
            kind: ProductSurfaceRejectionKind::Unavailable,
            status_code,
            retryable,
            ..
        } => ProductSurfaceError::from_status_kind(
            code_for_status(status_code),
            ProductSurfaceErrorKind::ReplayUnavailable,
            status_code,
            retryable,
        ),
        ProductAdapterError::SurfaceTransient { .. }
        | ProductAdapterError::EgressTransient { .. } => ProductSurfaceError::from_status_kind(
            ProductSurfaceErrorCode::Unavailable,
            ProductSurfaceErrorKind::ReplayUnavailable,
            503,
            true,
        ),
        _ => map_adapter_error(error),
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornStreamEventsRequest,
    ) -> Result<RebornStreamEventsResponse, ProductSurfaceError> {
        let (_, subscription_request) = self
            .resolve_projection_subscription_request(caller, request)
            .await?;
        let Some(event_stream) = &self.event_stream else {
            return Err(ProductSurfaceError::from_status_kind(
                ProductSurfaceErrorCode::Unavailable,
                ProductSurfaceErrorKind::ReplayUnavailable,
                503,
                false,
            ));
        };
        let events = event_stream
            .drain(subscription_request)
            .await
            .map_err(map_projection_error)?;
        Ok(RebornStreamEventsResponse { events })
    }

    pub(super) async fn resolve_projection_subscription_request(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornStreamEventsRequest,
    ) -> Result<(ResolvedThreadAccess, ProjectionSubscriptionRequest), ProductSurfaceError> {
        let thread_id = parse_thread_id_field("thread_id", request.thread_id)?;
        let actor = caller.actor();
        // Use the cheap ownership probe rather than loading the full
        // transcript. The automation fallback returns the trigger creator's
        // scope and actor when the caller owns that automation.
        let access = self
            .resolve_thread_access_for_caller(caller.clone(), caller.turn_scope(thread_id), &actor)
            .await?;
        // Projection identity is the actor that submitted the run, not
        // necessarily the browser caller. The authorization probe above is
        // the authority boundary for both one-shot drains and subscriptions.
        let subscription_request = ProjectionSubscriptionRequest {
            actor: access.run_actor.clone(),
            scope: access.scope.clone(),
            after_cursor: request.after_cursor,
        };
        Ok((access, subscription_request))
    }
}
