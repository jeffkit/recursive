//! Issue #124: CLI-side attachment of the optional Langfuse run trace.
//!
//! Keeps the host wiring in one place so both the `recursive run` path and
//! any future CLI host attach the exporter identically.

use recursive::observability::{with_sink, LangfuseRun, RunMeta};
use recursive::EventSink;

/// Build the run handle for a CLI run and append its sink to `sinks`.
///
/// Returns an inert handle (whose `sink()` is `None`) when the observability
/// env vars are unset, so callers can always attach unconditionally.
#[cfg_attr(test, mutants::skip)]
pub fn attach(
    session_id: String,
    model: String,
    provider: String,
    sinks: &mut Vec<Box<dyn EventSink>>,
) -> LangfuseRun {
    let run = LangfuseRun::try_new(RunMeta::new(session_id, model, provider));
    with_sink(&run, sinks);
    run
}

#[cfg(test)]
mod tests {
    use super::*;
    use recursive::NullSink;

    #[test]
    fn attach_appends_a_sink_only_when_active() {
        let mut sinks: Vec<Box<dyn EventSink>> = vec![Box::new(NullSink)];
        let run = attach(
            "sess".to_string(),
            "model".to_string(),
            "prov".to_string(),
            &mut sinks,
        );
        let expected = 1 + usize::from(run.is_active());
        assert_eq!(sinks.len(), expected);
    }
}
