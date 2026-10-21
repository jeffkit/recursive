//! Issue #182: `PermissionMode::Auto` must never silently degrade to
//! `default` ("unknown = allow") semantics.
//!
//! The no-classifier deny branch is unit-tested in
//! `src/tools/policy_domain/permission_pipeline.rs`; this companion pins the
//! classifier-wired path end to end through the public API. It lives outside
//! `src/tools/` because that tree may not import `crate::llm` internals
//! (invariant #2, orthogonality) and building an `AutoClassifier` needs a
//! `ChatProvider` mock.

use std::sync::Arc;

use recursive::llm::mock::MockProvider;
use recursive::llm::Completion;
use recursive::permissions::auto_classifier::AutoClassifier;
use recursive::permissions::{
    LayeredPermissionsConfig, PermissionLayer, PermissionMode, RuleSource,
};
use recursive::tools::{CheckOutcome, PermissionPipeline, ToolRegistry};

fn auto_config() -> LayeredPermissionsConfig {
    LayeredPermissionsConfig {
        mode: PermissionMode::Auto,
        layers: vec![PermissionLayer {
            source: RuleSource::User,
            allow: vec!["Read".into()],
            ..Default::default()
        }],
    }
}

fn classifier_completion(content: &str) -> Completion {
    Completion {
        content: content.to_string(),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        reasoning_content: None,
    }
}

/// With a classifier attached, `auto` delegates: a `block` decision denies
/// and a `pass` decision falls through to the static rules. Pins the Goal-200
/// wiring so the fail-closed branch cannot swallow it.
#[tokio::test]
async fn auto_with_classifier_respects_decision() {
    let blocking = ToolRegistry::local()
        .with_permissions(auto_config())
        .with_auto_classifier(AutoClassifier::new(Arc::new(MockProvider::new(vec![
            classifier_completion(r#"{"block":true,"reason":"risky"}"#),
        ]))));
    let outcome = PermissionPipeline::new(&blocking)
        .check("Read", serde_json::json!({"path": "src/lib.rs"}))
        .await;
    assert!(outcome.is_deny(), "a blocking classifier must deny");

    let passing = ToolRegistry::local()
        .with_permissions(auto_config())
        .with_auto_classifier(AutoClassifier::new(Arc::new(MockProvider::new(vec![
            classifier_completion(r#"{"block":false,"reason":"ok"}"#),
        ]))));
    let outcome = PermissionPipeline::new(&passing)
        .check("Read", serde_json::json!({"path": "src/lib.rs"}))
        .await;
    match outcome {
        CheckOutcome::Allow { .. } => {}
        CheckOutcome::Deny { error, .. } => {
            panic!("a passing classifier must fall through to the static rules: {error:?}")
        }
    }
}
