use super::*;
use crate::llm::tool_offer::Exposure;

#[test]
fn tool_offer_policy_captures_route_budget_and_surface_stability() {
    default_offer_tests::with_agent("[assistance]\ncompact_tools=true", |agent| {
        let deferred = agent.tool_offer_policy(Exposure::Deferred);
        assert_eq!(deferred.core_schema_budget, agent.core_schema_budget());
        assert_eq!(deferred.stable_offer, agent.stable_tool_offer());
        assert_eq!(deferred.preserve_surface, deferred.stable_offer);

        let allowed = agent.tool_offer_policy(Exposure::AllowedTools);
        assert!(allowed.preserve_surface);

        let plain = agent.tool_offer_policy(Exposure::Plain);
        assert!(!plain.preserve_surface);
    });
}
