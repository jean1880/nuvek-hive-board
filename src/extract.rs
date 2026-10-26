//! Extractors whose rejections use the JSON error envelope.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts};

use crate::error::ApiError;

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        ApiError::Validation(format!("invalid JSON body: {}", r.body_text()))
    }
}

impl From<QueryRejection> for ApiError {
    fn from(r: QueryRejection) -> Self {
        ApiError::Validation(format!("invalid query string: {}", r.body_text()))
    }
}

impl From<PathRejection> for ApiError {
    fn from(r: PathRejection) -> Self {
        ApiError::Validation(format!("invalid path parameter: {}", r.body_text()))
    }
}

#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct ApiQuery<T>(pub T);

#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct ApiPath<T>(pub T);
