use anyhow::{Error, Result};
use jupiter::Side;
use phoenix::program::load_with_dispatch;
use phoenix::program::MarketHeader;
use phoenix::state::markets::{Ladder, LadderOrder};
use phoenix_sdk_core::sdk_client_core::MarketMetadata;
use std::ops::Deref;
use std::{collections::HashMap, mem::size_of};

use jupiter_core::amm::{Amm, KeyedAccount, PartialAccount};
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey};

use jupiter::jupiter_override::Swap;
use jupiter_core::amm::{Quote, QuoteParams, SwapAndAccountMetas, SwapParams};

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
    /// Contain the conversion functions for the market
    market_metadata: MarketMetadata,
    /// Taker fee basis points
    taker_fee_bps: u16,
    /// The state of the orderbook (L2)
    ladder: Ladder,
}

impl Deref for JupiterPhoenix {
    type Target = MarketMetadata;

    fn deref(&self) -> &Self::Target {
        &self.market_metadata
    }
}

impl JupiterPhoenix {
    pub fn new_from_keyed_account(keyed_account: &KeyedAccount) -> Result<Self> {
        let (header_bytes, bytes) = &keyed_account
            .account
            .data
            .split_at(size_of::<MarketHeader>());
        let header = bytemuck::try_from_bytes::<MarketHeader>(header_bytes).unwrap();
        let market = load_with_dispatch(&header.market_size_params, bytes)?;
        let taker_fee_bps = market.inner.get_taker_fee_bps();
        let market_metadata = MarketMetadata::from_header(header)?;
        Ok(Self {
            market_key: keyed_account.key,
            label: "Phoenix".into(),
            base_mint: header.base_params.mint_key,
            quote_mint: header.quote_params.mint_key,
            program_id: phoenix::id(),
            taker_fee_bps: taker_fee_bps as u16,
            market_metadata,
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
    fn program_id(&self) -> Pubkey {
        self.program_id
    }

    fn from_keyed_account(keyed_account: &KeyedAccount) -> Result<Self> {
        JupiterPhoenix::new_from_keyed_account(keyed_account)
    }

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

    fn update(&mut self, accounts_map: &HashMap<Pubkey, PartialAccount>) -> Result<()> {
        let market_account = accounts_map.get(&self.market_key).unwrap();
        let (header_bytes, bytes) = &market_account.data.split_at(size_of::<MarketHeader>());
        let header = bytemuck::try_from_bytes::<MarketHeader>(header_bytes).unwrap();
        let market = load_with_dispatch(&header.market_size_params, bytes)?;
        self.ladder = market.inner.get_ladder(u64::MAX);
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let mut out_amount = 0;
        if quote_params.input_mint == self.base_mint {
            let mut base_lot_budget = quote_params.in_amount / self.base_atoms_per_base_lot;
            for LadderOrder {
                price_in_ticks,
                size_in_base_lots,
            } in self.ladder.bids.iter()
            {
                if base_lot_budget == 0 {
                    break;
                }
                out_amount += self.base_lots_and_price_to_quote_atoms(
                    *size_in_base_lots.min(&base_lot_budget),
                    *price_in_ticks,
                );
                base_lot_budget = base_lot_budget.saturating_sub(*size_in_base_lots);
            }
        } else {
            let mut quote_lot_budget = quote_params.in_amount / self.quote_atoms_per_quote_lot;
            for LadderOrder {
                price_in_ticks,
                size_in_base_lots,
            } in self.ladder.asks.iter()
            {
                if quote_lot_budget == 0 {
                    break;
                }
                let book_amount_in_quote_lots =
                    self.base_lots_and_price_to_quote_atoms(*size_in_base_lots, *price_in_ticks);

                out_amount += size_in_base_lots.min(
                    &((quote_lot_budget * self.num_base_lots_per_base_unit)
                        / (self.tick_size_in_quote_atoms_per_base_unit * price_in_ticks)),
                ) * self.base_atoms_per_base_lot;
                quote_lot_budget = quote_lot_budget.saturating_sub(book_amount_in_quote_lots);
            }
        };

        // Not 100% accurate, but it's a reasoanble enough approximation
        Ok(Quote {
            out_amount: (out_amount * (10000 - self.taker_fee_bps as u64)) / 10000,
            ..Quote::default()
        })
    }

    fn get_swap_leg_and_account_metas(
        &self,
        swap_params: &SwapParams,
    ) -> Result<SwapAndAccountMetas> {
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

        Ok(SwapAndAccountMetas {
            swap: Swap::Serum { side },
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
    use solana_sdk::pubkey;
    use solana_sdk::pubkey::Pubkey;
    use std::collections::HashMap;

    const SOL_USDC_MARKET: Pubkey = pubkey!("4DoNfFBfF7UokCC2FQzriy7yHK6DY6NVdYpuekQ5pRgg");
    const BONK_USDC_MARKET: Pubkey = pubkey!("GBMoNx84HsFdVK63t8BZuDgyZhSBaeKWB4pHHpoeRM9z");

    let rpc = RpcClient::new("https://api.mainnet-beta.solana.com/");
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
                m.insert(
                    accounts_to_update[index],
                    PartialAccount::from(account.clone()),
                );
            }
            m
        });
    jupiter_phoenix.update(&accounts_map).unwrap();
    let in_amount = 1_000_000_000_000;
    println!(
        "Getting quote for selling {} SOL",
        in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64)
    );
    let quote_in = in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64);
    let quote = jupiter_phoenix
        .quote(&QuoteParams {
            /// 1 SOL
            in_amount,
            input_mint: jupiter_phoenix.base_mint,
            output_mint: jupiter_phoenix.quote_mint,
        })
        .unwrap();

    let Quote { out_amount, .. } = quote;

    let quote_out = out_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_quote_decimals() as f64);
    println!("Quote result: {:?} ({})", quote_out, quote_out / quote_in);

    let in_amount = out_amount;

    println!(
        "Getting quote for buying SOL with {} USDC",
        in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_quote_decimals() as f64)
    );
    let quote_in = in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_quote_decimals() as f64);
    let quote = jupiter_phoenix
        .quote(&QuoteParams {
            in_amount,
            input_mint: jupiter_phoenix.quote_mint,
            output_mint: jupiter_phoenix.base_mint,
        })
        .unwrap();

    let Quote { out_amount, .. } = quote;

    let quote_out = out_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64);
    println!(
        "Quote result: {:?} ({})",
        out_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64),
        quote_in / quote_out
    );

    let account = rpc.get_account(&BONK_USDC_MARKET).unwrap();

    let market_account = KeyedAccount {
        key: BONK_USDC_MARKET,
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
                m.insert(
                    accounts_to_update[index],
                    PartialAccount::from(account.clone()),
                );
            }
            m
        });
    jupiter_phoenix.update(&accounts_map).unwrap();
    let in_amount = 100_000_000_000_000;
    println!(
        "Getting quote for selling {} BONK",
        in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64)
    );
    let quote_in = in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64);
    let quote = jupiter_phoenix
        .quote(&QuoteParams {
            /// 1B Bonk
            in_amount,
            input_mint: jupiter_phoenix.base_mint,
            output_mint: jupiter_phoenix.quote_mint,
        })
        .unwrap();

    let Quote { out_amount, .. } = quote;

    let quote_out = out_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_quote_decimals() as f64);
    println!("Quote result: {:?} ({})", quote_out, quote_out / quote_in);

    let in_amount = out_amount;

    println!(
        "Getting quote for buying BONK with {} USDC",
        in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_quote_decimals() as f64)
    );
    let quote_in = in_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_quote_decimals() as f64);
    let quote = jupiter_phoenix
        .quote(&QuoteParams {
            in_amount,
            input_mint: jupiter_phoenix.quote_mint,
            output_mint: jupiter_phoenix.base_mint,
        })
        .unwrap();

    let Quote { out_amount, .. } = quote;

    let quote_out = out_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64);
    println!(
        "Quote result: {:?} ({})",
        out_amount as f64 / 10.0_f64.powf(jupiter_phoenix.get_base_decimals() as f64),
        quote_in / quote_out
    );
}
