use std::str::FromStr;
use std::convert::TryFrom;

use crate::orderbook::{Order, Executor};
use crate::state::{
    increase_last_order_id, read_last_order_id, read_order, read_orderbook, read_orderbooks,
    read_orders, read_orders_with_indexer, remove_order, store_order, PREFIX_ORDER_BY_BIDDER,
    PREFIX_ORDER_BY_PRICE, PREFIX_TICK, PREFIX_ORDER_BY_DIRECTION, read_config, remove_orderbook, read_excecutor, read_excecutors, increase_last_executor_id, store_executor, read_last_executor_id, read_last_distributed, store_last_distributed,
};
use cosmwasm_std::{
    Addr, CosmosMsg, Deps, DepsMut, MessageInfo, Order as OrderBy, Response, StdResult, Uint128, Attribute, Decimal, Env,
};

use oraiswap::asset::{pair_key, Asset, AssetInfo};
use oraiswap::error::ContractError;
use oraiswap::limit_order::{
    LastOrderIdResponse, OrderBookResponse, OrderBooksResponse, OrderDirection, OrderFilter,
    OrderResponse, OrdersResponse, OrderStatus,
};

pub fn submit_order(
    deps: DepsMut,
    sender: Addr,
    direction: OrderDirection,
    assets: [Asset; 2],
) -> Result<Response, ContractError> {
    if assets[0].amount.is_zero() || assets[1].amount.is_zero() {
        return Err(ContractError::AssetMustNotBeZero {})
    }
    
    let pair_key = pair_key(&[assets[0].to_raw(deps.api)?.info, assets[1].to_raw(deps.api)?.info]);
    let order_book = read_orderbook(deps.storage, &pair_key)?;
    let quote_asset = match direction {
        OrderDirection::Buy => assets[0].clone(),
        OrderDirection::Sell => assets[1].clone(),
    };

    // require minimum amount for quote asset
    if quote_asset.amount.lt(&order_book.min_quote_coin_amount) {
        return Err(ContractError::TooSmallQuoteAsset {
            quote_coin: quote_asset.info.to_string(),
            min_quote_amount: order_book.min_quote_coin_amount,
        });
    }

    let order_id = increase_last_order_id(deps.storage)?;

    store_order(
        deps.storage,
        &pair_key,
        &Order {
            order_id,
            direction,
            bidder_addr: deps.api.addr_canonicalize(sender.as_str())?,
            offer_amount: assets[0].to_raw(deps.api)?.amount,
            ask_amount: assets[1].to_raw(deps.api)?.amount,
            filled_offer_amount: Uint128::zero(),
            filled_ask_amount: Uint128::zero(),
            status: OrderStatus::Open,
        },
        true,
    )?;

    Ok(Response::new().add_attributes(vec![
        ("action", "submit_order"),
        ("pair", &format!("{} - {}", &assets[0].info, &assets[1].info)),
        ("order_id", &order_id.to_string()),
        ("status", &format!("{:?}", OrderStatus::Open)),
        ("direction", &format!("{:?}", direction)),
        ("bidder_addr", sender.as_str()),
        ("offer_asset", &format!("{} {}", &assets[0].amount, &assets[0].info)),
        ("ask_asset", &format!("{} {}", &assets[1].amount, &assets[1].info)),
    ]))
}

pub fn cancel_order(
    deps: DepsMut,
    info: MessageInfo,
    order_id: u64,
    asset_infos: [AssetInfo; 2],
) -> Result<Response, ContractError> {
    let pair_key = pair_key(&[asset_infos[0].to_raw(deps.api)?, asset_infos[1].to_raw(deps.api)?]);
    let mut order = read_order(deps.storage, &pair_key, order_id)?;

    if order.status == OrderStatus::Fulfilled {
        return Err(ContractError::OrderFulfilled {
            order_id: order.order_id,
        });
    }

    if order.status == OrderStatus::Filling {
        return Err(ContractError::OrderIsFilling {
            order_id: order.order_id,
        });
    }

    if order.bidder_addr != deps.api.addr_canonicalize(info.sender.as_str())? {
        return Err(ContractError::Unauthorized {});
    }

    // Compute refund asset
    let left_offer_amount = order.offer_amount.checked_sub(order.filled_offer_amount)?;

    let bidder_refund = Asset {
        info: match order.direction {
            OrderDirection::Buy => asset_infos[1].clone(),
            OrderDirection::Sell => asset_infos[0].clone(),
        },
        amount: left_offer_amount,
    };

    // Build refund msg
    let messages = if left_offer_amount > Uint128::zero() {
        vec![bidder_refund
            .clone()
            .into_msg(None, &deps.querier, info.sender)?]
    } else {
        vec![]
    };
    order.status = OrderStatus::Cancel;
    remove_order(deps.storage, &pair_key, &order)?;

    Ok(Response::new().add_messages(messages).add_attributes(vec![
        ("action", "cancel_order"),
        ("pair", &format!("{} - {}", &asset_infos[0], &asset_infos[1])),
        ("order_id", &order_id.to_string()),
        ("status", &format!("{:?}", OrderStatus::Cancel)),
        ("bidder_addr", &deps.api.addr_humanize(&order.bidder_addr)?.to_string()),
        ("bidder_refund", &bidder_refund.to_string()),
    ]))
}

pub fn excecute_pair(
    deps: DepsMut,
    info: MessageInfo,
    asset_infos: [AssetInfo; 2],
) -> Result<Response, ContractError> {
    let contract_info = read_config(deps.storage)?;
    let executor_addr = deps.api.addr_canonicalize(info.sender.as_str())?;
    let commission_rate = Decimal::from_str(&contract_info.commission_rate)?;

    let pair_key = pair_key(&[asset_infos[0].to_raw(deps.api)?, asset_infos[1].to_raw(deps.api)?]);
    let ob = read_orderbook(deps.storage, &pair_key)?;

    let executor_res = read_excecutor(deps.storage, &pair_key, &executor_addr);
    
    let mut executor = match executor_res {
        Ok(r_executor) => r_executor,
        Err(_err) => {
            increase_last_executor_id(deps.storage)?;
            Executor::new(
                executor_addr, 
                [
                    Asset {
                        info: asset_infos[0].clone(),
                        amount: Uint128::zero(),
                    },
                    Asset {
                        info: asset_infos[1].clone(),
                        amount: Uint128::zero(),
                    }
                ]
            )
        }
    };

    let (best_buy_price, best_sell_price) = ob.find_match_price(deps.as_ref().storage).unwrap();

    let mut match_one_price = false;
    if best_buy_price.eq(&best_sell_price) {
        match_one_price = true;
    }

    let mut match_buy_orders = ob.find_match_orders(deps.as_ref().storage, best_buy_price, OrderDirection::Buy);
    let mut match_sell_orders = ob.find_match_orders(deps.as_ref().storage, best_sell_price, OrderDirection::Sell);

    let mut messages: Vec<CosmosMsg> = vec![];
    let mut ret_attributes: Vec<Vec<Attribute>> = vec![];
    let mut total_orders =  0;

    let mut reward_amount ;

    for buy_order in &mut match_buy_orders {
        buy_order.status = OrderStatus::Filling;
        store_order(deps.storage, &pair_key, &buy_order, false)?;

        let bidder_addr = deps.api.addr_humanize(&buy_order.bidder_addr)?;
        let mut match_price = buy_order.get_price();

        for sell_order in &mut match_sell_orders {
            // check status of sell_order and buy_order
            if sell_order.status == OrderStatus::Fulfilled || buy_order.status == OrderStatus::Fulfilled {
                continue;
            }

            let lef_sell_offer_amount = sell_order.offer_amount.checked_sub(sell_order.filled_offer_amount)?;
            let lef_buy_offer_amount = buy_order.offer_amount.checked_sub(buy_order.filled_offer_amount)?;
            
            if lef_buy_offer_amount.is_zero() || lef_sell_offer_amount.is_zero() {
                continue;
            }

            sell_order.status = OrderStatus::Filling;
            store_order(deps.storage, &pair_key, &sell_order, false)?;

            if match_one_price == false {
                if sell_order.order_id < buy_order.order_id {
                    match_price = buy_order.get_price();
                } else {
                    match_price = sell_order.get_price();
                }
            }
            let lef_sell_ask_amount = Uint128::from(lef_sell_offer_amount * match_price);
            let lef_buy_ask_amount = Uint128::from(lef_buy_offer_amount * Uint128::from(1000000000000000000u128)).checked_div(match_price * Uint128::from(1000000000000000000u128)).unwrap();

            let mut sell_ask_asset = Asset {
                info: asset_infos[1].clone(),
                amount: Uint128::min(
                    lef_buy_offer_amount,
                    lef_sell_ask_amount,
                ),
            };

            let sell_offer_amount = Uint128::min(
                Uint128::from(sell_ask_asset.amount * Uint128::from(1000000000000000000u128)).checked_div(match_price * Uint128::from(1000000000000000000u128)).unwrap(),
                lef_sell_offer_amount,
            );

            if lef_buy_ask_amount.is_zero() {
                if lef_buy_offer_amount > Uint128::zero() {
                    executor.reward_assets[1].amount += lef_buy_offer_amount;
                    executor.reward_assets[1].info = asset_infos[1].clone();
                }

                buy_order.status = OrderStatus::Fulfilled;
                remove_order(deps.storage, &pair_key, buy_order)?;

                ret_attributes.push([
                    Attribute { key: "status".to_string(), value: format!("{:?}", buy_order.status) },
                    Attribute { key: "bidder_addr".to_string(), value: deps.api.addr_humanize(&buy_order.bidder_addr)?.to_string() },
                    Attribute { key: "order_id".to_string(), value: buy_order.order_id.to_string() },
                    Attribute { key: "direction".to_string(), value: format!("{:?}", buy_order.direction) },
                    Attribute { key: "offer_amount".to_string(), value: buy_order.offer_amount.to_string() },
                    Attribute { key: "filled_offer_amount".to_string(), value: buy_order.filled_offer_amount.to_string() },
                    Attribute { key: "ask_amount".to_string(), value: buy_order.ask_amount.to_string() },
                    Attribute { key: "filled_ask_amount".to_string(), value: buy_order.filled_ask_amount.to_string() },
                ].to_vec());

                sell_order.status = OrderStatus::Open;
                store_order(deps.storage, &pair_key, &sell_order, false)?;
                continue;                
            }

            if lef_sell_ask_amount.is_zero() || sell_offer_amount.is_zero() {
                if lef_sell_offer_amount > Uint128::zero() {
                    executor.reward_assets[0].amount += lef_sell_offer_amount;
                    executor.reward_assets[0].info = asset_infos[0].clone();
                }

                sell_order.status = OrderStatus::Fulfilled;
                remove_order(deps.storage, &pair_key, sell_order)?;
                ret_attributes.push([
                    Attribute { key: "status".to_string(), value: format!("{:?}", sell_order.status) },
                    Attribute { key: "bidder_addr".to_string(), value: deps.api.addr_humanize(&sell_order.bidder_addr)?.to_string() },
                    Attribute { key: "order_id".to_string(), value: sell_order.order_id.to_string() },
                    Attribute { key: "direction".to_string(), value: format!("{:?}", sell_order.direction) },
                    Attribute { key: "offer_amount".to_string(), value: sell_order.offer_amount.to_string() },
                    Attribute { key: "filled_offer_amount".to_string(), value: sell_order.filled_offer_amount.to_string() },
                    Attribute { key: "ask_amount".to_string(), value: sell_order.ask_amount.to_string() },
                    Attribute { key: "filled_ask_amount".to_string(), value: sell_order.filled_ask_amount.to_string() },
                ].to_vec());

                buy_order.status = OrderStatus::Open;
                store_order(deps.storage, &pair_key, &buy_order, false)?;
                continue;
            }

            let asker_addr = deps.api.addr_humanize(&sell_order.bidder_addr)?;

            // fill this order
            sell_order.fill_order(deps.storage, &pair_key, sell_ask_asset.amount, sell_offer_amount)?;

            if !sell_ask_asset.amount.is_zero() {
                reward_amount = sell_ask_asset.amount * commission_rate;
                executor.reward_assets[1].amount += reward_amount;
                executor.reward_assets[1].info = asset_infos[1].clone();
                sell_ask_asset.amount = sell_ask_asset.amount.checked_sub(reward_amount)?;

                messages.push(sell_ask_asset.into_msg(None, &deps.querier, asker_addr)?);

                ret_attributes.push([
                    Attribute { key: "status".to_string(), value: format!("{:?}", sell_order.status) },
                    Attribute { key: "bidder_addr".to_string(), value: deps.api.addr_humanize(&sell_order.bidder_addr)?.to_string() },
                    Attribute { key: "order_id".to_string(), value: sell_order.order_id.to_string() },
                    Attribute { key: "direction".to_string(), value: format!("{:?}", sell_order.direction) },
                    Attribute { key: "offer_amount".to_string(), value: sell_order.offer_amount.to_string() },
                    Attribute { key: "filled_offer_amount".to_string(), value: sell_order.filled_offer_amount.to_string() },
                    Attribute { key: "ask_amount".to_string(), value: sell_order.ask_amount.to_string() },
                    Attribute { key: "filled_ask_amount".to_string(), value: sell_order.filled_ask_amount.to_string() },
                ].to_vec());
            }

            if sell_order.status == OrderStatus::Fulfilled {
                total_orders += 1;
            } else {
                sell_order.status = OrderStatus::Open;
                store_order(deps.storage, &pair_key, &sell_order, false)?;
            }

            // Match with buy order
            if !sell_offer_amount.is_zero() {
                buy_order.fill_order(
                    deps.storage,
                    &pair_key,
                    sell_offer_amount,
                    sell_ask_asset.amount,
                )?;

                let mut buy_ask_asset = Asset {
                    info: asset_infos[0].clone(),
                    amount: sell_offer_amount,
                };
                reward_amount = buy_ask_asset.amount * commission_rate;
                executor.reward_assets[0].amount += reward_amount;
                executor.reward_assets[0].info = asset_infos[0].clone();
                buy_ask_asset.amount = buy_ask_asset.amount.checked_sub(reward_amount)?;

                // dont use oracle for limit order
                messages.push(buy_ask_asset.into_msg(
                    None,
                    &deps.querier,
                    deps.api.addr_validate(bidder_addr.as_str())?,
                )?);

                ret_attributes.push([
                    Attribute { key: "status".to_string(), value: format!("{:?}", buy_order.status) },
                    Attribute { key: "bidder_addr".to_string(), value: deps.api.addr_humanize(&buy_order.bidder_addr)?.to_string() },
                    Attribute { key: "order_id".to_string(), value: buy_order.order_id.to_string() },
                    Attribute { key: "direction".to_string(), value: format!("{:?}", buy_order.direction) },
                    Attribute { key: "offer_amount".to_string(), value: buy_order.offer_amount.to_string() },
                    Attribute { key: "filled_offer_amount".to_string(), value: buy_order.filled_offer_amount.to_string() },
                    Attribute { key: "ask_amount".to_string(), value: buy_order.ask_amount.to_string() },
                    Attribute { key: "filled_ask_amount".to_string(), value: buy_order.filled_ask_amount.to_string() },
                ].to_vec());
            }
        }

        if buy_order.status == OrderStatus::Fulfilled {
            total_orders += 1;
        } else {
            buy_order.status = OrderStatus::Open;
            store_order(deps.storage, &pair_key, &buy_order, false)?;
        }
    }
    store_executor(deps.storage, &pair_key, &executor)?;

    Ok(Response::new().add_messages(messages)
        .add_attributes(vec![
            ("action", "execute_orderbook_pair"),
            ("pair", &format!("{} - {}", &asset_infos[0], &asset_infos[1])),
            ("list_order_matched", &format!("{:?}", &ret_attributes)),
            ("total_matched_orders", &total_orders.to_string()),
        ])
    )  
}

pub fn distribute_reward(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    asset_infos: Vec<[AssetInfo; 2]>,
) -> Result<Response, ContractError> {
    let contract_info = read_config(deps.storage)?;
    let sender_addr = deps.api.addr_canonicalize(info.sender.as_str())?;

    if contract_info.admin.ne(&sender_addr) {
        return Err(ContractError::Unauthorized {});
    }

    let now = env.block.time.seconds();
    let mut messages: Vec<CosmosMsg> = vec![];
    let mut reward_assets: [Asset; 2];
    for asset_info in asset_infos {
        let pair_key = pair_key(&[asset_info[0].to_raw(deps.api)?, asset_info[1].to_raw(deps.api)?]);
        let num_executor = read_last_executor_id(deps.storage).unwrap_or_default();
        let list_executor = read_excecutors(deps.storage, &pair_key, None, Some(num_executor), None)?;
        // default is init time
        let last_distributed = read_last_distributed(deps.storage, &pair_key)
            .unwrap_or(now - contract_info.distribution_interval - 1);

        let last_time_elapsed = now - last_distributed;

        if last_time_elapsed < contract_info.distribution_interval {
            // Cannot distribute reward tokens before interval, process next one
            continue;
        }

        // store last distributed
        store_last_distributed(deps.storage, &&pair_key, now)?;

        for mut executor in list_executor {
            reward_assets = [
                Asset {
                    info: asset_info[0].clone(),
                    amount: executor.reward_assets[0].amount,
                },
                Asset {
                    info: asset_info[1].clone(),
                    amount: executor.reward_assets[1].amount,
                }
            ];
            messages.push(reward_assets[0].into_msg(
                None,
                &deps.querier,
                deps.api.addr_validate(deps.api.addr_humanize(&executor.address)?.as_str())?,
            )?);
            messages.push(reward_assets[1].into_msg(
                None,
                &deps.querier,
                deps.api.addr_validate(deps.api.addr_humanize(&executor.address)?.as_str())?,
            )?);
            executor.reward_assets[0].amount = Uint128::zero();
            executor.reward_assets[1].amount = Uint128::zero();
            store_executor(deps.storage, &pair_key, &executor)?;
        }
    }

    Ok(Response::new().add_messages(messages).add_attributes(vec![
        ("action", "distribute_rewards"),
    ]))
}

pub fn remove_pair(
    deps: DepsMut,
    info: MessageInfo,
    asset_infos: [AssetInfo; 2],
) -> Result<Response, ContractError> {
    let contract_info = read_config(deps.storage)?;
    let sender_addr = deps.api.addr_canonicalize(info.sender.as_str())?;

    if contract_info.admin.ne(&sender_addr) {
        return Err(ContractError::Unauthorized {});
    }

    let pair_key = pair_key(&[asset_infos[0].to_raw(deps.api)?, asset_infos[1].to_raw(deps.api)?]);
    let ob = read_orderbook(deps.storage, &pair_key)?;

    let mut all_orders = ob.get_orders(deps.storage, None, None, Some(OrderBy::Ascending))?;
    
    let mut messages: Vec<CosmosMsg> = vec![];
    let mut ret_attributes: Vec<Vec<Attribute>> = vec![];
    let mut total_orders =  0;
    for order in &mut all_orders {
        // Compute refund asset
        let left_offer_amount = order.offer_amount.checked_sub(order.filled_offer_amount)?;

        let bidder_refund = Asset {
            info: match order.direction {
                OrderDirection::Buy => asset_infos[1].clone(),
                OrderDirection::Sell => asset_infos[0].clone(),
            },
            amount: left_offer_amount,
        };

        // Build refund msg
        if left_offer_amount > Uint128::zero() {
            messages.push(bidder_refund.into_msg(None, &deps.querier, deps.api.addr_humanize(&order.bidder_addr)?)?);
        }
        total_orders += 1;
        order.status = OrderStatus::Cancel;
        remove_order(deps.storage, &pair_key, &order)?;
        ret_attributes.push([
            Attribute { key: "status".to_string(), value: format!("{:?}", order.status) },
            Attribute { key: "bidder_addr".to_string(), value: deps.api.addr_humanize(&order.bidder_addr)?.to_string() },
            Attribute { key: "order_id".to_string(), value: order.order_id.to_string() },
            Attribute { key: "direction".to_string(), value: format!("{:?}", order.direction)},
            Attribute { key: "offer_amount".to_string(), value: order.offer_amount.to_string() },
            Attribute { key: "filled_offer_amount".to_string(), value: order.filled_offer_amount.to_string() },
            Attribute { key: "ask_amount".to_string(), value: order.ask_amount.to_string() },
            Attribute { key: "filled_ask_amount".to_string(), value: order.ask_amount.to_string() },
        ].to_vec());
    }

    remove_orderbook(deps.storage, &pair_key);

    Ok(Response::new().add_messages(messages).add_attributes(vec![
        ("action", "remove_orderbook_pair"),
        ("pair", &format!("{} - {}", &asset_infos[0], &asset_infos[1])),
        ("list_order_removed", &format!("{:?}", &ret_attributes)),
        ("total_removed_orders", &total_orders.to_string()),
    ]))
}

pub fn query_order(
    deps: Deps,
    asset_infos: [AssetInfo; 2],
    order_id: u64,
) -> StdResult<OrderResponse> {
    let pair_key = pair_key(&[asset_infos[0].to_raw(deps.api)?, asset_infos[1].to_raw(deps.api)?]);
    let order = read_order(deps.storage, &pair_key, order_id)?;
    order.to_response(deps.api, asset_infos[0].clone(), asset_infos[1].clone())
}

pub fn query_orders(
    deps: Deps,
    asset_infos: [AssetInfo; 2],
    direction: Option<OrderDirection>,
    filter: OrderFilter,
    start_after: Option<u64>,
    limit: Option<u32>,
    order_by: Option<i32>,
) -> StdResult<OrdersResponse> {
    let order_by = order_by.map_or(None, |val| OrderBy::try_from(val).ok());
    let pair_key = pair_key(&[asset_infos[0].to_raw(deps.api)?, asset_infos[1].to_raw(deps.api)?]);

    let (direction_filter, direction_key): (Box<dyn Fn(&OrderDirection) -> bool>, Vec<u8>) =
        match direction {
            // copy value to closure
            Some(d) => (Box::new(move |x| d.eq(x)), d.as_bytes().to_vec()),
            None => (Box::new(|_| true), OrderDirection::Buy.as_bytes().to_vec()),
        };

    let orders: Vec<Order> = match filter {
        OrderFilter::Bidder(bidder_addr) => {
            let bidder_addr_raw = deps.api.addr_canonicalize(&bidder_addr)?;
            read_orders_with_indexer::<OrderDirection>(
                deps.storage,
                &[
                    PREFIX_ORDER_BY_BIDDER,
                    &pair_key,
                    bidder_addr_raw.as_slice(),
                ],
                direction_filter,
                start_after,
                limit,
                order_by,
            )?
        }
        OrderFilter::Tick {} => read_orders_with_indexer::<u64>(
            deps.storage,
            &[PREFIX_TICK, &pair_key, &direction_key],
            Box::new(|_| true),
            start_after,
            limit,
            order_by,
        )?,
        OrderFilter::Price(price) => {
            let price_key = price.atomics().to_be_bytes();
            read_orders_with_indexer::<OrderDirection>(
                deps.storage,
                &[PREFIX_ORDER_BY_PRICE, &pair_key, &price_key],
                direction_filter,
                start_after,
                limit,
                order_by,
            )?
        }
        OrderFilter::None => {
            match direction {
                Some(_) => {
                    read_orders_with_indexer::<OrderDirection>(
                        deps.storage,
                        &[PREFIX_ORDER_BY_DIRECTION, &pair_key, &direction_key],
                        direction_filter,
                        start_after,
                        limit,
                        order_by,
                    )?
                },
                None => read_orders(deps.storage, &pair_key, start_after, limit, order_by)?,
            }
        },
    };

    let resp = OrdersResponse {
        orders: orders
            .iter()
            .map(|order| order.to_response(deps.api, asset_infos[0].clone(), asset_infos[1].clone()))
            .collect::<StdResult<Vec<OrderResponse>>>()?,
    };

    Ok(resp)
}

pub fn query_last_order_id(deps: Deps) -> StdResult<LastOrderIdResponse> {
    let last_order_id = read_last_order_id(deps.storage)?;
    let resp = LastOrderIdResponse { last_order_id };

    Ok(resp)
}

pub fn query_orderbooks(
    deps: Deps,
    start_after: Option<Vec<u8>>,
    limit: Option<u32>,
    order_by: Option<i32>,
) -> StdResult<OrderBooksResponse> {
    let order_by = order_by.map_or(None, |val| OrderBy::try_from(val).ok());
    let order_books = read_orderbooks(deps.storage, start_after, limit, order_by)?;
    order_books
        .into_iter()
        .map(|ob| ob.to_response(deps.api))
        .collect::<StdResult<Vec<OrderBookResponse>>>()
        .map(|order_books| OrderBooksResponse { order_books })
}

pub fn query_orderbook(
    deps: Deps,
    asset_infos: [AssetInfo; 2],
) -> StdResult<OrderBookResponse> {
    let pair_key = pair_key(&[asset_infos[0].to_raw(deps.api)?, asset_infos[1].to_raw(deps.api)?]);
    let ob = read_orderbook(deps.storage, &pair_key)?;
    ob.to_response(deps.api)
}
