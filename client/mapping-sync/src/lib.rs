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
use sp_api::ProvideRuntimeApi;
use sc_client_api::BlockOf;
use sp_blockchain::HeaderBackend;
use fp_rpc::EthereumRuntimeRPCApi;

pub fn sync_block<Block: BlockT>(
	backend: &fc_db::Backend<Block>,
	header: &Block::Header,
) -> Result<(), String> {
	let log = fp_consensus::find_log(header.digest()).map_err(|e| format!("{:?}", e))?;
	let post_hashes = log.into_hashes();

	let mapping_commitment = fc_db::MappingCommitment {
		block_hash: header.hash(),
		ethereum_block_hash: post_hashes.block_hash,
		ethereum_transaction_hashes: post_hashes.transaction_hashes,
	};
	log::debug!(target: "mapping-sync", "writing hashes: {:?}", mapping_commitment.block_hash);
	backend.mapping().write_hashes(mapping_commitment)?;

	Ok(())
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

	let block = client.runtime_api().current_block(&id)
		.map_err(|e| format!("{:?}", e))?;
	let block_hash = block.ok_or("Ethereum genesis block not found".to_string())?.header.hash();
	let mapping_commitment = fc_db::MappingCommitment::<Block> {
		block_hash: header.hash(),
		ethereum_block_hash: block_hash,
		ethereum_transaction_hashes: Vec::new(),
	};
	backend.mapping().write_hashes(mapping_commitment)?;

	Ok(())
}

pub fn sync_one_level<Block: BlockT, C, B>(
	client: &C,
	backend: &B,
	frontier_backend: &fc_db::Backend<Block>,
) -> Result<bool, String> where
	C: ProvideRuntimeApi<Block> + Send + Sync + HeaderBackend<Block> + BlockOf,
	C::Api: EthereumRuntimeRPCApi<Block>,
	B: sc_client_api::Backend<Block>,
{
	use sp_blockchain::Backend;
	let mut current_syncing_tips = frontier_backend.meta().current_syncing_tips()?;

	let substrate_backend = backend.blockchain();

	if current_syncing_tips.len() == 0 {
		// Sync genesis block.
	    log::debug!(target: "mapping-sync", "syncing genisis ");

		//
		// make sure we have some finalized block
		let _ = substrate_backend.last_finalized()
				.map_err(|e| format!("error get finalzied block: {:?}", e))?;

		let header = substrate_backend.header(BlockId::Number(Zero::zero()))
			.map_err(|e| format!("{:?}", e))?
			.ok_or("Genesis header not found".to_string())?;
		sync_genesis_block(client, frontier_backend, &header)?;

		current_syncing_tips.push(header.hash());
		frontier_backend.meta().write_current_syncing_tips(current_syncing_tips)?;

		Ok(true)
	} else {
		log::debug!(target: "mapping-sync", "syncing data");
		let mut syncing_tip_and_children = None;
		let last_finalized_block = substrate_backend.last_finalized()
							 .map_err(|e| format!("error get finalzied block: {:?}", e))?;
		let last_finalized = substrate_backend.block_number_from_id(&BlockId::Hash(last_finalized_block))
							 .map_err(|e| format!("error get finalzied block number: {:?}", e))?
							 .ok_or(format!(""))?;

		let mut actual_children_count = 0;

		for tip in &current_syncing_tips {
			let children = substrate_backend.children(*tip)
				.map_err(|e| format!("{:?}", e))?;

			actual_children_count = children.len();
			log::debug!(target: "mapping-sync", "syncing tip: {:?}, children: {:?}", tip, children);
			// make sure children is finalized
			let child = {
				let lock = backend.get_import_lock();
				children.iter().find(|c| {
					match substrate_backend.best_containing(*c.clone(), None, lock) {
						Ok(Some(_)) => true,
						Ok(_) => false,
						Err(e) => {
							log::warn!("failed to find best contains for {:?}", c);
							false
						},
					}
				})
			};

			if let Some(child) = child {
				let child_number = substrate_backend.block_number_from_id(&BlockId::Hash(child.clone()))
							 .map_err(|e| format!("error get child block number: {:?}", e))?
							 .ok_or(format!("error get child number"))?;
				log::debug!(target: "mapping-sync", "child number: {:?}, last finalized: {:?}", child_number, last_finalized);
				if child_number <= last_finalized {
					syncing_tip_and_children = Some((*tip, vec![child.clone()]));
					break
				}
			}
			if actual_children_count > 0 {
				return Ok(false)
			}
		}

		log::debug!(target: "mapping-sync", "syncing tips and children: {:?}", syncing_tip_and_children);

		if let Some((syncing_tip, children)) = syncing_tip_and_children {
			current_syncing_tips.retain(|tip| tip != &syncing_tip);

			for child in children {
				let header = substrate_backend.header(BlockId::Hash(child))
					.map_err(|e| format!("{:?}", e))?
					.ok_or("Header not found".to_string())?;

				log::debug!(target: "mapping-sync", "sync block!: {:?}", header);
				sync_block(frontier_backend, &header)?;
				current_syncing_tips.push(child);
			}
			frontier_backend.meta().write_current_syncing_tips(current_syncing_tips)?;

			Ok(true)
		} else {
			log::debug!(target: "mapping-sync", "no syncing tip and children");
			Ok(false)
		}
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
	B: sc_client_api::Backend<Block>,
{
	let mut synced_any = false;

	for _ in 0..limit {
		synced_any = synced_any || sync_one_level(client, substrate_backend, frontier_backend)?;
	}

	Ok(synced_any)
}
