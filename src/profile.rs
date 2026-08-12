use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use praxis_filter::{FilterAction, FilterError, FilterPipeline, HttpFilterContext};

/// A named processing profile containing its own filter pipeline.
pub struct NamedProfile {
    /// Profile identifier.
    pub name: Arc<str>,
    /// The filter pipeline for this profile.
    pub pipeline: FilterPipeline,
}

/// Three-tier processing pipeline: pre → profile selection → post.
pub struct ProfilePipeline {
    /// Optional pipeline executed before profile selection.
    pre: Option<FilterPipeline>,
    /// Named profiles; exactly one is selected per request.
    profiles: Vec<NamedProfile>,
    /// Optional pipeline executed after the selected profile.
    post: Option<FilterPipeline>,
}

/// Saved filter execution state for a single pipeline tier.
#[derive(Debug, Default)]
struct TierExecution {
    /// Which filters ran during the request phase.
    executed_filter_indices: Vec<bool>,
    /// Branch re-entrance counters.
    branch_iterations: HashMap<Arc<str>, u32>,
}

/// Per-tier filter execution state saved from the request phase and
/// restored during the response phase.
#[derive(Debug, Default)]
pub struct ExecutionState {
    /// State from the pre-processing tier.
    pre: Option<TierExecution>,
    /// State from the profile tier.
    profile: TierExecution,
    /// State from the post-processing tier.
    post: Option<TierExecution>,
}

/// Save execution state from the context for later restoration.
fn save_tier_state(ctx: &mut HttpFilterContext<'_>) -> TierExecution {
    TierExecution {
        executed_filter_indices: std::mem::take(&mut ctx.executed_filter_indices),
        branch_iterations: std::mem::take(&mut ctx.branch_iterations),
    }
}

/// Restore previously saved execution state into the context.
fn restore_tier_state(ctx: &mut HttpFilterContext<'_>, saved: TierExecution) {
    ctx.executed_filter_indices = saved.executed_filter_indices;
    ctx.branch_iterations = saved.branch_iterations;
}

impl ProfilePipeline {
    /// Create a three-tier profile pipeline.
    pub fn new(pre: Option<FilterPipeline>, profiles: Vec<NamedProfile>, post: Option<FilterPipeline>) -> Self {
        Self { pre, profiles, post }
    }

    /// Wraps a single pipeline as a "default" profile with no pre/post processing.
    pub fn from_single(pipeline: FilterPipeline) -> Self {
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
    pub fn default_pipeline(&self) -> &FilterPipeline {
        &self.selected_profile().pipeline
    }

    /// Execute the request-phase pipeline across all tiers.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if any tier's filter execution fails.
    pub async fn execute_request(
        &self,
        ctx: &mut HttpFilterContext<'_>,
    ) -> Result<(FilterAction, ExecutionState), FilterError> {
        let mut tier_state = ExecutionState::default();

        if let Some(pre) = &self.pre {
            let action = pre.execute_http_request(ctx).await?;
            tier_state.pre = Some(save_tier_state(ctx));
            if matches!(&action, FilterAction::Reject(_)) {
                return Ok((action, tier_state));
            }
        }

        let profile = self.selected_profile();
        let action = profile.pipeline.execute_http_request(ctx).await?;
        tier_state.profile = save_tier_state(ctx);
        if matches!(&action, FilterAction::Reject(_)) {
            return Ok((action, tier_state));
        }

        if let Some(post) = &self.post {
            let action = post.execute_http_request(ctx).await?;
            tier_state.post = Some(save_tier_state(ctx));
            return Ok((action, tier_state));
        }

        Ok((action, tier_state))
    }

    /// Execute the response-phase pipeline across all tiers.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if any tier's filter execution fails.
    pub async fn execute_response(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        mut tier_state: ExecutionState,
    ) -> Result<FilterAction, FilterError> {
        if let Some(post) = &self.post {
            if let Some(saved) = tier_state.post.take() {
                restore_tier_state(ctx, saved);
            }
            let action = post.execute_http_response(ctx).await?;
            if matches!(&action, FilterAction::Reject(_)) {
                return Ok(action);
            }
        }

        restore_tier_state(ctx, tier_state.profile);
        let action = self.selected_profile().pipeline.execute_http_response(ctx).await?;
        if matches!(&action, FilterAction::Reject(_)) {
            return Ok(action);
        }

        if let Some(pre) = &self.pre {
            if let Some(saved) = tier_state.pre.take() {
                restore_tier_state(ctx, saved);
            }
            return pre.execute_http_response(ctx).await;
        }

        Ok(action)
    }

    /// Execute request body filters across all tiers.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if any tier's body filter execution fails.
    pub async fn execute_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if let Some(pre) = &self.pre {
            ctx.body_done_indices.clear();
            let action = pre.execute_http_request_body(ctx, body, end_of_stream).await?;
            if matches!(&action, FilterAction::Reject(_)) {
                return Ok(action);
            }
        }

        ctx.body_done_indices.clear();
        let action = self
            .selected_profile()
            .pipeline
            .execute_http_request_body(ctx, body, end_of_stream)
            .await?;
        if matches!(&action, FilterAction::Reject(_)) {
            return Ok(action);
        }

        if let Some(post) = &self.post {
            ctx.body_done_indices.clear();
            return post.execute_http_request_body(ctx, body, end_of_stream).await;
        }

        Ok(action)
    }

    /// Execute response body filters across all tiers (synchronous).
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if any tier's body filter execution fails.
    pub fn execute_response_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if let Some(post) = &self.post {
            ctx.body_done_indices.clear();
            let action = post.execute_http_response_body(ctx, body, end_of_stream)?;
            if matches!(&action, FilterAction::Reject(_)) {
                return Ok(action);
            }
        }

        ctx.body_done_indices.clear();
        let action = self
            .selected_profile()
            .pipeline
            .execute_http_response_body(ctx, body, end_of_stream)?;
        if matches!(&action, FilterAction::Reject(_)) {
            return Ok(action);
        }

        if let Some(pre) = &self.pre {
            ctx.body_done_indices.clear();
            return pre.execute_http_response_body(ctx, body, end_of_stream);
        }

        Ok(action)
    }

    /// Returns the currently selected profile.
    ///
    /// Profile selection always returns the first profile; routing
    /// logic will be added in issue #5.
    fn selected_profile(&self) -> &NamedProfile {
        match self.profiles.first() {
            Some(p) => p,
            None => unreachable!(),
        }
    }
}
