//! C00-06 spec 3: an adapter that cannot express a message semantic refuses it instead of dropping it.
//!
//! The failure this prevents is silent: a transcript that carries cache breakpoints or replayed
//! reasoning is sent to a wire format that ignores those fields, the model sees a different
//! conversation than the host recorded, and nothing anywhere says so.

use std::future::Future;
use std::pin::Pin;

use kroma_core::budget::BudgetConfig;
use kroma_core::messages::{
    AssistantTurn, CacheBreakpoint, CacheBreakpointMarker, FinishReason, ReasoningBlock,
    TranscriptItem,
};
use kroma_core::provider::{
    check_semantics, MessageSemantics, ModelProvider, ProviderCompletion, ProviderError,
    ProviderRequest,
};
use tokio::sync::watch;

/// An adapter that declares nothing, which is the default every adapter starts from.
struct Undeclared {
    semantics: MessageSemantics,
}

impl ModelProvider for Undeclared {
    fn message_semantics(&self) -> MessageSemantics {
        self.semantics
    }

    fn complete<'a>(
        &'a self,
        req: ProviderRequest,
        _budget: &'a BudgetConfig,
        _cancel_rx: watch::Receiver<bool>,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderCompletion, ProviderError>> + Send + 'a>> {
        Box::pin(async move {
            check_semantics(&req, self.message_semantics())?;
            Err(ProviderError::Transport("not a real transport".to_string()))
        })
    }
}

fn request_with_breakpoints() -> ProviderRequest {
    ProviderRequest {
        cache_breakpoints: vec![CacheBreakpointMarker {
            breakpoint: CacheBreakpoint::TailWindowStart,
            message_index: 3,
        }],
        ..ProviderRequest::default()
    }
}

fn request_with_reasoning() -> ProviderRequest {
    ProviderRequest {
        messages: vec![TranscriptItem::Assistant(AssistantTurn {
            content: Some("done".to_string()),
            reasoning_blocks: vec![ReasoningBlock {
                text: Some("because".to_string()),
                signature: None,
                opaque_payload: None,
            }],
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: None,
        })],
        ..ProviderRequest::default()
    }
}

async fn dispatch(provider: &dyn ModelProvider, request: ProviderRequest) -> ProviderError {
    let (_tx, rx) = watch::channel(false);
    provider
        .complete(request, &BudgetConfig::default(), rx)
        .await
        .expect_err("the stub never succeeds")
}

/// C00-06-A1: a request that uses an undeclared semantic is refused, and the refusal names it.
#[tokio::test]
async fn an_undeclared_semantic_is_refused_by_name() {
    let provider = Undeclared {
        semantics: MessageSemantics::default(),
    };
    let error = dispatch(&provider, request_with_breakpoints()).await;
    assert!(
        matches!(&error, ProviderError::UnsupportedSemantic(what) if what.contains("cache breakpoints")),
        "{error}"
    );
    let error = dispatch(&provider, request_with_reasoning()).await;
    assert!(
        matches!(&error, ProviderError::UnsupportedSemantic(what) if what.contains("reasoning")),
        "{error}"
    );
}

/// C00-06-A2: declaring a semantic is what allows it. The same request that was refused above is
/// accepted once the adapter says it can express it.
#[tokio::test]
async fn declaring_a_semantic_is_what_allows_it() {
    let provider = Undeclared {
        semantics: MessageSemantics::all(),
    };
    // The stub still fails at the transport, but it got past the semantic gate - which is the point.
    let error = dispatch(&provider, request_with_breakpoints()).await;
    assert!(
        !matches!(error, ProviderError::UnsupportedSemantic(_)),
        "a declared semantic must not be refused: {error}"
    );
}

/// C00-06-A3: the shipped adapters declare what they can actually express, so the declaration is
/// not a wish. Chat Completions replays reasoning but has no prompt-cache API; Anthropic has both.
#[test]
fn the_shipped_adapters_declare_what_they_can_express() {
    use kroma_core::provider::anthropic_messages::{
        AnthropicMessagesClient, AnthropicMessagesConfig,
    };
    use kroma_core::provider::chat_completions::{ChatCompletionsClient, ChatCompletionsConfig};

    let chat = ChatCompletionsClient::new(ChatCompletionsConfig {
        base_url: "https://api.example.com".to_string(),
        api_key: "fixture".to_string(),
        model: "m".to_string(),
        max_retries: 0,
        reasoning_effort: None,
    })
    .expect("valid base url");
    let semantics = chat.message_semantics();
    assert!(
        semantics.reasoning_replay,
        "chat completions replays reasoning blocks"
    );
    assert!(
        !semantics.cache_breakpoints,
        "chat completions has no prompt-cache API, so it must not claim one"
    );

    let anthropic = AnthropicMessagesClient::new(AnthropicMessagesConfig {
        base_url: "https://api.example.com".to_string(),
        api_key: "fixture".to_string(),
        model: "m".to_string(),
        max_retries: 0,
        max_tokens: 0,
    })
    .expect("valid base url");
    assert_eq!(
        anthropic.message_semantics(),
        MessageSemantics::all(),
        "Anthropic Messages expresses both"
    );
}
