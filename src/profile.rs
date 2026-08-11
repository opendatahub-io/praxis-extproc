use std::sync::Arc;
use bytes::Bytes;
use praxis_filter::{FilterPipeline, HttpFilterContext};

/// A named processing profile containing its own filter pipeline.
pub struct NamedProfile {
    /// Profile identifier.
    pub name: Arc<str>,
    /// The filter pipeline for this profile.
    pub pipeline: Arc<FilterPipeline>,
}

/// Three-tier processing pipeline: pre → profile selection → post.
pub struct ProfilePipeline {
    /// Optional pipeline executed before profile selection.
    pre: Option<Arc<FilterPipeline>>,
    /// Named profiles; exactly one is selected per request.
    profiles: Vec<NamedProfile>,
    /// Optional pipeline executed after the selected profile.
    post: Option<Arc<FilterPipeline>>,
}

impl ProfilePipeline {
    /// Create a three-tier profile pipeline from optional pre/post tiers and named profiles.
    pub fn new(pre: Option<Arc<FilterPipeline>>, profiles: Vec<NamedProfile>, post: Option<Arc<FilterPipeline>>) -> Self {
        Self {
            pre,
            profiles,
            post,
        }
    }

    /// Wraps a single pipeline as a "default" profile with no pre/post processing.
    pub fn from_single(pipeline: Arc<FilterPipeline>) -> Self {
        Self {
            pre: None,
            profiles: vec![NamedProfile {
                name: Arc::from("default"),
                pipeline,
            }],
            post: None,
        }
    }

    /// Returns the default profile's filter pipeline.
    pub fn default_pipeline(&self) -> &Arc<FilterPipeline> {
        &self.profiles[0].pipeline
    }

    /// Execute the request-phase pipeline across all tiers.
    pub async fn execute_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<praxis_filter::FilterAction, praxis_filter::FilterError> {
        if let Some(pre) = &self.pre {
            let action = pre.execute_http_request(ctx).await?;
            if matches!(&action, praxis_filter::FilterAction::Reject(_)) {
                return Ok(action);
            }
        }

        let profile = &self.profiles[0];
        let action = profile.pipeline.execute_http_request(ctx).await?;
        if matches!(&action, praxis_filter::FilterAction::Reject(_)) {
            return Ok(action);
        }

        if let Some(post) = &self.post {
            return post.execute_http_request(ctx).await;
        }

        Ok(action)
    }

    /// Execute the response-phase pipeline across all tiers.
    pub async fn execute_response(&self, ctx: &mut HttpFilterContext<'_>) -> Result<praxis_filter::FilterAction, praxis_filter::FilterError> {
        if let Some(post) = &self.post {
            let action = post.execute_http_response(ctx).await?;
            if matches!(&action, praxis_filter::FilterAction::Reject(_)) {
                return Ok(action);
            }
        }

        let profile = &self.profiles[0];
        let action = profile.pipeline.execute_http_response(ctx).await?;
        if matches!(&action, praxis_filter::FilterAction::Reject(_)) {
            return Ok(action);
        }

        if let Some(pre) = &self.pre {
            return pre.execute_http_response(ctx).await;
        }

        Ok(action)
    }

    /// Execute request body filters across all tiers.
    pub async fn execute_request_body(&self, ctx: &mut HttpFilterContext<'_>, body: &mut Option<Bytes>, end_of_stream: bool) -> Result<praxis_filter::FilterAction, praxis_filter::FilterError> {
        if let Some(pre) = &self.pre {
            let action = pre.execute_http_request_body(ctx, body, end_of_stream).await?;
            if matches!(&action, praxis_filter::FilterAction::Reject(_)) {
                return Ok(action);
            }
        }

        let profile = &self.profiles[0];
        let action = profile.pipeline.execute_http_request_body(ctx, body, end_of_stream).await?;
        if matches!(&action, praxis_filter::FilterAction::Reject(_)) {
            return Ok(action);
        }

        if let Some(post) = &self.post {
            return post.execute_http_request_body(ctx, body, end_of_stream).await;
        }

        Ok(action)
    }

    /// Execute response body filters across all tiers (synchronous).
    pub fn execute_response_body(&self, ctx: &mut HttpFilterContext<'_>, body: &mut Option<Bytes>, end_of_stream: bool) -> Result<praxis_filter::FilterAction, praxis_filter::FilterError> {
        if let Some(post) = &self.post {
            let action = post.execute_http_response_body(ctx, body, end_of_stream)?;
            if matches!(&action, praxis_filter::FilterAction::Reject(_)) {
                return Ok(action);
            }
        }

        let profile = &self.profiles[0];
        let action = profile.pipeline.execute_http_response_body(ctx, body, end_of_stream)?;
        if matches!(&action, praxis_filter::FilterAction::Reject(_)) {
            return Ok(action);
        }

        if let Some(pre) = &self.pre {
            return pre.execute_http_response_body(ctx, body, end_of_stream);
        }

        Ok(action)
    }
}