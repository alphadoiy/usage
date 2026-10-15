#[cfg(test)]
mod ablation_binding_probes {
    use super::*;
    fn config()->ConnectionConfig {
        serde_json::from_value(serde_json::json!({
            "id":"a", "service":"new_api_account", "base_url":"https://a.invalid", "credential_env":"QUERY_TEST_ONLY",
            "profile_ids":["123e4567-e89b-12d3-a456-426614174001"], "options":{"user_id":1},
            "inference":{"base_url":"https://a.invalid/v1","credential_env":"INFERENCE_TEST_ONLY"}
        })).unwrap()
    }
    const PROFILE:&str="123e4567-e89b-12d3-a456-426614174001";
    fn credentials(name:&str)->Option<String> {
        match name {"QUERY_TEST_ONLY"=>Some("synthetic-query".into()),"INFERENCE_TEST_ONLY"=>Some("synthetic-inference".into()),_=>None}
    }
    fn capture(run:&str)->CapturedContext {
        CapturedContext::capture(run,PROFILE,LaunchIdentity {
            state:IdentityState::Resolved,agent:"custom".into(),auth_mode:"byok".into(),endpoint:Some("https://a.invalid/v1".into()),
            model:Some("m".into()),model_provider:Some("provider".into()),credential_source:Some("environment".into()),reason:None,
        },Some("synthetic-inference"),Ok(&[config()]),&credentials,&|_|Some(crate::resources::ResourceScope::Account))
    }
    #[test]
    fn ablation_probe_query_resolver_keeps_validated_values_without_reopening_environment() {
        let pinned=capture("run-a").select("run-a",PROFILE,&[config()],&[],&credentials).unwrap();
        assert_eq!(pinned.credential_resolver()("env:QUERY_TEST_ONLY").unwrap(),"synthetic-query");
        assert!(pinned.credential_resolver()("env:UNRELATED_TEST_ONLY").is_err());
    }
    #[test]
    fn ablation_probe_credential_proofs_cannot_be_reused_across_runs() {
        let captured=capture("run-a");
        assert!(captured.select("run-b",PROFILE,&[config()],&[],&credentials).is_err());
    }
    #[test]
    fn ablation_probe_inconsistent_unresolved_context_is_rejected() {
        let mut captured=capture("run-a"); captured.context.identity.state=IdentityState::Unresolved;
        assert!(captured.select("run-a",PROFILE,&[config()],&[],&credentials).is_err());
    }
    #[test]
    fn ablation_probe_inconsistent_service_metadata_is_rejected() {
        let mut captured=capture("run-a"); captured.context.resources[0].service="different-service".into();
        assert!(captured.select("run-a",PROFILE,&[config()],&[],&credentials).is_err());
    }
}
