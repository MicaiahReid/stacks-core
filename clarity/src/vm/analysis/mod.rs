// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

pub mod analysis_db;
pub mod arithmetic_checker;
pub mod contract_interface_builder;
#[allow(clippy::result_large_err)]
pub mod errors;
pub mod read_only_checker;
pub mod trait_checker;
pub mod type_checker;
pub mod types;

use stacks_common::types::StacksEpochId;

use self::arithmetic_checker::ArithmeticOnlyChecker;
use self::contract_interface_builder::build_contract_interface;
pub use self::errors::{CheckError, CheckErrors, CheckResult};
use self::read_only_checker::ReadOnlyChecker;
use self::trait_checker::TraitChecker;
use self::type_checker::v2_05::TypeChecker as TypeChecker2_05;
use self::type_checker::v2_1::TypeChecker as TypeChecker2_1;
pub use self::types::{AnalysisPass, ContractAnalysis};
use crate::vm::ast::{build_ast_with_rules, ASTRules};
use crate::vm::costs::LimitedCostTracker;
use crate::vm::database::{MemoryBackingStore, STORE_CONTRACT_SRC_INTERFACE};
use crate::vm::representations::SymbolicExpression;
use crate::vm::types::{QualifiedContractIdentifier, TypeSignature};
use crate::vm::ClarityVersion;

use super::database::ClarityDatabase;

/// Used by CLI tools like the docs generator. Not used in production
pub fn mem_type_check(
    snippet: &str,
    version: ClarityVersion,
    epoch: StacksEpochId,
) -> CheckResult<(Option<TypeSignature>, ContractAnalysis)> {
    let contract_identifier = QualifiedContractIdentifier::transient();
    let mut contract = build_ast_with_rules(
        &contract_identifier,
        snippet,
        &mut (),
        version,
        epoch,
        ASTRules::PrecheckSize,
    )
    .map_err(|_| CheckErrors::Expects("Failed to build AST".into()))?
    .expressions;

    let mut marf = MemoryBackingStore::new();
    let mut clarity_db = marf.as_clarity_db();
    let cost_tracker = LimitedCostTracker::new_free();
    match run_analysis(
        &QualifiedContractIdentifier::transient(),
        &mut contract,
        &mut clarity_db,
        false,
        cost_tracker,
        epoch,
        version,
    ) {
        Ok(x) => {
            // return the first type result of the type checker
            let first_type = x
                .type_map
                .as_ref()
                .ok_or_else(|| CheckErrors::Expects("Should be non-empty".into()))?
                .get_type(
                    x.expressions
                        .last()
                        .ok_or_else(|| CheckErrors::Expects("Should be non-empty".into()))?,
                )
                .cloned();
            Ok((first_type, x))
        }
        Err((e, _)) => Err(e),
    }
}

// Legacy function
// The analysis is not just checking type.
#[cfg(test)]
pub fn type_check(
    contract_identifier: &QualifiedContractIdentifier,
    expressions: &mut [SymbolicExpression],
    clarity_db: &mut ClarityDatabase,
    insert_contract: bool,
    epoch: &StacksEpochId,
    version: &ClarityVersion,
) -> CheckResult<ContractAnalysis> {
    run_analysis(
        contract_identifier,
        expressions,
        clarity_db,
        insert_contract,
        // for the type check tests, the cost tracker's epoch doesn't
        //  matter: the costs in those tests are all free anyways.
        LimitedCostTracker::new_free(),
        *epoch,
        *version,
    )
    .map_err(|(e, _cost_tracker)| e)
}

pub fn run_analysis(
    contract_identifier: &QualifiedContractIdentifier,
    expressions: &[SymbolicExpression],
    clarity_db: &mut ClarityDatabase,
    save_contract: bool,
    cost_tracker: LimitedCostTracker,
    epoch: StacksEpochId,
    version: ClarityVersion,
) -> Result<ContractAnalysis, (CheckError, LimitedCostTracker)> {
    let mut contract_analysis = ContractAnalysis::new(
        contract_identifier.clone(),
        expressions.to_vec(),
        cost_tracker,
        epoch,
        version,
    );

    let result = clarity_db.execute(|db| {
        ReadOnlyChecker::run_pass(&epoch, &mut contract_analysis, db)?;
        match epoch {
            StacksEpochId::Epoch20 | StacksEpochId::Epoch2_05 => {
                test_debug!("Running type checker for epoch 2.0");
                TypeChecker2_05::run_pass(&epoch, &mut contract_analysis, db)
            }
            StacksEpochId::Epoch21
            | StacksEpochId::Epoch22
            | StacksEpochId::Epoch23
            | StacksEpochId::Epoch24
            | StacksEpochId::Epoch25
            | StacksEpochId::Epoch30 => {
                test_debug!("Running type checker for epoch 2.1+");
                TypeChecker2_1::run_pass(&epoch, &mut contract_analysis, db)
            }
            StacksEpochId::Epoch10 => {
                return Err(CheckErrors::Expects(
                    "Epoch 1.0 is not a valid epoch for analysis".into(),
                )
                .into())
            }
        }?;
        TraitChecker::run_pass(&epoch, &mut contract_analysis, db)?;
        ArithmeticOnlyChecker::check_contract_cost_eligible(&mut contract_analysis);

        if STORE_CONTRACT_SRC_INTERFACE {
            let interface = build_contract_interface(&contract_analysis)?;
            contract_analysis.contract_interface = Some(interface);
        }
        if save_contract {
            db.insert_contract_analysis(&contract_analysis)
                .map_err(|e| CheckErrors::Expects(format!("Failed to save contract analysis: {}", e)))?;
        }

        Ok(())
    });
    match result {
        Ok(_) => Ok(contract_analysis),
        Err(e) => Err((e, contract_analysis.take_contract_cost_tracker())),
    }
}

#[cfg(test)]
mod tests;
