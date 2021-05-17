// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0
// This file is part of Frontier.
//
// Copyright (c) 2020 Parity Technologies (UK) Ltd.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

mod worker;

pub use worker::MappingSyncWorker;

use sp_runtime::{generic::BlockId, traits::{Block as BlockT, Header as HeaderT, Zero}};
use sp_api::{ApiExt, ProvideRuntimeApi};
use sc_client_api::BlockOf;
use sp_blockchain::HeaderBackend;
use fp_rpc::EthereumRuntimeRPCApi;
use fp_consensus::FindLogError;

pub fn sync_block<Block: BlockT>(
	backend: &fc_db::Backend<Block>,
	header: &Block::Header,
) -> Result<(), String> {
	match fp_consensus::find_log(header.digest()) {
		Ok(log) => {
			let post_hashes = log.into_hashes();

			let mapping_commitment = fc_db::MappingCommitment {
				block_hash: header.hash(),
				ethereum_block_hash: post_hashes.block_hash,
				ethereum_transaction_hashes: post_hashes.transaction_hashes,
			};
			backend.mapping().write_hashes(mapping_commitment)?;

			Ok(())
		},
		Err(FindLogError::NotFound) => {
			backend.mapping().write_none(header.hash())?;

			Ok(())
		},
		Err(FindLogError::MultipleLogs) => Err("Multiple logs found".to_string()),
	}

}

pub fn sync_genesis_block<Block: BlockT, C>(
	client: &C,
	backend: &fc_db::Backend<Block>,
	header: &Block::Header,
) -> Result<(), String> where
	C: ProvideRuntimeApi<Block> + Send + Sync + HeaderBackend<Block> + BlockOf,
	C::Api: EthereumRuntimeRPCApi<Block>,
{
	let id = BlockId::Hash(header.hash());

	let has_api = client.runtime_api().has_api::<dyn EthereumRuntimeRPCApi<Block, Error = ()>>(&id)
		.map_err(|e| format!("{:?}", e))?;

	if has_api {
		let block = client.runtime_api().current_block(&id)
			.map_err(|e| format!("{:?}", e))?;
		let block_hash = block.ok_or("Ethereum genesis block not found".to_string())?.header.hash();
		let mapping_commitment = fc_db::MappingCommitment::<Block> {
			block_hash: header.hash(),
			ethereum_block_hash: block_hash,
			ethereum_transaction_hashes: Vec::new(),
		};
		backend.mapping().write_hashes(mapping_commitment)?;
	} else {
		backend.mapping().write_none(header.hash())?;
	}

	Ok(())
}

pub fn rollback_last_block<Block: BlockT>(
	frontier_backend: &fc_db::Backend<Block>,
) -> Result<bool, String>
{
	let last_synced_block = frontier_backend.meta().last_synced_block()
		.map_err(|e| format!("{:?}", e))?
		.ok_or("failed to get last synced block")?;
	log::debug!(target: "mapping-sync", "rollback block: {:?}", last_synced_block);
	frontier_backend.mapping().rollback_block_by_id(&last_synced_block.hash)?;
	frontier_backend.meta().remove_block(&last_synced_block)?;

	if last_synced_block.number <= 0u32.into() { // already at genesis block, clear the last synced block
		frontier_backend.meta().clear_last_synced_block()?;
	} else {
		// should set the last synced block to the parent
		let number = last_synced_block.number - 1u32.into();
		let hash = frontier_backend.meta().get_synced_block_hash(&number)?;
		frontier_backend.meta().write_last_synced_block(&hash, &number)?;
	}

	Ok(true)
}

pub fn eusure_synced_blocks<Block: BlockT, B>(
	substrate_backend: &B,
	frontier_backend: &fc_db::Backend<Block>,
) -> Result<(), String> where
	B: sp_blockchain::HeaderBackend<Block> + sp_blockchain::Backend<Block>,
{
	loop {
		let last_synced_block = frontier_backend.meta().last_synced_block()?;
		// have synced some blocks
		if let Some(last_synced_block) = last_synced_block {
			// need to check last synced block is still in the chain
			// we need rollback to the last block that in the chain
			let header_on_chain = substrate_backend.header(BlockId::Number(last_synced_block.number))
				.map_err(|e| format!("{:?}", e))?;
			if let Some(header_on_chain) = header_on_chain {
				if header_on_chain.hash() != last_synced_block.hash {
					log::debug!(target: "mapping-sync", "last synced block hash doesn't match with chain data, last: {:?}, on chain: {:?}", last_synced_block, header_on_chain);
					rollback_last_block(frontier_backend)?;
				} else {
					break;
				}
			} else {
				break;
			}
		} else {
			break;
		}
	}
	Ok(())
}

pub fn sync_one_block<Block: BlockT, C, B>(
	client: &C,
	substrate_backend: &B,
	frontier_backend: &fc_db::Backend<Block>,
) -> Result<bool, String> where
	C: ProvideRuntimeApi<Block> + Send + Sync + HeaderBackend<Block> + BlockOf,
	C::Api: EthereumRuntimeRPCApi<Block>,
	B: sp_blockchain::HeaderBackend<Block> + sp_blockchain::Backend<Block>,
{
	// make sure the synced blocks are on the main chain
	eusure_synced_blocks(substrate_backend, frontier_backend)?;

	let last_synced_block = frontier_backend.meta().last_synced_block()?;
	// have synced some blocks
	if let Some(last_synced_block) = last_synced_block {
		let block_number = last_synced_block.number + 1u32.into();
		if substrate_backend.info().best_number < block_number {
			log::debug!(target: "mapping-sync", "{:?} is ahead of best block", block_number);
			return Ok(false)
		}

		let header = substrate_backend.header(BlockId::Number(last_synced_block.number + 1u32.into()))
			.map_err(|e| format!("{:?}", e))?
			.ok_or("Block header not found".to_string())?;

		sync_block(frontier_backend, &header)?;
		frontier_backend.meta().write_last_synced_block(&header.hash(), &header.number())?;

		return Ok(true)
	} else {
		let header = substrate_backend.header(BlockId::Number(Zero::zero()))
			.map_err(|e| format!("{:?}", e))?
			.ok_or("Genesis header not found".to_string())?;
        log::info!(target: "mapping-sync", "start sync genesis block");
		// no block synced, start with genesis block
		sync_genesis_block(client, frontier_backend, &header)?;
		frontier_backend.meta().write_last_synced_block(&header.hash(), &header.number())?;
		return Ok(true)
	}
}

pub fn sync_blocks<Block: BlockT, C, B>(
	client: &C,
	substrate_backend: &B,
	frontier_backend: &fc_db::Backend<Block>,
	limit: usize,
) -> Result<bool, String> where
	C: ProvideRuntimeApi<Block> + Send + Sync + HeaderBackend<Block> + BlockOf,
	C::Api: EthereumRuntimeRPCApi<Block>,
	B: sp_blockchain::HeaderBackend<Block> + sp_blockchain::Backend<Block>,
{
	let mut synced_any = false;

	for _ in 0..limit {
		synced_any = synced_any || sync_one_block(client, substrate_backend, frontier_backend)?;
	}

	Ok(synced_any)
}
