use anyhow::{Error, Result};
use borsh::BorshDeserialize;
use jupiter::Side;
use phoenix_types::{
    dispatch::load_with_dispatch,
    market::{Ladder, LadderOrder, MarketHeader},
};
use std::{collections::HashMap, mem::size_of};

use jupiter_core::amm::{Amm, KeyedAccount};
use solana_sdk::{instruction::AccountMeta, pubkey, pubkey::Pubkey};

use jupiter::jupiter_override::{Swap, SwapLeg};
use jupiter_core::amm::{Quote, QuoteParams, SwapLegAndAccountMetas, SwapParams};

pub const PHOENIX_PROGRAM_ID: Pubkey = pubkey!("phnxNHfGNVjpVVuHkceK3MgwZ1bW25ijfWACKhVFbBH");

#[derive(Clone, Debug)]
pub struct JupiterPhoenix {
    /// The pubkey of the market account
    market_key: Pubkey,
    /// Will always be "Phoenix"
    label: String,
    /// The pubkey of the base mint
    base_mint: Pubkey,
    /// The pubkey of the quote mint
    quote_mint: Pubkey,
    /// The pubkey of the Phoenix program
    program_id: Pubkey,
    /// Only here for convenience
    base_decimals: u32,
    /// Only here for convenience
    quote_decimals: u32,
    /// The size of a base lot in base atoms
    base_lot_size: u64,
    /// The size of a quote lot in quote atoms
    quote_lot_size: u64,
    /// The number of a base lot in a base unit
    base_lots_per_base_unit: u64,
    /// The number of a quote lots per base unit in a tick (tick_size)
    tick_size_in_quote_lots_per_base_unit_per_tick: u64,
    /// Taker fee basis points
    taker_fee_bps: u16,
    /// The state of the orderbook (L2)
    ladder: Ladder,
}

impl JupiterPhoenix {
    pub fn new_from_keyed_account(keyed_account: &KeyedAccount) -> Result<Self> {
        let (header_bytes, bytes) = &keyed_account
            .account
            .data
            .split_at(size_of::<MarketHeader>());
        let header = MarketHeader::try_from_slice(header_bytes).unwrap();
        let market = load_with_dispatch(&header.market_size_params, bytes)
            .ok_or(Error::msg("market configuration not found"))?;
        let taker_fee_bps = market.inner.get_taker_bps();
        Ok(Self {
            market_key: keyed_account.key,
            label: "Phoenix".into(),
            base_mint: header.base_params.mint_key,
            quote_mint: header.quote_params.mint_key,
            program_id: PHOENIX_PROGRAM_ID,
            base_decimals: header.base_params.decimals,
            quote_decimals: header.quote_params.decimals,
            taker_fee_bps,
            base_lot_size: header.get_base_lot_size(),
            quote_lot_size: header.get_quote_lot_size(),
            base_lots_per_base_unit: market.inner.get_base_lots_per_base_unit(),
            tick_size_in_quote_lots_per_base_unit_per_tick: header
                .get_tick_size_in_quote_atoms_per_base_unit()
                / header.get_quote_lot_size(),
            ladder: market.inner.get_ladder(u64::MAX),
        })
    }

    pub fn get_base_decimals(&self) -> u32 {
        self.base_decimals
    }

    pub fn get_quote_decimals(&self) -> u32 {
        self.quote_decimals
    }
}

impl Amm for JupiterPhoenix {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn key(&self) -> Pubkey {
        self.market_key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.base_mint, self.quote_mint]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![self.market_key]
    }

    fn update(&mut self, accounts_map: &HashMap<Pubkey, Vec<u8>>) -> Result<()> {
        let market_account_data = accounts_map.get(&self.market_key).unwrap();
        let (header_bytes, bytes) = &market_account_data.split_at(size_of::<MarketHeader>());
        let header = MarketHeader::try_from_slice(header_bytes).unwrap();
        let market = load_with_dispatch(&header.market_size_params, bytes)
            .ok_or(Error::msg("market configuration not found"))?;
        self.ladder = market.inner.get_ladder(u64::MAX);
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let mut out_amount = 0;
        for a in self.ladder.asks.iter().take(5).rev() {
            println!(
                "       {} {}",
                ((a.price_in_ticks * self.tick_size_in_quote_lots_per_base_unit_per_tick) as f64
                    / self.quote_lot_size as f64)
                    / 10f64.powi(self.quote_decimals as i32),
                (a.size_in_base_lots * self.base_lot_size) as f64
                    / 10f64.powi(self.base_decimals as i32)
            );
        }

        for a in self.ladder.bids.iter().take(5) {
            println!(
                "{} {}",
                (a.size_in_base_lots * self.base_lot_size) as f64
                    / 10f64.powi(self.base_decimals as i32),
                ((a.price_in_ticks * self.tick_size_in_quote_lots_per_base_unit_per_tick) as f64
                    / self.quote_lot_size as f64)
                    / 10f64.powi(self.quote_decimals as i32),
            );
        }

        if quote_params.input_mint == self.base_mint {
            let mut base_lot_budget = quote_params.in_amount / self.base_lot_size;
            for LadderOrder {
                price_in_ticks,
                size_in_base_lots,
            } in self.ladder.bid.iter()
            {
                if base_lot_budget == 0 {
                    break;
                }
                out_amount += price_in_ticks
                    * size_in_base_lots.min(&base_lot_budget)
                    * self.tick_size_in_quote_lots_per_base_unit_per_tick
                    * self.quote_lot_size
                    / self.base_lots_per_base_unit;
                base_lot_budget = base_lot_budget.saturating_sub(*size_in_base_lots);
            }
        } else {
            let mut quote_lot_budget = quote_params.in_amount / self.quote_lot_size;
            for LadderOrder {
                price_in_ticks,
                size_in_base_lots,
            } in self.ladder.asks.iter()
            {
                if quote_lot_budget == 0 {
                    break;
                }
                let book_amount_in_quote_lots = price_in_ticks
                    * size_in_base_lots
                    * self.tick_size_in_quote_lots_per_base_unit_per_tick
                    / self.base_lots_per_base_unit;

                out_amount += size_in_base_lots.min(
                    &(quote_lot_budget * self.base_lots_per_base_unit
                        / self.tick_size_in_quote_lots_per_base_unit_per_tick
                        / price_in_ticks),
                ) * self.base_lot_size;
                quote_lot_budget = quote_lot_budget.saturating_sub(book_amount_in_quote_lots);
            }
        };

        // Not 100% accurate, but it's a reasoanble enough approximation
        Ok(Quote {
            out_amount: ((out_amount * 10000) - self.taker_fee_bps as u64) / 10000,
            ..Quote::default()
        })
    }

    fn get_swap_leg_and_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> Result<SwapLegAndAccountMetas> {
        let SwapParams {
            destination_mint,
            source_mint,
            user_destination_token_account,
            user_source_token_account,
            user_transfer_authority,
            ..
        } = swap_params;

        let log_authority = Pubkey::find_program_address(&["log".as_ref()], &self.program_id).0;

        let (side, base_account, quote_account) = if source_mint == &self.base_mint {
            if destination_mint != &self.quote_mint {
                return Err(Error::msg("Invalid quote mint"));
            }
            (
                Side::Ask,
                user_source_token_account,
                user_destination_token_account,
            )
        } else {
            if destination_mint != &self.base_mint {
                return Err(Error::msg("Invalid base mint"));
            }
            (
                Side::Bid,
                user_destination_token_account,
                user_source_token_account,
            )
        };

        let base_vault = Pubkey::find_program_address(
            &[b"vault", self.market_key.as_ref(), self.base_mint.as_ref()],
            &self.program_id,
        )
        .0;

        let quote_vault = Pubkey::find_program_address(
            &[b"vault", self.market_key.as_ref(), self.quote_mint.as_ref()],
            &self.program_id,
        )
        .0;

        let account_metas = vec![
            AccountMeta::new(self.market_key, false),
            AccountMeta::new(*user_transfer_authority, true),
            AccountMeta::new_readonly(log_authority, false),
            AccountMeta::new_readonly(self.program_id, false),
            AccountMeta::new(*base_account, false),
            AccountMeta::new(*quote_account, false),
            AccountMeta::new(base_vault, false),
            AccountMeta::new(quote_vault, false),
            AccountMeta::new_readonly(spl_token::id(), false),
        ];

        Ok(SwapLegAndAccountMetas {
            swap_leg: SwapLeg::Swap {
                /// TODO change to phoenix
                swap: Swap::Serum { side },
            },
            account_metas,
        })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}

#[test]
fn test_jupiter_phoenix_integration() {
    use jupiter_core::amm::Amm;
    use solana_client::rpc_client::RpcClient;
    use solana_sdk::pubkey::Pubkey;
    use std::collections::HashMap;

    const SOL_USDC_MARKET: Pubkey = pubkey!("5iLqmcg8vifdnnw6wEpVtQxFE4Few5uiceDWzi3jvzH8");

    // Going to borrow the Solana FM devnet RPC
    let rpc = RpcClient::new("https://qn-devnet.solana.fm/");
    let account = rpc.get_account(&SOL_USDC_MARKET).unwrap();

    let market_account = KeyedAccount {
        key: SOL_USDC_MARKET,
        account,
        params: None,
    };

    let mut jupiter_phoenix = JupiterPhoenix::new_from_keyed_account(&market_account).unwrap();

    let accounts_to_update = jupiter_phoenix.get_accounts_to_update();

    let accounts_map = rpc
        .get_multiple_accounts(&accounts_to_update)
        .unwrap()
        .iter()
        .enumerate()
        .fold(HashMap::new(), |mut m, (index, account)| {
            if let Some(account) = account {
                m.insert(accounts_to_update[index], account.data.clone());
            }
            m
        });
    jupiter_phoenix.update(&accounts_map).unwrap();
    let in_amount = 1_000_000_000_000;
    println!(
        "Getting quote for selling {} SOL",
        in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64)
    );
    let quote = jupiter_phoenix
        .quote(&QuoteParams {
            /// 1 SOL
            in_amount,
            input_mint: jupiter_phoenix.base_mint,
            output_mint: jupiter_phoenix.quote_mint,
        })
        .unwrap();

    let Quote { out_amount, .. } = quote;

    println!(
        "Quote result: {:?}",
        out_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_quote_decimals() as f64)
    );

    let in_amount = out_amount;

    println!(
        "Getting quote for buying SOL with {} USDC",
        in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_quote_decimals() as f64)
    );
    let quote = jupiter_phoenix
        .quote(&QuoteParams {
            in_amount,
            input_mint: jupiter_phoenix.quote_mint,
            output_mint: jupiter_phoenix.base_mint,
        })
        .unwrap();

    let Quote { out_amount, .. } = quote;

    println!(
        "Quote result: {:?}",
        out_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64)
    );
}
