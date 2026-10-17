//! Request body and query extractors that fail with a problem document.
//!
//! axum's own `Json` and `Query` reject a malformed request with a plain-text
//! body; these wrappers answer an `invalid-request` problem instead, so every
//! error of the native API has the same shape.

use axum::Json;
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::problem::Problem;

/// A JSON request body.
#[derive(Debug, Clone)]
pub struct JsonBody<T>(pub T);

impl<T, S> FromRequest<S> for JsonBody<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    /// Parses the body, answering `invalid-request` when it is not the JSON
    /// the resource takes.
    async fn from_request(request: Request, state: &S) -> Result<JsonBody<T>, Problem> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            Err(rejection) => Err(Problem::invalid_request(rejection.body_text())),
        }
    }
}

/// The query parameters of a request.
#[derive(Debug, Clone)]
pub struct QueryParams<T>(pub T);

impl<T, S> FromRequestParts<S> for QueryParams<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    /// Parses the query, answering `invalid-request` when a parameter does not
    /// parse.
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<QueryParams<T>, Problem> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(value)) => Ok(QueryParams(value)),
            Err(rejection) => Err(Problem::invalid_request(rejection.body_text())),
        }
    }
}
