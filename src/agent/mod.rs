//! The fake agent. fakellm plays the model in an agent loop; the framework under test runs the
//! tools and sends the results back, so what gets exercised is the framework: tool dispatch,
//! result handling, retries, approval, context management.
//!
//! Two ways to decide a reply beyond plain script rules live here and in `crate::policy`:
//!
//! - [`trajectory`]: an ordered list of model turns replayed one per request. Step selection is
//!   stateless (the step is the number of assistant messages already in the conversation), so
//!   parallel conversations and client retries need no session ids.
//! - [`tool_map`]: renames the trajectory's tools and rewrites their arguments into the tools
//!   the client actually offers, which is what makes datasets recorded against other agents
//!   usable.
//!
//! A script rule hands a request to a trajectory with `reply: {trajectory: "name"}`. Everything
//! here produces a `ReplySpec`; the engine renders it like any rule's reply. See docs/m3-spec.md.

pub mod tool_map;
pub mod trajectory;
