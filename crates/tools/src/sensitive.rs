//! Resource-specific authorization for tools that can reach data outside the workspace.
//! The regular policy gate answers "may this tool run?".  This layer answers the narrower
//! question "may it touch this particular resource?".  Keeping the two decisions separate means
//! a durable permission for a high-risk tool never silently becomes permission for a production
//! database, cloud account, remote Git repository, or recording device.

use crate::{Choice, Control, Form, FormField, Result, ToolCtx};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitiveEnvironment {
    NonProduction,
    Production,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveResource<'a> {
    pub kind: &'a str,
    pub target: &'a str,
    pub action: &'a str,
    /// Some tools are intentionally unavailable for production resources even after confirmation.
    pub allow_production: bool,
    /// Managed resources already have a trusted classification. Supplying it prevents a caller
    /// from reclassifying the resource in the per-access form.
    pub environment: Option<SensitiveEnvironment>,
    /// Stable hash of the exact target configuration shown in the confirmation.
    pub fingerprint: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAuthorization {
    Allowed(SensitiveEnvironment),
    Cancelled,
    Denied,
    ProductionBlocked,
}

pub async fn authorize_sensitive_resource(
    ctx: &ToolCtx,
    resource: SensitiveResource<'_>,
) -> Result<ResourceAuthorization> {
    let mut fields = Vec::new();
    if resource.environment.is_none() {
        fields.push(
            FormField::required(
                "environment",
                "Environment",
                Control::Select {
                    options: vec![
                        Choice::new("non_production", "Development / test")
                            .with_description("The resource is not used by production traffic."),
                        Choice::new("production", "Production")
                            .with_description("The resource serves production users or data."),
                    ],
                    default: None,
                    // A closed set by design: an "other" answer must not dodge the environment gate.
                    free_text: false,
                },
            )
            .with_help("Choose based on the resource itself, not the current workspace."),
        );
    }
    fields.push(
        FormField::required(
            "authorize",
            "Allow this access",
            Control::Confirm {
                default: Some(false),
            },
        )
        .with_help("Confirm only after checking the exact target and action above."),
    );
    let fingerprint = resource
        .fingerprint
        .map(|value| format!("\nConfiguration fingerprint: {value}"))
        .unwrap_or_default();
    let form =
        Form::new(format!("Authorize {} access", resource.kind), fields).with_message(format!(
            "Resource: {}\nTarget: {}\nAction: {}{}\n\nThis confirmation is required for the \
         resource itself and is not bypassed by a general tool permission.",
            resource.kind, resource.target, resource.action, fingerprint
        ));

    let Some(answer) = ctx.ask_form(form).await? else {
        return Ok(ResourceAuthorization::Cancelled);
    };
    if answer.bool("authorize") != Some(true) {
        return Ok(ResourceAuthorization::Denied);
    }
    match resource
        .environment
        .map(|environment| match environment {
            SensitiveEnvironment::NonProduction => "non_production",
            SensitiveEnvironment::Production => "production",
        })
        .or_else(|| answer.text("environment"))
    {
        Some("non_production") => Ok(ResourceAuthorization::Allowed(
            SensitiveEnvironment::NonProduction,
        )),
        Some("production") if resource.allow_production => Ok(ResourceAuthorization::Allowed(
            SensitiveEnvironment::Production,
        )),
        Some("production") => Ok(ResourceAuthorization::ProductionBlocked),
        _ => Ok(ResourceAuthorization::Denied),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use zlogic_protocol::interaction::{
        FieldValue, FormAnswer, InteractionDecision, InteractionPort, InteractionRequest,
    };

    use super::*;

    struct Answer(InteractionDecision);

    #[async_trait]
    impl InteractionPort for Answer {
        async fn ask(
            &self,
            _request: InteractionRequest,
        ) -> std::result::Result<InteractionDecision, String> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn production_is_blocked_even_after_the_user_confirms() {
        let mut ctx = crate::test_ctx(std::path::Path::new("/work"));
        ctx.interaction = Some(Arc::new(Answer(InteractionDecision::Submitted(
            FormAnswer::new()
                .set("environment", FieldValue::Choice("production".into()))
                .set("authorize", FieldValue::Bool(true)),
        ))));
        let result = authorize_sensitive_resource(
            &ctx,
            SensitiveResource {
                kind: "database",
                target: "postgresql://reader@db/prod",
                action: "read",
                allow_production: false,
                environment: None,
                fingerprint: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(result, ResourceAuthorization::ProductionBlocked);
    }
}
