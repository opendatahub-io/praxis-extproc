//! Generic transport/adapter mechanics, exercised with stub filters so the
//! server machinery is tested independently of real filters.

mod support;

mod basics;
mod body_modes;
mod headers;
mod lifecycle;
mod limits;
mod message_limits;
mod payloads;
mod routing;
mod sequencing;
mod streaming;
mod validation;
