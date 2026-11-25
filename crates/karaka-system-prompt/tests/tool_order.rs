//! The tools' order. Ported from dsh's
//! `core/system-prompt/tests/tool-order.spec.ts`.

use std::sync::Arc;

use karaka_model::ToolSchema;
use karaka_system_prompt::{
    AssembleContext, AssembleFuture, AssembleListener, AssembleNext, PromptAssembly, SystemPrompt,
    SystemPromptConfig, TOOL_ORDER_REST, ToolProviderResult,
};

fn tool(name: &str, description: &str) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: description.into(),
        parameters: r#"{"type":"object","properties":{}}"#.into(),
    }
}

fn mount(tool_order: Option<&[&str]>) -> SystemPrompt {
    SystemPrompt::new(SystemPromptConfig {
        tool_order: tool_order.map(|order| order.iter().map(|name| (*name).to_owned()).collect()),
        ..SystemPromptConfig::default()
    })
    .expect("the order is valid")
}

fn source(prompt: &SystemPrompt, names: &[&'static str]) {
    let names = names.to_vec();
    let _ = prompt.tools(None, move |_| ToolProviderResult {
        schemas: names.iter().map(|name| tool(name, name)).collect(),
        known_names: None,
    });
}

async fn names(prompt: &SystemPrompt) -> Vec<String> {
    prompt
        .assemble(&AssembleContext::default())
        .await
        .expect("the assembly succeeds")
        .tools
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

async fn refusal(prompt: &SystemPrompt) -> String {
    prompt
        .assemble(&AssembleContext::default())
        .await
        .expect_err("the assembly fails")
        .to_string()
}

#[test]
fn the_rest_entry_is_named_unlisted_tools() {
    assert_eq!(TOOL_ORDER_REST, "<unlisted-tools>");
}

#[tokio::test]
async fn without_an_order_tools_go_by_name_whatever_the_sources_order() {
    let prompt = mount(None);
    source(&prompt, &["charlie", "alpha"]);
    source(&prompt, &["bravo"]);
    assert_eq!(names(&prompt).await, ["alpha", "bravo", "charlie"]);
    let (forward, backward) = (mount(None), mount(None));
    source(&forward, &["alpha"]);
    source(&forward, &["zulu"]);
    source(&backward, &["zulu"]);
    source(&backward, &["alpha"]);
    assert_eq!(names(&forward).await, ["alpha", "zulu"]);
    assert_eq!(names(&backward).await, ["alpha", "zulu"]);
}

#[tokio::test]
async fn a_configured_order_places_the_rest_by_name_at_the_rest_entry() {
    let prompt = mount(Some(&["todo_write", TOOL_ORDER_REST, "bash"]));
    source(&prompt, &["bash", "echo_b", "todo_write", "echo_a"]);
    assert_eq!(
        names(&prompt).await,
        ["todo_write", "echo_a", "echo_b", "bash"]
    );
}

#[tokio::test]
async fn an_order_naming_a_tool_no_source_knows_fails_every_assembly() {
    let prompt = mount(Some(&["todo_write", "ghost", TOOL_ORDER_REST, "wraith"]));
    source(&prompt, &["bash", "todo_write"]);
    assert_eq!(
        refusal(&prompt).await,
        "toolOrder lists unregistered tools \"ghost\", \"wraith\"; known tools: bash, todo_write"
    );
    let empty = mount(Some(&["ghost", TOOL_ORDER_REST]));
    assert_eq!(
        refusal(&empty).await,
        "toolOrder lists unregistered tool \"ghost\"; known tools: (none)"
    );
}

#[tokio::test]
async fn a_tool_named_like_the_rest_entry_fails_the_assembly() {
    for order in [None, Some(&[TOOL_ORDER_REST][..])] {
        let prompt = mount(order);
        source(&prompt, &[TOOL_ORDER_REST]);
        assert!(
            refusal(&prompt)
                .await
                .starts_with("tool provider returned reserved tool name \"<unlisted-tools>\"")
        );
    }
}

#[tokio::test]
async fn tools_sharing_a_name_keep_their_collection_order() {
    let prompt = mount(None);
    let _ = prompt.tools(None, |_| ToolProviderResult {
        schemas: vec![
            tool("dup", "first"),
            tool("anchor", "anchor"),
            tool("dup", "second"),
        ],
        known_names: None,
    });
    let assembly = prompt.assemble(&AssembleContext::default()).await.unwrap();
    let descriptions: Vec<&str> = assembly
        .tools
        .iter()
        .map(|tool| tool.description.as_str())
        .collect();
    assert_eq!(descriptions, ["anchor", "first", "second"]);
}

/// Records the tools it is handed, then appends one.
struct Appender(std::sync::Mutex<Vec<String>>);

impl AssembleListener for Appender {
    fn assemble<'a>(
        &'a self,
        mut assembly: PromptAssembly,
        _context: &'a AssembleContext,
        next: AssembleNext<'a>,
    ) -> AssembleFuture<'a> {
        *self.0.lock().unwrap() = assembly
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        assembly.tools.push(tool("aardvark", "aardvark"));
        next.run(assembly)
    }
}

#[tokio::test]
async fn the_order_is_set_before_the_listeners_which_own_their_edits() {
    let prompt = mount(None);
    source(&prompt, &["zulu", "alpha"]);
    let listener = Arc::new(Appender(Default::default()));
    let _ = prompt.on_assemble(None, Arc::clone(&listener) as Arc<dyn AssembleListener>);
    assert_eq!(names(&prompt).await, ["alpha", "zulu", "aardvark"]);
    assert_eq!(*listener.0.lock().unwrap(), ["alpha", "zulu"]);
}

#[test]
fn an_order_without_one_rest_entry_or_with_a_repeat_is_refused_at_load() {
    let load = |order: &[&str]| {
        SystemPrompt::new(SystemPromptConfig {
            tool_order: Some(order.iter().map(|name| (*name).to_owned()).collect()),
            ..SystemPromptConfig::default()
        })
        .expect_err("the order is refused")
        .to_string()
    };
    for order in [&[][..], &["bash", "todo_write"]] {
        assert!(load(order).contains("must contain the \"<unlisted-tools>\" rest entry"));
    }
    for order in [
        &["bash", "bash", TOOL_ORDER_REST][..],
        &[TOOL_ORDER_REST, "bash", TOOL_ORDER_REST],
    ] {
        assert!(load(order).contains("more than once"));
    }
}
