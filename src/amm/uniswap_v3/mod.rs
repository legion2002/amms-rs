pub mod batch_request;
pub mod factory;

use std::{cmp::Ordering, collections::HashMap, sync::Arc};

use async_trait::async_trait;
use ethers::{
    abi::{decode, ethabi::Bytes, ParamType, Token},
    providers::Middleware,
    types::{BlockNumber, Filter, Log, H160, H256, I256, U256, U64},
};
use futures::future::join_all;
use num_bigfloat::BigFloat;
use serde::{Deserialize, Serialize};

use crate::{
    amm::AutomatedMarketMaker,
    errors::{ArithmeticError, DAMMError, EventLogError, SwapSimulationError},
};

use ethers::prelude::abigen;

use super::uniswap_v2::factory::PAIR_CREATED_EVENT_SIGNATURE;

abigen!(

    IUniswapV3Factory,
    r#"[
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool)
        event PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool)
    ]"#;

    IUniswapV3Pool,
    r#"[
        function token0() external view returns (address)
        function token1() external view returns (address)
        function liquidity() external view returns (uint128)
        function slot0() external view returns (uint160, int24, uint16, uint16, uint16, uint8, bool)
        function fee() external view returns (uint24)
        function tickSpacing() external view returns (int24)
        function ticks(int24 tick) external view returns (uint128, int128, uint256, uint256, int56, uint160, uint32, bool)
        function tickBitmap(int16 wordPosition) external view returns (uint256)
        function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data) external returns (int256, int256)
        event Swap( address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)
    ]"#;

    IErc20,
    r#"[
        function balanceOf(address account) external view returns (uint256)
        function decimals() external view returns (uint8)
    ]"#;


);

pub const MIN_SQRT_RATIO: U256 = U256([4295128739, 0, 0, 0]);
pub const MAX_SQRT_RATIO: U256 = U256([6743328256752651558, 17280870778742802505, 4294805859, 0]);
pub const SWAP_EVENT_SIGNATURE: H256 = H256([
    196, 32, 121, 249, 74, 99, 80, 215, 230, 35, 95, 41, 23, 73, 36, 249, 40, 204, 42, 200, 24,
    235, 100, 254, 216, 0, 78, 17, 95, 188, 202, 103,
]);

// Burn event signature
pub const BURN_EVENT_SIGNATURE: H256 = H256([
    12, 57, 108, 217, 137, 163, 159, 68, 89, 181, 250, 26, 237, 106, 154, 141, 205, 188, 69, 144,
    138, 207, 214, 126, 2, 140, 213, 104, 218, 152, 152, 44,
]);

// Mint event signature
pub const MINT_EVENT_SIGNATURE: H256 = H256([
    122, 83, 8, 11, 164, 20, 21, 139, 231, 236, 105, 185, 135, 181, 251, 125, 7, 222, 225, 1, 254,
    133, 72, 143, 8, 83, 174, 22, 35, 157, 11, 222,
]);

pub const U256_TWO: U256 = U256([2, 0, 0, 0]);
pub const Q128: U256 = U256([0, 0, 1, 0]);
pub const Q224: U256 = U256([0, 0, 0, 4294967296]);
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UniswapV3Pool {
    pub address: H160,
    pub token_a: H160,
    pub token_a_decimals: u8,
    pub token_b: H160,
    pub token_b_decimals: u8,
    pub liquidity: u128,
    pub sqrt_price: U256,
    pub fee: u32,
    pub tick: i32,
    pub tick_spacing: i32,
    pub tick_bitmap: HashMap<i16, U256>,
    pub ticks: HashMap<i32, Info>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Info {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub initialized: bool,
}

impl Info {
    pub fn new(liquidity_gross: u128, liquidity_net: i128, initialized: bool) -> Self {
        Info {
            liquidity_gross,

            liquidity_net,
            initialized,
        }
    }
}

#[async_trait]
impl AutomatedMarketMaker for UniswapV3Pool {
    fn address(&self) -> H160 {
        self.address
    }

    async fn sync<M: Middleware>(&mut self, middleware: Arc<M>) -> Result<(), DAMMError<M>> {
        batch_request::sync_v3_pool_batch_request(self, middleware.clone()).await?;
        Ok(())
    }

    //This defines the event signatures to listen to that will produce events to be passed into AMM::sync_from_log()
    fn sync_on_event_signatures(&self) -> Vec<H256> {
        vec![
            SWAP_EVENT_SIGNATURE,
            MINT_EVENT_SIGNATURE,
            BURN_EVENT_SIGNATURE,
        ]
    }

    fn sync_from_log(&mut self, log: &Log) -> Result<(), EventLogError> {
        let event_signature = log.topics[0];

        if event_signature == BURN_EVENT_SIGNATURE {
            self.sync_from_burn_log(log);
        } else if event_signature == MINT_EVENT_SIGNATURE {
            self.sync_from_mint_log(log);
        } else if event_signature == SWAP_EVENT_SIGNATURE {
            self.sync_from_swap_log(log);
        } else {
            Err(EventLogError::InvalidEventSignature)?
        }

        Ok(())
    }

    fn tokens(&self) -> Vec<H160> {
        vec![self.token_a, self.token_b]
    }

    fn calculate_price(&self, base_token: H160) -> Result<f64, ArithmeticError> {
        let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(self.sqrt_price)?;
        let shift = self.token_a_decimals as i8 - self.token_b_decimals as i8;

        let price = match shift.cmp(&0) {
            Ordering::Less => 1.0001_f64.powi(tick) / 10_f64.powi(-shift as i32),
            Ordering::Greater => 1.0001_f64.powi(tick) * 10_f64.powi(shift as i32),
            Ordering::Equal => 1.0001_f64.powi(tick),
        };

        if base_token == self.token_a {
            Ok(price)
        } else {
            Ok(1.0 / price)
        }
    }
    //TODO: document that this function will not populate the tick_bitmap and ticks, if you want to populate those, you must call populate_tick_data on an initialized pool.1.0001_f64
    async fn populate_data<M: Middleware>(
        &mut self,
        block_number: Option<u64>,
        middleware: Arc<M>,
    ) -> Result<(), DAMMError<M>> {
        batch_request::get_v3_pool_data_batch_request(self, block_number, middleware.clone())
            .await?;
        Ok(())
    }

    fn simulate_swap(&self, token_in: H160, amount_in: U256) -> Result<U256, SwapSimulationError> {
        if amount_in.is_zero() {
            return Ok(U256::zero());
        }

        let zero_for_one = token_in == self.token_a;

        //Set sqrt_price_limit_x_96 to the max or min sqrt price in the pool depending on zero_for_one
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + 1
        } else {
            MAX_SQRT_RATIO - 1
        };

        //Initialize a mutable state state struct to hold the dynamic simulated state of the pool
        let mut current_state = CurrentState {
            sqrt_price_x_96: self.sqrt_price, //Active price on the pool
            amount_calculated: I256::zero(),  //Amount of token_out that has been calculated
            amount_specified_remaining: I256::from_raw(amount_in), //Amount of token_in that has not been swapped
            tick: self.tick,                                       //Current i24 tick of the pool
            liquidity: self.liquidity, //Current available liquidity in the tick range
        };

        while current_state.amount_specified_remaining != I256::zero()
            && current_state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            //Initialize a new step struct to hold the dynamic state of the pool at each step
            let mut step = StepComputations {
                sqrt_price_start_x_96: current_state.sqrt_price_x_96, //Set the sqrt_price_start_x_96 to the current sqrt_price_x_96
                ..Default::default()
            };

            //Get the next tick from the current tick
            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            //Note: this could be removed as we are clamping in the batch contract
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            //Get the next sqrt price from the input amount
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)?;

            //Target spot price
            let swap_target_sqrt_ratio = if zero_for_one {
                if step.sqrt_price_next_x96 < sqrt_price_limit_x_96 {
                    sqrt_price_limit_x_96
                } else {
                    step.sqrt_price_next_x96
                }
            } else if step.sqrt_price_next_x96 > sqrt_price_limit_x_96 {
                sqrt_price_limit_x_96
            } else {
                step.sqrt_price_next_x96
            };

            //Compute swap step and update the current state
            (
                current_state.sqrt_price_x_96,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
            ) = uniswap_v3_math::swap_math::compute_swap_step(
                current_state.sqrt_price_x_96,
                swap_target_sqrt_ratio,
                current_state.liquidity,
                current_state.amount_specified_remaining,
                self.fee,
            )?;

            //Decrement the amount remaining to be swapped and amount received from the step
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_sub(I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;

            current_state.amount_calculated -= I256::from_raw(step.amount_out);

            //If the price moved all the way to the next price, recompute the liquidity change for the next iteration
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = self.ticks[&step.tick_next].liquidity_net;

                    // we are on a tick boundary, and the next tick is initialized, so we must charge a protocol fee
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        current_state.liquidity - (-liquidity_net as u128)
                    } else {
                        current_state.liquidity + (liquidity_net as u128)
                    };

                    //Increment the current tick
                    current_state.tick = if zero_for_one {
                        step.tick_next.wrapping_sub(1)
                    } else {
                        step.tick_next
                    }
                }
                //If the current_state sqrt price is not equal to the step sqrt price, then we are not on the same tick.
                //Update the current_state.tick to the tick at the current_state.sqrt_price_x_96
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )?;
            }
        }

        Ok((-current_state.amount_calculated).into_raw())
    }

    fn simulate_swap_mut(
        &mut self,
        token_in: H160,
        amount_in: U256,
    ) -> Result<U256, SwapSimulationError> {
        if amount_in.is_zero() {
            return Ok(U256::zero());
        }

        let zero_for_one = token_in == self.token_a;

        //Set sqrt_price_limit_x_96 to the max or min sqrt price in the pool depending on zero_for_one
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + 1
        } else {
            MAX_SQRT_RATIO - 1
        };

        //Initialize a mutable state state struct to hold the dynamic simulated state of the pool
        let mut current_state = CurrentState {
            sqrt_price_x_96: self.sqrt_price, //Active price on the pool
            amount_calculated: I256::zero(),  //Amount of token_out that has been calculated
            amount_specified_remaining: I256::from_raw(amount_in), //Amount of token_in that has not been swapped
            tick: self.tick,                                       //Current i24 tick of the pool
            liquidity: self.liquidity, //Current available liquidity in the tick range
        };

        while current_state.amount_specified_remaining != I256::zero()
            && current_state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            //Initialize a new step struct to hold the dynamic state of the pool at each step
            let mut step = StepComputations {
                sqrt_price_start_x_96: current_state.sqrt_price_x_96, //Set the sqrt_price_start_x_96 to the current sqrt_price_x_96
                ..Default::default()
            };

            //Get the next tick from the current tick
            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            //Note: this could be removed as we are clamping in the batch contract
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            //Get the next sqrt price from the input amount
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)?;

            //Target spot price
            let swap_target_sqrt_ratio = if zero_for_one {
                if step.sqrt_price_next_x96 < sqrt_price_limit_x_96 {
                    sqrt_price_limit_x_96
                } else {
                    step.sqrt_price_next_x96
                }
            } else if step.sqrt_price_next_x96 > sqrt_price_limit_x_96 {
                sqrt_price_limit_x_96
            } else {
                step.sqrt_price_next_x96
            };

            //Compute swap step and update the current state
            (
                current_state.sqrt_price_x_96,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
            ) = uniswap_v3_math::swap_math::compute_swap_step(
                current_state.sqrt_price_x_96,
                swap_target_sqrt_ratio,
                current_state.liquidity,
                current_state.amount_specified_remaining,
                self.fee,
            )?;

            //Decrement the amount remaining to be swapped and amount received from the step
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_sub(I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;

            current_state.amount_calculated -= I256::from_raw(step.amount_out);

            //If the price moved all the way to the next price, recompute the liquidity change for the next iteration
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = self.ticks[&step.tick_next].liquidity_net;

                    // we are on a tick boundary, and the next tick is initialized, so we must charge a protocol fee
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        current_state.liquidity - (-liquidity_net as u128)
                    } else {
                        current_state.liquidity + (liquidity_net as u128)
                    };

                    //Increment the current tick
                    current_state.tick = if zero_for_one {
                        step.tick_next.wrapping_sub(1)
                    } else {
                        step.tick_next
                    }
                }
                //If the current_state sqrt price is not equal to the step sqrt price, then we are not on the same tick.
                //Update the current_state.tick to the tick at the current_state.sqrt_price_x_96
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )?;
            }
        }

        //Update the pool state
        self.liquidity = current_state.liquidity;
        self.sqrt_price = current_state.sqrt_price_x_96;
        self.tick = current_state.tick;

        Ok((-current_state.amount_calculated).into_raw())
    }

    fn get_token_out(&self, token_in: H160) -> H160 {
        if self.token_a == token_in {
            self.token_b
        } else {
            self.token_a
        }
    }
}

impl UniswapV3Pool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: H160,
        token_a: H160,
        token_a_decimals: u8,
        token_b: H160,
        token_b_decimals: u8,
        fee: u32,
        liquidity: u128,
        sqrt_price: U256,
        tick: i32,
        tick_spacing: i32,
        tick_bitmap: HashMap<i16, U256>,
        ticks: HashMap<i32, Info>,
    ) -> UniswapV3Pool {
        UniswapV3Pool {
            address,
            token_a,
            token_a_decimals,
            token_b,
            token_b_decimals,
            fee,
            liquidity,
            sqrt_price,
            tick,
            tick_spacing,
            tick_bitmap,
            ticks,
        }
    }

    //TODO: document that this function will not populate the tick_bitmap and ticks, if you want to populate those, you must call populate_tick_data on an initialized pool.1.0001_f64

    //Creates a new instance of the pool from the pair address
    pub async fn new_from_address<M: Middleware>(
        pair_address: H160,
        creation_block: u64,
        middleware: Arc<M>,
    ) -> Result<Self, DAMMError<M>> {
        let mut pool = UniswapV3Pool {
            address: pair_address,
            token_a: H160::zero(),
            token_a_decimals: 0,
            token_b: H160::zero(),
            token_b_decimals: 0,
            liquidity: 0,
            sqrt_price: U256::zero(),
            tick: 0,
            tick_spacing: 0,
            fee: 0,
            tick_bitmap: HashMap::new(),
            ticks: HashMap::new(),
        };

        //We need to get tick spacing before populating tick data because tick spacing can not be uninitialized when syncing burn and mint logs
        pool.tick_spacing = pool.get_tick_spacing(middleware.clone()).await?;

        let synced_block = pool
            .populate_tick_data(creation_block, middleware.clone())
            .await?;

        //TODO: break this into two threads so it can happen concurrently?
        pool.populate_data(Some(synced_block), middleware).await?;

        if !pool.data_is_populated() {
            return Err(DAMMError::PoolDataError);
        }

        Ok(pool)
    }

    pub async fn new_from_log<M: Middleware>(
        log: Log,
        middleware: Arc<M>,
    ) -> Result<Self, DAMMError<M>> {
        let event_signature = log.topics[0];

        if event_signature == PAIR_CREATED_EVENT_SIGNATURE {
            let tokens =
                ethers::abi::decode(&[ParamType::Uint(32), ParamType::Address], &log.data)?;
            let pair_address = tokens[1].to_owned().into_address().unwrap();

            if let Some(block_number) = log.block_number {
                UniswapV3Pool::new_from_address(pair_address, block_number.as_u64(), middleware)
                    .await
            } else {
                Err(EventLogError::LogBlockNumberNotFound)?
            }
        } else {
            Err(EventLogError::InvalidEventSignature)?
        }
    }

    pub fn new_empty_pool_from_log(log: Log) -> Result<Self, EventLogError> {
        let event_signature = log.topics[0];

        if event_signature == PAIR_CREATED_EVENT_SIGNATURE {
            let tokens =
                ethers::abi::decode(&[ParamType::Uint(32), ParamType::Address], &log.data)?;
            let token_a = H160::from(log.topics[0]);
            let token_b = H160::from(log.topics[1]);
            let fee = tokens[0].to_owned().into_uint().unwrap().as_u32();
            let address = tokens[1].to_owned().into_address().unwrap();

            Ok(UniswapV3Pool {
                address,
                token_a,
                token_b,
                token_a_decimals: 0,
                token_b_decimals: 0,
                fee,
                liquidity: 0,
                sqrt_price: U256::zero(),
                tick_spacing: 0,
                tick: 0,
                tick_bitmap: HashMap::new(),
                ticks: HashMap::new(),
            })
        } else {
            Err(EventLogError::InvalidEventSignature)
        }
    }

    pub async fn populate_tick_data<M: Middleware>(
        &mut self,
        creation_block: u64,
        middleware: Arc<M>,
    ) -> Result<u64, DAMMError<M>> {
        let current_block = middleware
            .get_block_number()
            .await
            .map_err(DAMMError::MiddlewareError)?
            .as_u64();

        let step = 100000;
        //For each block within the range, get all logs asynchronously in batches
        for from_block in (creation_block..=current_block).step_by(step) {
            let to_block = from_block + step as u64;
            let filter = Filter::new()
                .topic0(vec![BURN_EVENT_SIGNATURE, MINT_EVENT_SIGNATURE])
                .address(self.address)
                .from_block(BlockNumber::Number(U64([from_block])))
                .to_block(BlockNumber::Number(U64([to_block])));

            for log in middleware
                .get_logs(&filter)
                .await
                .map_err(DAMMError::MiddlewareError)?
            {
                self.sync_from_log(&log)?;
            }
        }

        Ok(current_block)
    }

    pub fn fee(&self) -> u32 {
        self.fee
    }

    pub fn data_is_populated(&self) -> bool {
        !(self.token_a.is_zero() || self.token_b.is_zero())
    }

    pub async fn get_tick_word<M: Middleware>(
        &self,
        tick: i32,
        middleware: Arc<M>,
    ) -> Result<U256, DAMMError<M>> {
        let v3_pool = IUniswapV3Pool::new(self.address, middleware);
        let (word_position, _) = uniswap_v3_math::tick_bitmap::position(tick);
        Ok(v3_pool.tick_bitmap(word_position).call().await?)
    }

    pub async fn get_next_word<M: Middleware>(
        &self,
        word_position: i16,
        middleware: Arc<M>,
    ) -> Result<U256, DAMMError<M>> {
        let v3_pool = IUniswapV3Pool::new(self.address, middleware);
        Ok(v3_pool.tick_bitmap(word_position).call().await?)
    }

    pub async fn get_tick_spacing<M: Middleware>(
        &self,
        middleware: Arc<M>,
    ) -> Result<i32, DAMMError<M>> {
        let v3_pool = IUniswapV3Pool::new(self.address, middleware);
        Ok(v3_pool.tick_spacing().call().await?)
    }

    pub async fn get_tick<M: Middleware>(&self, middleware: Arc<M>) -> Result<i32, DAMMError<M>> {
        Ok(self.get_slot_0(middleware).await?.1)
    }

    pub async fn get_tick_info<M: Middleware>(
        &self,
        tick: i32,
        middleware: Arc<M>,
    ) -> Result<(u128, i128, U256, U256, i64, U256, u32, bool), DAMMError<M>> {
        let v3_pool = IUniswapV3Pool::new(self.address, middleware.clone());

        let tick_info = v3_pool.ticks(tick).call().await?;

        Ok((
            tick_info.0,
            tick_info.1,
            tick_info.2,
            tick_info.3,
            tick_info.4,
            tick_info.5,
            tick_info.6,
            tick_info.7,
        ))
    }

    pub async fn get_liquidity_net<M: Middleware>(
        &self,
        tick: i32,
        middleware: Arc<M>,
    ) -> Result<i128, DAMMError<M>> {
        let tick_info = self.get_tick_info(tick, middleware).await?;
        Ok(tick_info.1)
    }

    pub async fn get_initialized<M: Middleware>(
        &self,
        tick: i32,
        middleware: Arc<M>,
    ) -> Result<bool, DAMMError<M>> {
        let tick_info = self.get_tick_info(tick, middleware).await?;
        Ok(tick_info.7)
    }

    pub async fn get_slot_0<M: Middleware>(
        &self,
        middleware: Arc<M>,
    ) -> Result<(U256, i32, u16, u16, u16, u8, bool), DAMMError<M>> {
        let v3_pool = IUniswapV3Pool::new(self.address, middleware);
        Ok(v3_pool.slot_0().call().await?)
    }

    pub async fn get_liquidity<M: Middleware>(
        &self,
        middleware: Arc<M>,
    ) -> Result<u128, DAMMError<M>> {
        let v3_pool = IUniswapV3Pool::new(self.address, middleware);
        Ok(v3_pool.liquidity().call().await?)
    }

    pub async fn get_sqrt_price<M: Middleware>(
        &self,
        middleware: Arc<M>,
    ) -> Result<U256, DAMMError<M>> {
        Ok(self.get_slot_0(middleware).await?.0)
    }

    pub fn sync_from_burn_log(&mut self, log: &Log) {
        let (tick_lower, tick_upper, amount) = self.decode_burn_log(log);
        self.modify_position(tick_lower, tick_upper, -(amount as i128));
    }

    pub fn sync_from_mint_log(&mut self, log: &Log) {
        let (tick_lower, tick_upper, amount) = self.decode_mint_log(log);
        self.modify_position(tick_lower, tick_upper, amount as i128);
    }

    pub fn modify_position(&mut self, tick_lower: i32, tick_upper: i32, liquidity_delta: i128) {
        //We are only using this function when a mint or burn event is emitted, therefore we do not need to checkTicks as that has happened before the event is emitted
        self.update_position(tick_lower, tick_upper, liquidity_delta);

        if liquidity_delta != 0 {
            //if the tick is between the tick lower and tick upper, update the liquidity between the ticks
            if self.tick > tick_lower && self.tick < tick_upper {
                self.liquidity = if liquidity_delta < 0 {
                    self.liquidity - ((-liquidity_delta) as u128)
                } else {
                    self.liquidity + (liquidity_delta as u128)
                }
            }
        }
    }

    pub fn update_position(&mut self, tick_lower: i32, tick_upper: i32, liquidity_delta: i128) {
        let mut flipped_lower = false;
        let mut flipped_upper = false;

        if liquidity_delta != 0 {
            flipped_lower = self.update_tick(tick_lower, liquidity_delta, false);
            flipped_upper = self.update_tick(tick_upper, liquidity_delta, true);
            if flipped_lower {
                self.flip_tick(tick_lower, self.tick_spacing);
            }
            if flipped_upper {
                self.flip_tick(tick_upper, self.tick_spacing);
            }
        }

        if liquidity_delta < 0 {
            if flipped_lower {
                self.ticks.remove(&tick_lower);
            }

            if flipped_upper {
                self.ticks.remove(&tick_upper);
            }
        }
    }

    pub fn update_tick(&mut self, tick: i32, liquidity_delta: i128, upper: bool) -> bool {
        //TODO: sanity check this
        let info = match self.ticks.get_mut(&tick) {
            Some(info) => info,
            None => {
                self.ticks.insert(tick, Info::default());
                self.ticks.get_mut(&tick).unwrap()
            }
        };

        let liquidity_gross_before = info.liquidity_gross;

        let liquidity_gross_after = if liquidity_delta < 0 {
            liquidity_gross_before - ((-liquidity_delta) as u128)
        } else {
            liquidity_gross_before + (liquidity_delta as u128)
        };

        //we do not need to check if liqudity_gross_after > maxLiquidity because we are only calling update tick on a burn or mint log.
        // this should already be validated when a log is
        let flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);

        if liquidity_gross_before == 0 {
            info.initialized = true;
        }

        info.liquidity_gross = liquidity_gross_after;

        info.liquidity_net = if upper {
            info.liquidity_net - liquidity_delta
        } else {
            info.liquidity_net + liquidity_delta
        };

        flipped
    }

    pub fn flip_tick(&mut self, tick: i32, tick_spacing: i32) {
        let (word_pos, bit_pos) = uniswap_v3_math::tick_bitmap::position(tick / tick_spacing);
        let mask = U256::one() << bit_pos;

        if let Some(word) = self.tick_bitmap.get_mut(&word_pos) {
            *word ^= mask;
        } else {
            self.tick_bitmap.insert(word_pos, mask);
        }
    }

    pub fn sync_from_swap_log(&mut self, log: &Log) {
        (_, _, self.sqrt_price, self.liquidity, self.tick) = self.decode_swap_log(log);
    }

    //Returns reserve0, reserve1
    pub fn decode_swap_log(&self, swap_log: &Log) -> (I256, I256, U256, u128, i32) {
        let log_data = decode(
            &[
                ParamType::Int(256),  //amount0
                ParamType::Int(256),  //amount1
                ParamType::Uint(160), //sqrtPriceX96
                ParamType::Uint(128), //liquidity
                ParamType::Int(24),   //tick
            ],
            &swap_log.data,
        )
        .expect("Could not get log data");

        let amount_0 = I256::from_raw(log_data[0].to_owned().into_int().unwrap());
        let amount_1 = I256::from_raw(log_data[1].to_owned().into_int().unwrap());
        let sqrt_price = log_data[2].to_owned().into_uint().unwrap();
        let liquidity = log_data[3].to_owned().into_uint().unwrap().as_u128();
        let tick = I256::from_raw(log_data[4].to_owned().into_int().unwrap()).as_i32();

        (amount_0, amount_1, sqrt_price, liquidity, tick)
    }

    //Decodes the burn event log from a burned v3 position
    pub fn decode_burn_log(&self, burn_log: &Log) -> (i32, i32, u128) {
        let tick_lower =
            I256::from_raw(U256::from_big_endian(burn_log.topics[2].as_bytes())).as_i32();
        let tick_upper =
            I256::from_raw(U256::from_big_endian(burn_log.topics[3].as_bytes())).as_i32();

        let log_data = decode(
            &[
                ParamType::Uint(128), //amount
                ParamType::Uint(256), //amount0
                ParamType::Uint(256), //amount1
            ],
            &burn_log.data,
        )
        .expect("Could not get log data");

        let amount: u128 = log_data[0].to_owned().into_uint().unwrap().as_u128();

        (tick_lower, tick_upper, amount)
    }

    //Decodes mint log of a new v3 position
    pub fn decode_mint_log(&self, mint_log: &Log) -> (i32, i32, u128) {
        let tick_lower =
            I256::from_raw(U256::from_big_endian(mint_log.topics[2].as_bytes())).as_i32();
        let tick_upper =
            I256::from_raw(U256::from_big_endian(mint_log.topics[3].as_bytes())).as_i32();

        let log_data = decode(
            &[
                ParamType::Address,   //sender
                ParamType::Uint(128), //amount
                ParamType::Uint(256), //amount0
                ParamType::Uint(256), //amount1
            ],
            &mint_log.data,
        )
        .expect("Could not get log data");

        let amount = log_data[1].to_owned().into_uint().unwrap().as_u128();

        (tick_lower, tick_upper, amount)
    }

    pub async fn get_token_decimals<M: Middleware>(
        &mut self,
        middleware: Arc<M>,
    ) -> Result<(u8, u8), DAMMError<M>> {
        let token_a_decimals = IErc20::new(self.token_a, middleware.clone())
            .decimals()
            .call()
            .await?;

        let token_b_decimals = IErc20::new(self.token_b, middleware)
            .decimals()
            .call()
            .await?;

        Ok((token_a_decimals, token_b_decimals))
    }

    pub async fn get_fee<M: Middleware>(
        &mut self,
        middleware: Arc<M>,
    ) -> Result<u32, DAMMError<M>> {
        let fee = IUniswapV3Pool::new(self.address, middleware)
            .fee()
            .call()
            .await?;

        Ok(fee)
    }

    pub async fn get_token_0<M: Middleware>(
        &self,
        middleware: Arc<M>,
    ) -> Result<H160, DAMMError<M>> {
        let v3_pool = IUniswapV3Pool::new(self.address, middleware);

        let token_0 = match v3_pool.token_0().call().await {
            Ok(result) => result,
            Err(contract_error) => return Err(DAMMError::ContractError(contract_error)),
        };

        Ok(token_0)
    }

    pub async fn get_token_1<M: Middleware>(
        &self,
        middleware: Arc<M>,
    ) -> Result<H160, DAMMError<M>> {
        let v3_pool = IUniswapV3Pool::new(self.address, middleware);

        let token_1 = match v3_pool.token_1().call().await {
            Ok(result) => result,
            Err(contract_error) => return Err(DAMMError::ContractError(contract_error)),
        };

        Ok(token_1)
    }
    /* Legend:
       sqrt(price) = sqrt(y/x)
       L = sqrt(x*y)
       ==> x = L^2/price
       ==> y = L^2*price
    */
    pub fn calculate_virtual_reserves(&self) -> Result<(u128, u128), ArithmeticError> {
        let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(self.sqrt_price)?;
        let price = 1.0001_f64.powi(tick);

        let sqrt_price = BigFloat::from_f64(price.sqrt());
        let liquidity = BigFloat::from_u128(self.liquidity);

        //Sqrt price is stored as a Q64.96 so we need to left shift the liquidity by 96 to be represented as Q64.96
        //We cant right shift sqrt_price because it could move the value to 0, making divison by 0 to get reserve_x
        let liquidity = liquidity;

        let (reserve_0, reserve_1) = if !sqrt_price.is_zero() {
            let reserve_x = liquidity.div(&sqrt_price);
            let reserve_y = liquidity.mul(&sqrt_price);

            (reserve_x, reserve_y)
        } else {
            (BigFloat::from(0), BigFloat::from(0))
        };

        Ok((
            reserve_0
                .to_u128()
                .expect("Could not convert reserve_0 to uint128"),
            reserve_1
                .to_u128()
                .expect("Could not convert reserve_1 to uint128"),
        ))
    }

    pub async fn get_word<M: Middleware>(
        &self,
        word_pos: i16,
        block_number: Option<U64>,
        middleware: Arc<M>,
    ) -> Result<U256, DAMMError<M>> {
        if block_number.is_some() {
            //TODO: in the future, create a batch call to get this and liquidity net within the same call

            Ok(IUniswapV3Pool::new(self.address, middleware.clone())
                .tick_bitmap(word_pos)
                .block(block_number.unwrap())
                .call()
                .await?)
        } else {
            //TODO: in the future, create a batch call to get this and liquidity net within the same call
            Ok(IUniswapV3Pool::new(self.address, middleware.clone())
                .tick_bitmap(word_pos)
                .call()
                .await?)
        }
    }

    pub fn calculate_compressed(&self, tick: i32) -> i32 {
        if tick < 0 && tick % self.tick_spacing != 0 {
            (tick / self.tick_spacing) - 1
        } else {
            tick / self.tick_spacing
        }
    }

    pub fn calculate_word_pos_bit_pos(&self, compressed: i32) -> (i16, u8) {
        uniswap_v3_math::tick_bitmap::position(compressed)
    }

    pub fn swap_calldata(
        &self,
        recipient: H160,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        calldata: Vec<u8>,
    ) -> Bytes {
        let input_tokens = vec![
            Token::Address(recipient),
            Token::Bool(zero_for_one),
            Token::Int(amount_specified.into_raw()),
            Token::Uint(sqrt_price_limit_x_96),
            Token::Bytes(calldata),
        ];

        IUNISWAPV3POOL_ABI
            .function("swap")
            .unwrap()
            .encode_input(&input_tokens)
            .expect("Could not encode swap calldata")
    }
}

pub struct CurrentState {
    amount_specified_remaining: I256,
    amount_calculated: I256,
    sqrt_price_x_96: U256,
    tick: i32,
    liquidity: u128,
}

#[derive(Default)]
pub struct StepComputations {
    pub sqrt_price_start_x_96: U256,
    pub tick_next: i32,
    pub initialized: bool,
    pub sqrt_price_next_x96: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

const MIN_TICK: i32 = -887272;
const MAX_TICK: i32 = 887272;

pub struct Tick {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub fee_growth_outside_0_x_128: U256,
    pub fee_growth_outside_1_x_128: U256,
    pub tick_cumulative_outside: U256,
    pub seconds_per_liquidity_outside_x_128: U256,
    pub seconds_outside: u32,
    pub initialized: bool,
}

#[cfg(test)]
mod test {
    use super::IUniswapV3Pool;
    #[allow(unused)]
    #[allow(unused)]
    use super::UniswapV3Pool;
    use crate::{amm::AutomatedMarketMaker, errors::DAMMError};

    #[allow(unused)]
    use ethers::providers::Middleware;

    #[allow(unused)]
    use ethers::{
        prelude::abigen,
        providers::{Http, Provider},
        types::{H160, U256},
    };
    #[allow(unused)]
    use std::error::Error;
    #[allow(unused)]
    use std::{str::FromStr, sync::Arc};
    abigen!(
        IQuoter,
    r#"[
        function quoteExactInputSingle(address tokenIn, address tokenOut,uint24 fee, uint256 amountIn, uint160 sqrtPriceLimitX96) external returns (uint256 amountOut)
    ]"#;);

    async fn initialize_test_pool<M: Middleware>(
        middleware: Arc<M>,
    ) -> Result<(UniswapV3Pool, u64), DAMMError<M>> {
        let mut pool = UniswapV3Pool {
            address: H160::from_str("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640").unwrap(),
            ..Default::default()
        };

        let creation_block = 12369620;
        pool.tick_spacing = pool.get_tick_spacing(middleware.clone()).await?;
        let synced_block = pool
            .populate_tick_data(creation_block, middleware.clone())
            .await?;
        pool.populate_data(Some(synced_block), middleware).await?;

        Ok((pool, synced_block))
    }

    #[tokio::test]
    async fn test_simulate_swap_0() {
        let rpc_endpoint =
            std::env::var("ETHEREUM_RPC_ENDPOINT").expect("Could not get ETHEREUM_RPC_ENDPOINT");
        let middleware = Arc::new(Provider::<Http>::try_from(rpc_endpoint).unwrap());

        let (pool, synced_block) = initialize_test_pool(middleware.clone())
            .await
            .expect("could not initialize test pool");

        let quoter = IQuoter::new(
            H160::from_str("0xb27308f9f90d607463bb33ea1bebb41c27ce5ab6").unwrap(),
            middleware.clone(),
        );

        let amount_in = U256::from_dec_str("100000000").unwrap(); // 100 USDC

        let amount_out = pool.simulate_swap(pool.token_a, amount_in).unwrap();
        let expected_amount_out = quoter
            .quote_exact_input_single(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in,
                U256::zero(),
            )
            .block(synced_block)
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out);
        let amount_in_1 = U256::from_dec_str("10000000000").unwrap(); // 10_000 USDC

        let amount_out_1 = pool.simulate_swap(pool.token_a, amount_in_1).unwrap();

        let expected_amount_out_1 = quoter
            .quote_exact_input_single(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_1,
                U256::zero(),
            )
            .block(synced_block)
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1);

        let amount_in_2 = U256::from_dec_str("10000000000000").unwrap(); // 10_000_000 USDC

        let amount_out_2 = pool.simulate_swap(pool.token_a, amount_in_2).unwrap();

        let expected_amount_out_2 = quoter
            .quote_exact_input_single(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_2,
                U256::zero(),
            )
            .block(synced_block)
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2);

        let amount_in_3 = U256::from_dec_str("100000000000000").unwrap(); // 100_000_000 USDC

        let amount_out_3 = pool.simulate_swap(pool.token_a, amount_in_3).unwrap();

        let expected_amount_out_3 = quoter
            .quote_exact_input_single(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_3,
                U256::zero(),
            )
            .block(synced_block)
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3);
    }

    #[tokio::test]
    async fn test_simulate_swap_2() {
        let rpc_endpoint = std::env::var("ARBITRUM_MAINNET_ENDPOINT")
            .expect("Could not get ETHEREUM_RPC_ENDPOINT");
        let middleware = Arc::new(Provider::<Http>::try_from(rpc_endpoint).unwrap());
        let (pool, synced_block) = initialize_test_pool(middleware.clone())
            .await
            .expect("could not initialize test pool");

        let quoter = IQuoter::new(
            H160::from_str("0xb27308f9f90d607463bb33ea1bebb41c27ce5ab6").unwrap(),
            middleware.clone(),
        );

        let amount_in_2 = U256::from_dec_str("10000000000000").unwrap(); // 10_000_000 USDC

        let amount_out_2 = pool.simulate_swap(pool.token_a, amount_in_2).unwrap();

        let expected_amount_out_2 = quoter
            .quote_exact_input_single(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_2,
                U256::zero(),
            )
            .block(synced_block)
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2);
    }

    #[tokio::test]
    async fn test_get_new_from_address() {
        let rpc_endpoint = std::env::var("ARBITRUM_MAINNET_ENDPOINT")
            .expect("Could not get ETHEREUM_RPC_ENDPOINT");
        let middleware = Arc::new(Provider::<Http>::try_from(rpc_endpoint).unwrap());

        let pool = UniswapV3Pool::new_from_address(
            H160::from_str("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640").unwrap(),
            12369620,
            middleware.clone(),
        )
        .await
        .expect("could not initialize uniswap v3 pool from address");

        assert_eq!(
            pool.address,
            H160::from_str("0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640").unwrap()
        );
        assert_eq!(
            pool.token_a,
            H160::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap()
        );
        assert_eq!(pool.token_a_decimals, 6);
        assert_eq!(
            pool.token_b,
            H160::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap()
        );
        assert_eq!(pool.token_b_decimals, 18);
        assert_eq!(pool.fee, 500);
        assert!(pool.tick != 0);
        assert_eq!(pool.tick_spacing, 10);
    }

    #[tokio::test]
    async fn test_get_pool_data() {
        let rpc_endpoint =
            std::env::var("ETHEREUM_RPC_ENDPOINT").expect("Could not get ETHEREUM_RPC_ENDPOINT");
        let middleware = Arc::new(Provider::<Http>::try_from(rpc_endpoint).unwrap());

        let (pool, _synced_block) = initialize_test_pool(middleware.clone())
            .await
            .expect("could not initialize test pool");

        assert_eq!(
            pool.address,
            H160::from_str("0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640").unwrap()
        );
        assert_eq!(
            pool.token_a,
            H160::from_str("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap()
        );
        assert_eq!(pool.token_a_decimals, 6);
        assert_eq!(
            pool.token_b,
            H160::from_str("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").unwrap()
        );
        assert_eq!(pool.token_b_decimals, 18);
        assert_eq!(pool.fee, 500);
        assert!(pool.tick != 0);
        assert_eq!(pool.tick_spacing, 10);
    }

    #[tokio::test]
    async fn test_sync_pool() {
        let rpc_endpoint =
            std::env::var("ETHEREUM_RPC_ENDPOINT").expect("Could not get ETHEREUM_RPC_ENDPOINT");
        let middleware = Arc::new(Provider::<Http>::try_from(rpc_endpoint).unwrap());

        let mut pool = UniswapV3Pool {
            address: H160::from_str("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640").unwrap(),
            ..Default::default()
        };

        pool.sync(middleware).await.unwrap();

        //TODO: need to assert values
    }

    #[tokio::test]
    async fn test_calculate_virtual_reserves() {
        let rpc_endpoint =
            std::env::var("ETHEREUM_RPC_ENDPOINT").expect("Could not get ETHEREUM_RPC_ENDPOINT");
        let middleware = Arc::new(Provider::<Http>::try_from(rpc_endpoint).unwrap());

        let mut pool = UniswapV3Pool {
            address: H160::from_str("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640").unwrap(),
            ..Default::default()
        };

        pool.populate_data(None, middleware.clone()).await.unwrap();

        let pool_at_block = IUniswapV3Pool::new(
            H160::from_str("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640").unwrap(),
            middleware.clone(),
        );

        let sqrt_price = pool_at_block
            .slot_0()
            .block(16515398)
            .call()
            .await
            .unwrap()
            .0;
        let liquidity = pool_at_block
            .liquidity()
            .block(16515398)
            .call()
            .await
            .unwrap();

        pool.sqrt_price = sqrt_price;
        pool.liquidity = liquidity;

        dbg!(pool.sqrt_price);
        dbg!(pool.liquidity);

        let (r_0, r_1) = pool
            .calculate_virtual_reserves()
            .expect("Could not calculate virtual reserves");

        dbg!(r_0, r_1);

        assert_eq!(1067543429906214, r_0);
        assert_eq!(649198362624067343572319, r_1);
    }

    #[tokio::test]
    async fn test_calculate_price() {
        let rpc_endpoint =
            std::env::var("ETHEREUM_RPC_ENDPOINT").expect("Could not get ETHEREUM_RPC_ENDPOINT");
        let middleware = Arc::new(Provider::<Http>::try_from(rpc_endpoint).unwrap());

        let mut pool = UniswapV3Pool {
            address: H160::from_str("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640").unwrap(),
            ..Default::default()
        };

        pool.populate_data(None, middleware.clone()).await.unwrap();

        let block_pool = IUniswapV3Pool::new(
            H160::from_str("0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640").unwrap(),
            middleware.clone(),
        );

        let sqrt_price = block_pool.slot_0().block(16515398).call().await.unwrap().0;
        pool.sqrt_price = sqrt_price;

        let float_price_a = pool
            .calculate_price(pool.token_a)
            .expect("error when calculating price");

        let float_price_b = pool
            .calculate_price(pool.token_b)
            .expect("error when calculating price");

        dbg!(pool);

        assert_eq!(float_price_a, 0.0006081236083117488);
        assert_eq!(float_price_b, 1644.4025299004006);
    }
}
