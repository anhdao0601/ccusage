mod loader;
mod report;
mod types;

use crate::{cli::AgentCommandArgs, print_json_or_jq, wants_json, Result};

pub(crate) fn run(args: AgentCommandArgs) -> Result<()> {
    let kind = args.kind;
    let shared = args.shared;
    // Caching happens per agent inside `loader::load_rows`, keyed on each
    // agent's own source fingerprint, so one busy agent does not force the
    // other agents to reload.
    let result = loader::load_rows(kind, &shared)?;
    if wants_json(&shared) {
        return print_json_or_jq(
            report::report_json(&result.rows, kind),
            shared.jq.as_deref(),
        );
    }
    report::print_table(&result.rows, kind, &shared, &result.detected_agents)
}

#[cfg(test)]
use loader::{
    aggregate_rows, aggregate_rows_by_provider, codex_group_row, load_agent_rows_parallel,
};
#[cfg(test)]
use report::{all_report_title, all_table_columns, all_table_row, report_json};
#[cfg(test)]
use types::{AgentLoadSpec, AgentRows, AllRow};

#[cfg(test)]
mod tests;
