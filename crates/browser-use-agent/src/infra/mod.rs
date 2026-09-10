//! Misc-infra leaf modules ported from `browser-use-core` (Phase D).
//!
//! These are the small, mostly-leaf helpers the TUI/CLI consume at well-defined
//! lifecycle points, ported faithfully from the legacy sync engine:
//!
//! - [`review`] — review-mode prompt builders for git scenarios
//!   (`browser-use-core::review`). The base review instructions delegate to the
//!   canonical [`crate::prompts::review_prompt`]; only the git-scenario builders
//!   are ported fresh.
//! - [`lifecycle`] — process-lifecycle infra: `install_process_crypto_provider`
//!   and the `UnifiedExecShutdownCleanup` RAII guard
//!   (`browser-use-core::lib`).
//! - [`persistence`] — `record_*` hooks that append Python/browser tool output
//!   into the [`browser_use_store::Store`]
//!   (`browser-use-core::persistence`).

pub mod lifecycle;
pub mod persistence;
pub mod review;

// Flat reexports mirroring the symbols the TUI/CLI import from the legacy core.
pub use lifecycle::{install_process_crypto_provider, UnifiedExecShutdownCleanup};
pub use persistence::{
    record_browser_command_response_events, record_browser_script_response_events,
    record_browser_script_response_events_for_tool, record_python_response_events,
    record_python_response_final_event, record_python_worker_event,
};
pub use review::{
    review_base_instructions, review_prompt_base_branch, review_prompt_commit,
    review_prompt_custom, review_prompt_uncommitted_changes, start_review_session,
    ReviewPromptError,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_are_available() {
        // Lifecycle.
        install_process_crypto_provider();
        let _guard = UnifiedExecShutdownCleanup::new();

        // Review builders delegate to the canonical prompt.
        assert_eq!(review_base_instructions(), crate::prompts::review_prompt());
        assert_eq!(
            review_prompt_uncommitted_changes(),
            "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings."
        );
    }
}
