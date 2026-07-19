use crate::embedding::InputType;

use super::super::http_transport::request_json;
use super::super::*;

#[test]
fn voyage_defaults_and_normalization() {
    assert_eq!(voyage_url(None), VOYAGE_ENDPOINT);
    for blank in ["", "   ", "\t\n"] {
        assert_eq!(voyage_url(Some(blank)), VOYAGE_ENDPOINT);
    }
    assert_eq!(
        voyage_url(Some("https://my-proxy.com/v1")),
        "https://my-proxy.com/v1/embeddings"
    );
    assert_eq!(
        voyage_url(Some("https://my-proxy.com/v1/")),
        "https://my-proxy.com/v1/embeddings"
    );
    assert_eq!(
        voyage_url(Some("https://my-proxy.com/v1/embeddings/")),
        "https://my-proxy.com/v1/embeddings"
    );
}

#[test]
fn provider_parsing_preserves_voyage_fallback() {
    assert_eq!(Provider::parse("openai"), Provider::OpenAI);
    assert_eq!(Provider::parse(" OpenAI "), Provider::OpenAI);
    assert_eq!(Provider::parse("voyage"), Provider::Voyage);
    assert_eq!(Provider::parse("unknown"), Provider::Voyage);
    assert_eq!(Provider::parse(""), Provider::Voyage);
}

#[test]
fn openai_defaults_and_normalization() {
    assert_eq!(embedding_url(Provider::OpenAI, None), OPENAI_ENDPOINT);
    assert_eq!(embedding_url(Provider::OpenAI, Some("")), OPENAI_ENDPOINT);
    assert_eq!(embedding_url(Provider::Voyage, None), VOYAGE_ENDPOINT);
    assert_eq!(
        embedding_url(Provider::OpenAI, Some("https://gateway.local/v1/")),
        "https://gateway.local/v1/embeddings"
    );
    assert_eq!(
        embedding_url(
            Provider::OpenAI,
            Some("https://gateway.local/v1/embeddings")
        ),
        "https://gateway.local/v1/embeddings"
    );
}

#[test]
fn voyage_body_has_only_voyage_fields() {
    let body = request_json(Provider::Voyage, InputType::Document, Some(512));
    let object = body.as_object().unwrap();
    assert_eq!(object["input_type"], "document");
    assert!(!object.contains_key("dimensions"));
    assert_eq!(object.len(), 3);
}

#[test]
fn openai_body_omits_unset_provider_fields() {
    let body = request_json(Provider::OpenAI, InputType::Document, None);
    let object = body.as_object().unwrap();
    assert!(!object.contains_key("input_type"));
    assert!(!object.contains_key("dimensions"));
    assert_eq!(object.len(), 2);
}

#[test]
fn openai_body_includes_dimensions_when_set() {
    let body = request_json(Provider::OpenAI, InputType::Query, Some(256));
    let object = body.as_object().unwrap();
    assert!(!object.contains_key("input_type"));
    assert_eq!(object["dimensions"], 256);
    assert_eq!(object.len(), 3);
}
