use copro_api::tool::HostedToolSpec;
use copro_derive::CoproHostedTool;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, CoproHostedTool)]
#[serde(default)]
#[hosted_tool(kind = "test_tool")]
struct TestHostedTool {
    #[serde(skip_serializing_if = "Option::is_none")]
    partial: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, CoproHostedTool)]
#[hosted_tool(kind = "generic_tool")]
struct GenericHostedTool<T> {
    value: T,
}

#[test]
fn derived_hosted_tool_builds_spec() {
    let spec: HostedToolSpec = TestHostedTool {
        partial: Some(3),
        label: None,
    }
    .try_into()
    .unwrap();

    assert_eq!(spec.kind, "test_tool");
    assert_eq!(spec.parameters["partial"], 3);
    assert!(!spec.parameters.contains_key("label"));
    assert_eq!(
        spec.parameters::<TestHostedTool>().unwrap(),
        TestHostedTool {
            partial: Some(3),
            label: None,
        }
    );
}

#[test]
fn derived_hosted_tool_supports_generics() {
    let spec: HostedToolSpec = GenericHostedTool { value: "ok" }.try_into().unwrap();

    assert_eq!(spec.kind, "generic_tool");
    assert_eq!(spec.parameters["value"], "ok");
}
