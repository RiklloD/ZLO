use super::*;

pub(crate) async fn invoke(capability: &str, input: Value) -> Result<Value, FailureKind> {
    let runtime = runtime();
    invoke_with_context(&runtime, capability, input, execution_context([capability])).await
}

pub(crate) async fn invoke_shell(input: Value) -> Result<Value, FailureKind> {
    let runtime = runtime();
    invoke_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        input,
        execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy()),
    )
    .await
}

pub(crate) async fn invoke_with_context<R: HostRuntime + ?Sized>(
    runtime: &R,
    capability: &str,
    input: Value,
    context: ExecutionContext,
) -> Result<Value, FailureKind> {
    let outcome = runtime
        .invoke_capability((
            context,
            capability_id(capability),
            ResourceEstimate::default(),
            input,
        ))
        .await
        .unwrap();
    match outcome {
        RuntimeCapabilityOutcome::Completed(completed) => Ok(completed.output),
        RuntimeCapabilityOutcome::Failed(failure) => Err(failure.kind),
        other => panic!("unexpected capability outcome: {other:?}"),
    }
}

pub(crate) async fn invoke_failure_with_context<R: HostRuntime + ?Sized>(
    runtime: &R,
    capability: &str,
    input: Value,
    context: ExecutionContext,
) -> RuntimeCapabilityFailure {
    let outcome = runtime
        .invoke_capability((
            context,
            capability_id(capability),
            ResourceEstimate::default(),
            input,
        ))
        .await
        .unwrap();
    match outcome {
        RuntimeCapabilityOutcome::Failed(failure) => failure,
        other => panic!("unexpected capability outcome: {other:?}"),
    }
}

pub(crate) fn assert_failure_has_input_issue(
    failure: &RuntimeCapabilityFailure,
    path: &str,
    code: DispatchInputIssueCode,
    case_name: &str,
) {
    let _ = failure_input_issue(failure, path, code, case_name);
}

pub(crate) fn assert_failure_input_issue_expected(
    failure: &RuntimeCapabilityFailure,
    path: &str,
    code: DispatchInputIssueCode,
    expected: &str,
    case_name: &str,
) {
    let issue = failure_input_issue(failure, path, code, case_name);
    assert_eq!(issue.expected.as_deref(), Some(expected), "{case_name}");
}

pub(crate) fn failure_input_issue<'a>(
    failure: &'a RuntimeCapabilityFailure,
    path: &str,
    code: DispatchInputIssueCode,
    case_name: &str,
) -> &'a ironclaw_host_api::dispatch::DispatchInputIssue {
    let Some(DispatchFailureDetail::InvalidInput { issues }) = &failure.detail else {
        panic!(
            "{case_name}: expected invalid-input detail, got {:?}",
            failure.detail
        );
    };
    issues
        .iter()
        .find(|issue| issue.path == path && issue.code == code)
        .unwrap_or_else(|| panic!("{case_name}: expected issue {path} {code:?}, got {issues:?}"))
}
