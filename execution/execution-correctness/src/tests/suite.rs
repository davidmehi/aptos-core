// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::execution_correctness::ExecutionCorrectness;
use aptos_crypto::{ed25519::*, traits::Signature};
use consensus_types::{block::Block, vote_proposal::VoteProposal};
use executor_test_helpers::{extract_signer, gen_ledger_info_with_sigs};

pub fn run_test_suite(executor_pair: (Box<dyn ExecutionCorrectness>, Option<Ed25519PublicKey>)) {
    let (mut config, _genesis_key) = aptos_genesis_tool::test_config();
    let signer = extract_signer(&mut config);
    let (executor, execution_pubkey) = executor_pair;
    let parent_block_id = executor.committed_block_id().unwrap();

    let block = Block::make_genesis_block();
    let block_id = block.id();

    let result = executor
        .execute_block(block.clone(), parent_block_id)
        .unwrap();

    if let Some(sig) = result.signature().as_ref() {
        let vote_proposal = VoteProposal::new(
            result.extension_proof(),
            block,
            result.epoch_state().clone(),
            false,
        );
        sig.verify(&vote_proposal, &execution_pubkey.unwrap())
            .unwrap();
    }

    let ledger_info_with_sigs = gen_ledger_info_with_sigs(1, &result, block_id, vec![&signer]);
    executor
        .commit_blocks(vec![block_id], ledger_info_with_sigs)
        .unwrap();
}
