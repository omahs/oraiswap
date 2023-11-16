use cosmwasm_std::{
    entry_point, from_binary, to_binary, Addr, Attribute, Binary, CosmosMsg, Decimal, Deps,
    DepsMut, Env, MessageInfo, Response, StdError, StdResult,
};
use cw20::Cw20ReceiveMsg;
use oraiswap::math::Converter128;

use crate::state::{
    read_config, read_token_ratio, store_config, store_token_ratio, token_ratio_remove, Config,
};

use oraiswap::converter::{
    ConfigResponse, ConvertInfoResponse, Cw20HookMsg, ExecuteMsg, InstantiateMsg, MigrateMsg,
    QueryMsg, TokenInfo, TokenRatio,
};

use oraiswap::asset::{Asset, AssetInfo};

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    _msg: InstantiateMsg,
) -> StdResult<Response> {
    store_config(
        deps.storage,
        &Config {
            owner: deps.api.addr_canonicalize(info.sender.as_str())?,
        },
    )?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(deps: DepsMut, env: Env, info: MessageInfo, msg: ExecuteMsg) -> StdResult<Response> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::UpdateConfig { owner } => update_config(deps, info, owner),
        ExecuteMsg::UpdatePair { from, to } => update_pair(deps, info, from, to),
        ExecuteMsg::UnregisterPair { from } => unregister_pair(deps, info, from),
        ExecuteMsg::Convert {} => convert(deps, env, info),
        ExecuteMsg::ConvertReverse { from_asset } => convert_reverse(deps, env, info, from_asset),
        ExecuteMsg::WithdrawTokens { asset_infos } => withdraw_tokens(deps, env, info, asset_infos),
    }
}

pub fn update_config(deps: DepsMut, info: MessageInfo, owner: Addr) -> StdResult<Response> {
    let mut config: Config = read_config(deps.storage)?;

    if config.owner != deps.api.addr_canonicalize(info.sender.as_str())? {
        return Err(StdError::generic_err("unauthorized"));
    }

    config.owner = deps.api.addr_canonicalize(owner.as_str())?;

    store_config(deps.storage, &config)?;

    Ok(Response::new().add_attribute("action", "update_config"))
}

pub fn receive_cw20(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> StdResult<Response> {
    match from_binary(&cw20_msg.msg) {
        Ok(Cw20HookMsg::Convert {}) => {
            // check permission
            let token_raw = deps.api.addr_canonicalize(info.sender.as_str())?;
            let token_ratio = read_token_ratio(deps.storage, token_raw.as_slice())?;
            let amount = cw20_msg.amount * token_ratio.ratio;
            let message = Asset {
                info: token_ratio.info,
                amount: amount.clone(),
            }
            .into_msg(
                None,
                &deps.querier,
                deps.api.addr_validate(cw20_msg.sender.as_str())?,
            )?;

            Ok(Response::new().add_message(message).add_attributes(vec![
                ("action", "convert_token"),
                ("from_amount", &cw20_msg.amount.to_string()),
                ("to_amount", &amount.to_string()),
            ]))
        }
        Ok(Cw20HookMsg::ConvertReverse { from }) => {
            let asset_key = from.to_vec(deps.api)?;
            let token_ratio = read_token_ratio(deps.storage, &asset_key)?;

            if let AssetInfo::Token { contract_addr } = token_ratio.info {
                if contract_addr != info.sender {
                    return Err(StdError::generic_err("invalid cw20 hook message"));
                }

                let amount = cw20_msg.amount.checked_div_decimal(token_ratio.ratio)?;

                let message = Asset {
                    info: from,
                    amount: amount.clone(),
                }
                .into_msg(
                    None,
                    &deps.querier,
                    deps.api.addr_validate(cw20_msg.sender.as_str())?,
                )?;

                Ok(Response::new().add_message(message).add_attributes(vec![
                    ("action", "convert_token_reverse"),
                    ("from_amount", &cw20_msg.amount.to_string()),
                    ("to_amount", &amount.to_string()),
                ]))
            } else {
                return Err(StdError::generic_err("invalid cw20 hook message"));
            }
        }
        Err(_) => Err(StdError::generic_err("invalid cw20 hook message")),
    }
}

pub fn update_pair(
    deps: DepsMut,
    info: MessageInfo,
    from: TokenInfo,
    to: TokenInfo,
) -> StdResult<Response> {
    let config: Config = read_config(deps.storage)?;
    if config.owner != deps.api.addr_canonicalize(info.sender.as_str())? {
        return Err(StdError::generic_err("unauthorized"));
    }

    let asset_key = from.info.to_vec(deps.api)?;

    let token_ratio = TokenRatio {
        info: to.info,
        ratio: Decimal::from_ratio(
            10u128.pow(to.decimals.into()),
            10u128.pow(from.decimals.into()),
        ),
    };

    store_token_ratio(deps.storage, &asset_key, &token_ratio)?;

    Ok(Response::new().add_attribute("action", "update_pair"))
}

pub fn unregister_pair(deps: DepsMut, info: MessageInfo, from: TokenInfo) -> StdResult<Response> {
    let config: Config = read_config(deps.storage)?;
    if config.owner != deps.api.addr_canonicalize(info.sender.as_str())? {
        return Err(StdError::generic_err("unauthorized"));
    }

    let asset_key = from.info.to_vec(deps.api)?;

    token_ratio_remove(deps.storage, &asset_key);

    Ok(Response::new().add_attribute("action", "unregister_convert_info"))
}

pub fn convert(deps: DepsMut, _env: Env, info: MessageInfo) -> StdResult<Response> {
    let mut messages: Vec<CosmosMsg> = vec![];
    let mut attributes: Vec<Attribute> = vec![];
    attributes.push(("action", "convert_token").into());

    for native_coin in info.funds {
        let asset_key = native_coin.denom.as_bytes();
        let amount = native_coin.amount;
        attributes.push(("denom", native_coin.denom.clone()).into());
        attributes.push(("from_amount", amount.to_string()).into());
        let token_ratio = read_token_ratio(deps.storage, asset_key)?;
        let to_amount = amount * token_ratio.ratio;

        attributes.push(("to_amount", to_amount).into());

        let message = Asset {
            info: token_ratio.info,
            amount: to_amount.clone(),
        }
        .into_msg(None, &deps.querier, info.sender.clone())?;

        messages.push(message);
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attributes(attributes))
}

pub fn convert_reverse(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    from_asset: AssetInfo,
) -> StdResult<Response> {
    let asset_key = from_asset.to_vec(deps.api)?;
    let token_ratio = read_token_ratio(deps.storage, &asset_key)?;

    if let AssetInfo::NativeToken { denom } = token_ratio.info {
        //check funds includes To token
        if let Some(native_coin) = info.funds.iter().find(|a| a.denom.eq(&denom)) {
            let amount = native_coin.amount.checked_div_decimal(token_ratio.ratio)?;
            let message = Asset {
                info: from_asset,
                amount: amount.clone(),
            }
            .into_msg(None, &deps.querier, info.sender.clone())?;

            return Ok(Response::new().add_message(message).add_attributes(vec![
                ("action", "convert_token_reverse"),
                ("denom", native_coin.denom.as_str()),
                ("from_amount", &native_coin.amount.to_string()),
                ("to_amount", &amount.to_string()),
            ]));
        } else {
            return Err(StdError::generic_err("Cannot find the native token that matches the input to convert in convert_reverse()"));
        };
    } else {
        return Err(StdError::generic_err("invalid cw20 hook message"));
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::ConvertInfo { asset_info } => to_binary(&query_convert_info(deps, asset_info)?),
    }
}

pub fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let state = read_config(deps.storage)?;
    let resp = ConfigResponse {
        owner: deps.api.addr_humanize(&state.owner)?,
    };

    Ok(resp)
}

pub fn query_convert_info(deps: Deps, asset_info: AssetInfo) -> StdResult<ConvertInfoResponse> {
    let asset_key = asset_info.to_vec(deps.api)?;
    let token_ratio = read_token_ratio(deps.storage, &asset_key)?;
    Ok(ConvertInfoResponse { token_ratio })
}

pub fn withdraw_tokens(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    asset_infos: Vec<AssetInfo>,
) -> StdResult<Response> {
    let config = read_config(deps.storage)?;
    let owner = deps.api.addr_humanize(&config.owner)?;
    if owner != info.sender {
        return Err(StdError::generic_err("unauthorized"));
    }
    let mut messages: Vec<CosmosMsg> = vec![];
    let mut attributes: Vec<Attribute> = vec![("action", "withdraw_tokens").into()];

    for asset in asset_infos {
        let balance = asset.query_pool(&deps.querier, env.contract.address.clone())?;
        let message = Asset {
            info: asset,
            amount: balance.clone(),
        }
        .into_msg(None, &deps.querier, owner.clone())?;
        messages.push(message);
        attributes.push(("amount", balance.to_string()).into())
    }

    Ok(Response::new()
        .add_messages(messages)
        .add_attributes(attributes))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: MigrateMsg) -> StdResult<Response> {
    Ok(Response::default())
}
