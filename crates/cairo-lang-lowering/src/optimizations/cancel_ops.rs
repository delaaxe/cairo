#[cfg(test)]
#[path = "cancel_ops_test.rs"]
mod test;

use cairo_lang_utils::ordered_hash_set::OrderedHashSet;
use cairo_lang_utils::unordered_hash_map::UnorderedHashMap;
use itertools::{izip, zip_eq, Itertools};

use crate::borrow_check::analysis::{Analyzer, BackAnalysis, StatementLocation};
use crate::utils::{Rebuilder, RebuilderEx};
use crate::{BlockId, FlatLowered, MatchInfo, Statement, VarRemapping, VarUsage, VariableId};

/// Cancels out a (StructConstruct, StructDestructure) and (Snap, Desnap) pair.
///
///
/// The algorithm is as follows:
/// Run backwards analysis with demand to find all the use sites.
/// When we reach the first item in the pair, check which statement can be removed and
/// construct the relevant `renamed_vars` mapping.
///
/// See CancelOpsContext::handle_stmt for more detail on when it is safe
/// to remove a statement.
pub fn cancel_ops(lowered: &mut FlatLowered) {
    if lowered.blocks.is_empty() {
        return;
    }
    let ctx = CancelOpsContext {
        lowered,
        use_sites: Default::default(),
        var_remapper: Default::default(),
        stmts_to_remove: vec![],
    };
    let mut analysis =
        BackAnalysis { lowered: &*lowered, block_info: Default::default(), analyzer: ctx };
    analysis.get_root_info();

    let CancelOpsContext { mut var_remapper, stmts_to_remove, .. } = analysis.analyzer;

    // Remove no-longer needed statements.
    // Note that dedup() is used since a statement might be marked for removal more then once.
    for (block_id, stmt_id) in stmts_to_remove
        .into_iter()
        .sorted_by_key(|(block_id, stmt_id)| (block_id.0, *stmt_id))
        .rev()
        .dedup()
    {
        lowered.blocks[block_id].statements.remove(stmt_id);
    }

    // Rebuild the blocks with the new variable names.
    for block in lowered.blocks.iter_mut() {
        *block = var_remapper.rebuild_block(block);
    }
}

pub struct CancelOpsContext<'a> {
    lowered: &'a FlatLowered,

    /// Maps a variable to the use sites of that variable.
    /// Note that a remapping is cosidered as usage here.
    use_sites: UnorderedHashMap<VariableId, Vec<StatementLocation>>,

    /// Maps a variable to the variable that it was renamed to.
    var_remapper: CancelOpsRebuilder,

    /// Statements that can be be removed.
    stmts_to_remove: Vec<StatementLocation>,
}

/// Returns the use sites of a variable.
///
/// Takes 'use_sites' map rather than `CancelOpsContext` to avoid borrowing the entire context.
fn get_use_sites<'a>(
    use_sites: &'a UnorderedHashMap<VariableId, Vec<StatementLocation>>,
    var: &VariableId,
) -> &'a [StatementLocation] {
    match use_sites.get(var) {
        Some(use_sites) => &use_sites[..],
        None => &[],
    }
}

impl<'a> CancelOpsContext<'a> {
    fn rename_var(&mut self, from: VariableId, to: VariableId) {
        self.var_remapper.renamed_vars.insert(from, to);
        // Move `from` used sites to `to` to allow the optimization to be applied to them.
        if let Some(from_use_sites) = self.use_sites.remove(&from) {
            self.use_sites.entry(to).or_default().extend(from_use_sites);
        }
    }

    fn add_use_site(&mut self, var: VariableId, use_site: StatementLocation) {
        self.use_sites.entry(var).or_default().push(use_site);
    }

    /// Handles a statement and returns true if it can be removed.
    fn handle_stmt(&mut self, stmt: &'a Statement, statement_location: StatementLocation) -> bool {
        match stmt {
            Statement::StructDestructure(stmt) => {
                let mut use_sites = OrderedHashSet::<&StatementLocation>::default();

                for output in stmt.outputs.iter() {
                    let output_use_sites = get_use_sites(&self.use_sites, output);
                    use_sites.extend(output_use_sites);
                }

                let mut can_remove_struct_destructure = true;

                let constructs = use_sites
                    .iter()
                    .filter_map(|location| {
                        match self.lowered.blocks[location.0].statements.get(location.1) {
                            Some(Statement::StructConstruct(construct_stmt))
                                if stmt.outputs.len() == construct_stmt.inputs.len()
                                    && self.lowered.variables[stmt.input.var_id].ty
                                        == self.lowered.variables[construct_stmt.output].ty
                                    && zip_eq(
                                        stmt.outputs.iter(),
                                        construct_stmt.inputs.iter(),
                                    )
                                    .all(|(output, input)| {
                                        output == &self.var_remapper.map_var_id(input.var_id)
                                    }) =>
                            {
                                self.stmts_to_remove.push(**location);
                                Some(construct_stmt)
                            }
                            _ => {
                                can_remove_struct_destructure = false;
                                None
                            }
                        }
                    })
                    .collect_vec();

                if !(can_remove_struct_destructure
                    || self.lowered.variables[stmt.input.var_id].duplicatable.is_ok())
                {
                    // We can't remove any of of the construct statements.
                    self.stmts_to_remove.truncate(self.stmts_to_remove.len() - constructs.len());
                    return false;
                }

                // Mark the statements for removal and set the renaming for it outputs.
                if can_remove_struct_destructure {
                    self.stmts_to_remove.push(statement_location);
                }

                for construct in constructs {
                    self.rename_var(construct.output, stmt.input.var_id)
                }
                can_remove_struct_destructure
            }
            Statement::StructConstruct(stmt) => {
                let use_sites = get_use_sites(&self.use_sites, &stmt.output);

                let mut can_remove_struct_construct = true;
                let destructures = use_sites
                    .iter()
                    .filter_map(|location| {
                        if let Some(Statement::StructDestructure(destructure_stmt)) =
                            self.lowered.blocks[location.0].statements.get(location.1)
                        {
                            self.stmts_to_remove.push(*location);
                            Some(destructure_stmt)
                        } else {
                            can_remove_struct_construct = false;
                            None
                        }
                    })
                    .collect_vec();

                if !(can_remove_struct_construct
                    || stmt
                        .inputs
                        .iter()
                        .all(|input| self.lowered.variables[input.var_id].duplicatable.is_ok()))
                {
                    // We can't remove any of the destructure statements.
                    self.stmts_to_remove.truncate(self.stmts_to_remove.len() - destructures.len());
                    return false;
                }

                // Mark the statements for removal and set the renaming for it outputs.
                if can_remove_struct_construct {
                    self.stmts_to_remove.push(statement_location);
                }

                for destructure_stmt in destructures {
                    for (output, input) in
                        izip!(destructure_stmt.outputs.iter(), stmt.inputs.iter())
                    {
                        self.rename_var(*output, input.var_id);
                    }
                }
                can_remove_struct_construct
            }
            Statement::Snapshot(stmt) => {
                let use_sites = get_use_sites(&self.use_sites, &stmt.output_snapshot);

                let mut can_remove_snap = true;
                let desnaps = use_sites
                    .iter()
                    .filter_map(|location| {
                        if let Some(Statement::Desnap(desnap_stmt)) =
                            self.lowered.blocks[location.0].statements.get(location.1)
                        {
                            self.stmts_to_remove.push(*location);
                            Some(desnap_stmt)
                        } else {
                            can_remove_snap = false;
                            None
                        }
                    })
                    .collect_vec();

                let new_var = if can_remove_snap {
                    self.stmts_to_remove.push(statement_location);
                    self.rename_var(stmt.output_original, stmt.input.var_id);
                    stmt.input.var_id
                } else if desnaps.is_empty()
                    && self.lowered.variables[stmt.input.var_id].duplicatable.is_err()
                {
                    stmt.output_original
                } else {
                    stmt.input.var_id
                };

                for desnap in desnaps {
                    self.rename_var(desnap.output, new_var);
                }
                can_remove_snap
            }
            _ => false,
        }
    }
}

impl<'a> Analyzer<'a> for CancelOpsContext<'a> {
    type Info = ();

    fn visit_stmt(
        &mut self,
        _info: &mut Self::Info,
        statement_location: StatementLocation,
        stmt: &'a Statement,
    ) {
        if !self.handle_stmt(stmt, statement_location) {
            for input in stmt.inputs() {
                self.add_use_site(input.var_id, statement_location);
            }
        }
    }

    fn visit_goto(
        &mut self,
        _info: &mut Self::Info,
        statement_location: StatementLocation,
        _target_block_id: BlockId,
        remapping: &VarRemapping,
    ) {
        for src in remapping.values() {
            self.add_use_site(src.var_id, statement_location);
        }
    }

    fn merge_match(
        &mut self,
        statement_location: StatementLocation,
        match_info: &'a MatchInfo,
        _infos: &[Self::Info],
    ) -> Self::Info {
        for var in match_info.inputs() {
            self.add_use_site(var.var_id, statement_location);
        }
    }

    fn info_from_return(
        &mut self,
        statement_location: StatementLocation,
        vars: &[VarUsage],
    ) -> Self::Info {
        for var in vars {
            self.add_use_site(var.var_id, statement_location);
        }
    }
}

#[derive(Default)]
pub struct CancelOpsRebuilder {
    renamed_vars: UnorderedHashMap<VariableId, VariableId>,
}

impl Rebuilder for CancelOpsRebuilder {
    fn map_var_id(&mut self, var: VariableId) -> VariableId {
        let Some(mut new_var_id) = self.renamed_vars.get(&var).cloned() else {
            return var;
        };
        while let Some(new_id) = self.renamed_vars.get(&new_var_id) {
            new_var_id = *new_id;
        }

        self.renamed_vars.insert(var, new_var_id);
        new_var_id
    }

    fn map_block_id(&mut self, block: BlockId) -> BlockId {
        block
    }
}
